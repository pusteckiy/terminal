//! Stateless signed-GET relay. Private payloads never enter AppState or broadcasts.
use crate::AppState;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use terminal_core::{Exchange, private::SignedRead};

fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| reqwest::Url::parse(s).ok())
    else {
        return false;
    };
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else {
        return false;
    };
    matches!(origin.scheme(), "http" | "https")
        && origin.host_str().is_some_and(|name| {
            let authority = match origin.port() {
                Some(port) => format!("{name}:{port}"),
                None => name.to_owned(),
            };
            authority == host
        })
}

fn valid(request: &SignedRead) -> bool {
    if request.query.len() > 8192
        || request
            .query
            .bytes()
            .any(|b| b < 32 || b == b'#' || b > 126)
        || request.headers.len() > 5
    {
        return false;
    }
    let allowed: &[&str] = match request.route.exchange() {
        Exchange::Binance => &["x-mbx-apikey"],
        Exchange::Bybit => &[
            "x-bapi-api-key",
            "x-bapi-sign",
            "x-bapi-timestamp",
            "x-bapi-recv-window",
        ],
        Exchange::Okx => &[
            "ok-access-key",
            "ok-access-sign",
            "ok-access-timestamp",
            "ok-access-passphrase",
        ],
        _ => return false,
    };
    let mut names = std::collections::HashSet::new();
    request.headers.iter().all(|(name, value)| {
        allowed.contains(&name.as_str())
            && names.insert(name)
            && !value.is_empty()
            && value.len() <= 2048
            && value.bytes().all(|b| (32..127).contains(&b))
    })
}
fn reply(status: StatusCode, body: impl IntoResponse) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        body,
    )
        .into_response()
}
pub async fn read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SignedRead>,
) -> Response {
    if !same_origin(&headers) {
        return reply(StatusCode::FORBIDDEN, "Same-origin requests only");
    }
    if !valid(&request) {
        return reply(StatusCode::BAD_REQUEST, "Invalid signed read request");
    }
    static REQUESTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    let Ok(_permit) = REQUESTS
        .get_or_init(|| tokio::sync::Semaphore::new(8))
        .try_acquire()
    else {
        return reply(StatusCode::TOO_MANY_REQUESTS, "Private read limit reached");
    };
    let url = format!(
        "{}{}{}{}",
        request.route.origin(),
        request.route.path(),
        if request.query.is_empty() { "" } else { "?" },
        request.query
    );
    let mut outgoing = state.http.get(url);
    for (name, value) in &request.headers {
        outgoing = outgoing.header(name, value);
    }
    let Ok(Ok(mut upstream)) = tokio::time::timeout(Duration::from_secs(20), outgoing.send()).await
    else {
        return reply(StatusCode::BAD_GATEWAY, "Exchange request failed");
    };
    let status = upstream.status();
    let mut bytes = Vec::new();
    loop {
        match upstream.chunk().await {
            Ok(Some(chunk)) => {
                if bytes.len() + chunk.len() > 8 * 1024 * 1024 {
                    return reply(StatusCode::BAD_GATEWAY, "Exchange response too large");
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return reply(StatusCode::BAD_GATEWAY, "Exchange response interrupted"),
        }
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return reply(StatusCode::BAD_GATEWAY, "Invalid exchange response");
    };
    reply(status, Json(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_core::private::ReadRoute;
    #[test]
    fn private_socket_relay_allows_only_auth_and_read_topics() {
        assert!(valid_frame(
            SocketFeed::Bybit,
            r#"{"op":"auth","args":["key",1234567890,"signature"]}"#
        ));
        assert!(valid_frame(
            SocketFeed::Bybit,
            r#"{"op":"subscribe","args":["order.spot","execution.linear","wallet"]}"#
        ));
        assert!(!valid_frame(
            SocketFeed::Bybit,
            r#"{"op":"order.create","args":[]}"#
        ));
        assert!(!valid_frame(
            SocketFeed::Bybit,
            r#"{"op":"subscribe","args":["order.spot"],"secret":"forbidden"}"#
        ));
        assert!(valid_frame(
            SocketFeed::Okx,
            r#"{"op":"login","args":[{"apiKey":"key","passphrase":"api-passphrase","timestamp":"1","sign":"signature"}]}"#
        ));
        assert!(!valid_frame(SocketFeed::Okx, r#"{"op":"order","args":[]}"#));
        assert!(!valid_frame(
            SocketFeed::Okx,
            r#"{"op":"subscribe","args":[{"channel":"orders","url":"http://localhost"}]}"#
        ));
        assert!(valid_frame(
            SocketFeed::BinanceSpot,
            r#"{"id":1,"method":"userDataStream.subscribe.signature","params":{"apiKey":"key","timestamp":1,"signature":"signed"}}"#
        ));
        assert!(!valid_frame(
            SocketFeed::BinanceSpot,
            r#"{"method":"order.place","params":{}}"#
        ));
        assert!(!valid_frame(SocketFeed::BinancePerp, "ping"));
    }
    #[test]
    fn relay_rejects_cross_origin_and_arbitrary_credentials_or_routes() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "localhost:3000".parse().unwrap());
        headers.insert("origin", "https://evil.example".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.insert("origin", "http://localhost:3000".parse().unwrap());
        assert!(same_origin(&headers));
        let mut request = SignedRead {
            route: ReadRoute::BybitKey,
            query: String::new(),
            headers: vec![("x-bapi-sign".into(), "signed".into())],
        };
        assert!(valid(&request));
        request
            .headers
            .push(("api-secret".into(), "forbidden".into()));
        assert!(!valid(&request));
        assert!(
            serde_json::from_str::<SignedRead>(r#"{"route":"withdraw","query":"","headers":[]}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<SignedRead>(
                r#"{"route":"bybit_key","query":"","headers":[],"secret":"forbidden"}"#
            )
            .is_err()
        );
        assert_eq!(ReadRoute::OkxOrders.path(), "/api/v5/trade/orders-pending");
    }
}

use axum::extract::{
    Query,
    ws::{Message as ClientFrame, WebSocket, WebSocketUpgrade},
};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use terminal_core::private::SocketFeed;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message as ExchangeFrame, protocol::WebSocketConfig},
};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SocketQuery {
    feed: SocketFeed,
}
fn valid_frame(feed: SocketFeed, text: &str) -> bool {
    if text.len() > 16 * 1024 {
        return false;
    }
    if feed == SocketFeed::Okx && text == "ping" {
        return true;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    let only = |value: &serde_json::Value, keys: &[&str]| {
        value
            .as_object()
            .is_some_and(|o| o.keys().all(|key| keys.contains(&key.as_str())))
    };
    match feed {
        SocketFeed::BinanceSpot | SocketFeed::BinanceApi => {
            if !only(&value, &["id", "method", "params"]) {
                return false;
            }
            let allowed: &[&str] = if feed == SocketFeed::BinanceSpot {
                &[
                    "userDataStream.subscribe.signature",
                    "userDataStream.unsubscribe",
                    "ping",
                ]
            } else {
                &[
                    "userDataStream.start",
                    "userDataStream.ping",
                    "userDataStream.stop",
                ]
            };
            value["method"]
                .as_str()
                .is_some_and(|m| allowed.contains(&m))
                && value.get("params").is_none_or(|p| {
                    only(
                        p,
                        &[
                            "apiKey",
                            "signature",
                            "timestamp",
                            "recvWindow",
                            "subscriptionId",
                        ],
                    )
                })
        }
        SocketFeed::Bybit => {
            if !only(&value, &["op", "args", "req_id"]) {
                return false;
            }
            match value["op"].as_str() {
                Some("auth") => value["args"].as_array().is_some_and(|a| {
                    a.len() == 3 && a[0].is_string() && a[1].is_i64() && a[2].is_string()
                }),
                Some("ping") => true,
                Some("subscribe" | "unsubscribe") => value["args"].as_array().is_some_and(|a| {
                    a.len() <= 8
                        && a.iter().all(|t| {
                            matches!(
                                t.as_str(),
                                Some(
                                    "order.spot"
                                        | "order.linear"
                                        | "execution.spot"
                                        | "execution.linear"
                                        | "position.linear"
                                        | "wallet"
                                )
                            )
                        })
                }),
                _ => false,
            }
        }
        SocketFeed::Okx => {
            if !only(&value, &["id", "op", "args"]) {
                return false;
            }
            match value["op"].as_str() {
                Some("login") => value["args"].as_array().is_some_and(|a| {
                    a.len() == 1
                        && only(&a[0], &["apiKey", "passphrase", "timestamp", "sign"])
                        && ["apiKey", "passphrase", "timestamp", "sign"]
                            .iter()
                            .all(|key| a[0][*key].is_string())
                }),
                Some("subscribe" | "unsubscribe") => value["args"].as_array().is_some_and(|a| {
                    a.len() <= 8
                        && a.iter().all(|t| {
                            only(t, &["channel", "instType"])
                                && matches!(
                                    t["channel"].as_str(),
                                    Some("orders" | "positions" | "account")
                                )
                                && t.get("instType").is_none_or(|kind| {
                                    matches!(kind.as_str(), Some("SPOT" | "SWAP"))
                                })
                        })
                }),
                _ => false,
            }
        }
        SocketFeed::BinancePerp => false, // Incoming data only; keepalive uses BinanceApi.
    }
}
pub async fn ws(
    headers: HeaderMap,
    Query(query): Query<SocketQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !same_origin(&headers) {
        return reply(StatusCode::FORBIDDEN, "Same-origin requests only");
    }
    static CONNECTIONS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    let Ok(permit) = CONNECTIONS
        .get_or_init(|| tokio::sync::Semaphore::new(128))
        .try_acquire()
    else {
        return reply(
            StatusCode::TOO_MANY_REQUESTS,
            "Private connection limit reached",
        );
    };
    upgrade
        .max_message_size(16 * 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            relay(socket, query.feed).await;
        })
        .into_response()
}
async fn relay(mut client: WebSocket, feed: SocketFeed) {
    let mut url = feed.url().to_owned();
    if feed == SocketFeed::BinancePerp {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Listen {
            listen_key: String,
        }
        let Ok(Some(Ok(ClientFrame::Text(text)))) =
            tokio::time::timeout(Duration::from_secs(10), client.recv()).await
        else {
            return;
        };
        let Ok(listen) = serde_json::from_str::<Listen>(&text) else {
            return;
        };
        if listen.listen_key.is_empty()
            || listen.listen_key.len() > 512
            || !listen.listen_key.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return;
        }
        url.push_str(&listen.listen_key);
    }
    // One conservative connection budget per venue, shared by every browser.
    static GATES: std::sync::OnceLock<[tokio::sync::Mutex<tokio::time::Instant>; 3]> =
        std::sync::OnceLock::new();
    let index = match feed {
        SocketFeed::Bybit => 1,
        SocketFeed::Okx => 2,
        _ => 0,
    };
    {
        let mut gate = GATES.get_or_init(|| {
            std::array::from_fn(|_| tokio::sync::Mutex::new(tokio::time::Instant::now()))
        })[index]
            .lock()
            .await;
        tokio::time::sleep_until(*gate).await;
        *gate = tokio::time::Instant::now() + Duration::from_millis(400);
    }
    let Ok(Ok((mut exchange, _))) = tokio::time::timeout(
        Duration::from_secs(10),
        connect_async_with_config(
            url,
            Some(
                WebSocketConfig::default()
                    .max_message_size(Some(8 * 1024 * 1024))
                    .max_frame_size(Some(8 * 1024 * 1024)),
            ),
            true,
        ),
    )
    .await
    else {
        return;
    };
    if client
        .send(ClientFrame::Text(r#"{"relay":"ready"}"#.into()))
        .await
        .is_err()
    {
        return;
    }
    let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await;
    let mut last_reply = tokio::time::Instant::now();
    loop {
        // Backpressure is bounded by socket buffers; stalled clients are disconnected.
        let step = async {
            tokio::select! {
                event = client.recv() => match event {
                    Some(Ok(ClientFrame::Text(text))) if valid_frame(feed, &text) => exchange.send(ExchangeFrame::Text(text.to_string().into())).await.is_ok(),
                    Some(Ok(ClientFrame::Ping(bytes))) => client.send(ClientFrame::Pong(bytes)).await.is_ok(),
                    _ => false,
                },
                _ = heartbeat.tick() => {
                    let application_ping = match feed {
                        SocketFeed::Bybit => Some(r#"{"op":"ping"}"#),
                        SocketFeed::Okx => Some("ping"),
                        SocketFeed::BinanceSpot => Some(r#"{"id":"relay-heartbeat","method":"ping"}"#),
                        _ => None,
                    };
                    let sent = if let Some(ping) = application_ping { exchange.send(ExchangeFrame::Text(ping.into())).await.is_ok() } else { true };
                    sent && last_reply.elapsed() < Duration::from_secs(60) && exchange.send(ExchangeFrame::Ping(Vec::new().into())).await.is_ok()
                },
                event = exchange.next() => match event {
                    Some(Ok(ExchangeFrame::Text(text))) if text.len() <= 8 * 1024 * 1024 => { last_reply = tokio::time::Instant::now(); client.send(ClientFrame::Text(text.to_string().into())).await.is_ok() },
                    Some(Ok(ExchangeFrame::Ping(bytes))) => exchange.send(ExchangeFrame::Pong(bytes)).await.is_ok(),
                    Some(Ok(ExchangeFrame::Pong(_))) => { last_reply = tokio::time::Instant::now(); client.send(ClientFrame::Text(r#"{"relay":"heartbeat"}"#.into())).await.is_ok() },
                    _ => false,
                },
            }
        };
        if !tokio::time::timeout(Duration::from_secs(60), step)
            .await
            .unwrap_or(false)
        {
            break;
        }
    }
}
