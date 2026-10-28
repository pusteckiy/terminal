use super::{
    Output,
    parse::{Store, text, time},
};
use crate::{
    socket,
    vault::{self, Credential},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use eframe::egui;
use ewebsock::{WsEvent, WsMessage, WsSender};
use futures_util::{
    future::{AbortHandle, Abortable, Either, poll_fn, select, select_all},
    task::AtomicWaker,
};
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc, sync::Arc, task::Poll};
use terminal_core::{
    Exchange, Market, MarketKind, OwnFill,
    private::{ReadRoute, SignedRead, SocketFeed},
};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, spawn_local};

type Result<T> = std::result::Result<T, String>;
fn wall_ms() -> i64 {
    js_sys::Date::now() as i64
}
fn crypto_error(_: JsValue) -> String {
    "Browser cryptography unavailable. Use HTTPS or localhost.".into()
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn encode(value: &str) -> String {
    js_sys::encode_uri_component(value)
        .as_string()
        .unwrap_or_default()
}
fn query(values: &[(&str, String)]) -> String {
    values
        .iter()
        .map(|(key, value)| format!("{}={}", encode(key), encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

pub async fn sleep(ms: u32) {
    let (send, receive) = futures_channel::oneshot::channel();
    let closure = Closure::once(move || {
        let _ = send.send(());
    });
    struct Timer {
        id: i32,
        _closure: Closure<dyn FnMut()>,
    }
    impl Drop for Timer {
        fn drop(&mut self) {
            if let Some(window) = web_sys::window() {
                window.clear_timeout_with_handle(self.id);
            }
        }
    }
    if let Some(window) = web_sys::window() {
        if let Ok(id) = window.set_timeout_with_callback_and_timeout_and_arguments_0(
            closure.as_ref().unchecked_ref(),
            ms.min(i32::MAX as u32) as i32,
        ) {
            let _timer = Timer {
                id,
                _closure: closure,
            };
            let _ = receive.await;
        }
    }
}
async fn read(
    credential: &Credential,
    route: ReadRoute,
    values: Vec<(&str, String)>,
) -> Result<Value> {
    let mut query = query(&values);
    let now = wall_ms().to_string();
    let headers = match credential.account.exchange {
        Exchange::Binance => {
            if !query.is_empty() {
                query.push('&');
            }
            query.push_str(&format!("recvWindow=60000&timestamp={now}"));
            let signature = hex(&vault::hmac(&credential.secret, &query)
                .await
                .map_err(crypto_error)?);
            query.push_str(&format!("&signature={signature}"));
            vec![("x-mbx-apikey".into(), credential.api_key.clone())]
        }
        Exchange::Bybit => {
            let signed = format!("{now}{}60000{query}", credential.api_key);
            let signature = hex(&vault::hmac(&credential.secret, &signed)
                .await
                .map_err(crypto_error)?);
            vec![
                ("x-bapi-api-key".into(), credential.api_key.clone()),
                ("x-bapi-sign".into(), signature),
                ("x-bapi-timestamp".into(), now),
                ("x-bapi-recv-window".into(), "60000".into()),
            ]
        }
        Exchange::Okx => {
            let timestamp = js_sys::Date::new_0()
                .to_iso_string()
                .as_string()
                .ok_or("Invalid browser clock")?;
            let path = format!(
                "{}{}{}",
                route.path(),
                if query.is_empty() { "" } else { "?" },
                query
            );
            let signature = STANDARD.encode(
                vault::hmac(&credential.secret, &format!("{timestamp}GET{path}"))
                    .await
                    .map_err(crypto_error)?,
            );
            vec![
                ("ok-access-key".into(), credential.api_key.clone()),
                ("ok-access-sign".into(), signature),
                ("ok-access-timestamp".into(), timestamp),
                ("ok-access-passphrase".into(), credential.passphrase.clone()),
            ]
        }
        _ => return Err("Unsupported private exchange".into()),
    };
    let signed = SignedRead {
        route,
        query,
        headers,
    };
    let body = serde_json::to_string(&signed).map_err(|_| "Cannot encode signed read")?;
    let options = web_sys::RequestInit::new();
    options.set_method("POST");
    options.set_mode(web_sys::RequestMode::SameOrigin);
    options.set_credentials(web_sys::RequestCredentials::SameOrigin);
    options.set_body(&JsValue::from_str(&body));
    let request = web_sys::Request::new_with_str_and_init("/api/private/read", &options)
        .map_err(|_| "Cannot create signed read")?;
    request
        .headers()
        .set("content-type", "application/json")
        .map_err(|_| "Cannot create signed read")?;
    let window = web_sys::window().ok_or("Browser window unavailable")?;
    let response: web_sys::Response = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|_| "Signed read relay unavailable")?
        .dyn_into()
        .map_err(|_| "Invalid relay response")?;
    let status = response.status();
    let text = JsFuture::from(response.text().map_err(|_| "Invalid relay response")?)
        .await
        .map_err(|_| "Cannot read exchange response")?
        .as_string()
        .ok_or("Invalid exchange response")?;
    if matches!(status, 418 | 429) {
        return Err("Exchange rate limit reached; retrying with backoff".into());
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| "Relay unavailable; restart the server after updating")?;
    let code = match credential.account.exchange {
        Exchange::Bybit => value.get("retCode").and_then(Value::as_i64).unwrap_or(-1),
        Exchange::Okx => value
            .get("code")
            .and_then(Value::as_str)
            .and_then(|s| s.parse().ok())
            .unwrap_or(-1),
        _ => value.get("code").and_then(Value::as_i64).unwrap_or(0),
    };
    if !(200..300).contains(&status) || code != 0 {
        return Err(format!(
            "Exchange rejected signed read ({status}/{code}). Check read permissions, IP whitelist and clock."
        ));
    }
    Ok(value)
}
async fn pages(
    credential: &Credential,
    route: ReadRoute,
    values: Vec<(&str, String)>,
    cap: usize,
) -> Result<Vec<Value>> {
    let mut rows = Vec::new();
    let mut cursor = String::new();
    for _ in 0..200 {
        let mut params = values.clone();
        if !cursor.is_empty() {
            params.push((
                if credential.account.exchange == Exchange::Bybit {
                    "cursor"
                } else {
                    "after"
                },
                cursor.clone(),
            ));
        }
        let value = read(credential, route, params).await?;
        let list = if credential.account.exchange == Exchange::Bybit {
            value["result"]["list"].as_array()
        } else {
            value["data"].as_array()
        }
        .ok_or("Invalid exchange page")?;
        let next = if credential.account.exchange == Exchange::Bybit {
            text(&value["result"], &["nextPageCursor"])
        } else if list.len() == 100 {
            list.last()
                .map(|row| text(row, &["billId", "ordId"]))
                .unwrap_or_default()
        } else {
            String::new()
        };
        rows.extend(list.iter().cloned());
        if rows.len() >= cap {
            rows.truncate(cap);
            return Ok(rows);
        }
        if next.is_empty() {
            return Ok(rows);
        }
        if next == cursor {
            return Err("Exchange pagination did not advance".into());
        }
        cursor = next;
    }
    Err("Account exceeds the supported snapshot page limit".into())
}

struct Connection {
    sender: WsSender,
    inbox: socket::Inbox,
    notify: Arc<AtomicWaker>,
}
impl Connection {
    async fn open(feed: SocketFeed, listen_key: Option<&str>) -> Result<Self> {
        let public = crate::ws_url();
        let url = format!(
            "{}/api/private/ws?feed={}",
            public.strip_suffix("/ws").ok_or("Invalid relay URL")?,
            feed.name()
        );
        let notify = Arc::new(AtomicWaker::new());
        let wake = notify.clone();
        let (inbox, callback) = socket::channel(move || wake.wake());
        let sender =
            ewebsock::ws_connect(url.into(), ewebsock::Options::default(), Box::new(callback))
                .map_err(|_| "Cannot open private WebSocket")?;
        let connection = Self {
            sender,
            inbox,
            notify,
        };
        let opened = matches!(
            select(Box::pin(connection.next(false)), Box::pin(sleep(15_000))).await,
            Either::Left((Ok(WsEvent::Opened), _))
        );
        if opened {
            let mut connection = connection;
            if let Some(listen_key) = listen_key {
                connection.send(json!({"listen_key": listen_key}));
            }
            if connection.json().await?["relay"] != "ready" {
                return Err("Private relay handshake failed".into());
            }
            Ok(connection)
        } else {
            Err(
                "Private WebSocket could not connect. Check network and exchange availability."
                    .into(),
            )
        }
    }
    fn send(&mut self, value: Value) {
        self.sender.send(WsMessage::Text(value.to_string()));
    }
    async fn next(&self, check_age: bool) -> Result<WsEvent> {
        poll_fn(|cx| {
            self.notify.register(cx.waker());
            if self
                .inbox
                .needs_resync(if check_age { socket::now_ms() } else { 0 })
            {
                return Poll::Ready(Err("Private stream fell behind; resynchronizing".into()));
            }
            self.inbox
                .try_recv()
                .map_or(Poll::Pending, |event| Poll::Ready(Ok(event)))
        })
        .await
    }
    async fn json(&self) -> Result<Value> {
        let receive = async {
            loop {
                match self.next(false).await? {
                    WsEvent::Message(WsMessage::Text(text)) if text != "pong" => {
                        let value: Value = serde_json::from_str(&text)
                            .map_err(|_| "Invalid private WebSocket message")?;
                        if value["relay"] == "heartbeat"
                            || value["id"] == "relay-heartbeat"
                            || matches!(value["op"].as_str(), Some("ping" | "pong"))
                        {
                            continue;
                        }
                        return Ok(value);
                    }
                    WsEvent::Closed | WsEvent::Error(_) => {
                        return Err("Private WebSocket disconnected".into());
                    }
                    _ => {}
                }
            }
        };
        match select(Box::pin(receive), Box::pin(sleep(15_000))).await {
            Either::Left((result, _)) => result,
            _ => Err("Private WebSocket authentication timed out".into()),
        }
    }
}

type Feed = SocketFeed;
async fn connect(credential: &Credential) -> Result<Vec<(Feed, Connection)>> {
    let mut feeds = Vec::new();
    match credential.account.exchange {
        Exchange::Binance => {
            let permissions = read(credential, ReadRoute::BinanceKey, Vec::new()).await?;
            if !super::parse::read_only(Exchange::Binance, &permissions) {
                return Err("Use a Binance key with Enable Reading only; disable trading, Futures trading, transfers and withdrawals.".into());
            }
            if credential.spot {
                let mut stream = Connection::open(SocketFeed::BinanceSpot, None).await?;
                let timestamp = wall_ms();
                let signed = format!(
                    "apiKey={}&recvWindow=60000&timestamp={timestamp}",
                    credential.api_key
                );
                let signature = hex(&vault::hmac(&credential.secret, &signed)
                    .await
                    .map_err(crypto_error)?);
                stream.send(json!({"id":"subscribe","method":"userDataStream.subscribe.signature","params":{"apiKey":credential.api_key,"timestamp":timestamp,"recvWindow":60000,"signature":signature}}));
                if stream.json().await?["status"] != 200 {
                    return Err("Binance Spot authentication rejected".into());
                }
                feeds.push((Feed::BinanceSpot, stream));
            }
            if credential.perp {
                let mut api = Connection::open(SocketFeed::BinanceApi, None).await?;
                api.send(json!({"id":"start","method":"userDataStream.start","params":{"apiKey":credential.api_key}}));
                let response = api.json().await?;
                let listen_key = response["result"]["listenKey"]
                    .as_str()
                    .filter(|key| {
                        !key.is_empty()
                            && key.len() <= 512
                            && key.bytes().all(|c| c.is_ascii_alphanumeric())
                    })
                    .ok_or("Binance Futures stream authentication rejected")?;
                let stream = Connection::open(SocketFeed::BinancePerp, Some(listen_key)).await?;
                feeds.push((Feed::BinanceApi, api));
                feeds.push((Feed::BinancePerp, stream));
            }
        }
        Exchange::Bybit => {
            let permissions = read(credential, ReadRoute::BybitKey, Vec::new()).await?;
            if !super::parse::read_only(Exchange::Bybit, &permissions) {
                return Err("Use a Bybit Read-Only API key".into());
            }
            let mut stream = Connection::open(SocketFeed::Bybit, None).await?;
            let expires = wall_ms() + 10_000;
            let signature = hex(&vault::hmac(
                &credential.secret,
                &format!("GET/realtime{expires}"),
            )
            .await
            .map_err(crypto_error)?);
            stream.send(json!({"op":"auth","args":[credential.api_key, expires, signature]}));
            if stream.json().await?["success"] != true {
                return Err("Bybit authentication rejected".into());
            }
            let mut topics = Vec::new();
            if credential.spot {
                topics.extend(["order.spot", "execution.spot"]);
            }
            if credential.perp {
                topics.extend(["order.linear", "execution.linear", "position.linear"]);
            }
            topics.push("wallet");
            stream.send(json!({"op":"subscribe","args":topics}));
            feeds.push((Feed::Bybit, stream));
        }
        Exchange::Okx => {
            let config = read(credential, ReadRoute::OkxConfig, Vec::new()).await?;
            if !super::parse::read_only(Exchange::Okx, &config) {
                return Err("Use an OKX API key with Read permission only".into());
            }
            let mut stream = Connection::open(SocketFeed::Okx, None).await?;
            let timestamp = (wall_ms() / 1000).to_string();
            let signature = STANDARD.encode(
                vault::hmac(
                    &credential.secret,
                    &format!("{timestamp}GET/users/self/verify"),
                )
                .await
                .map_err(crypto_error)?,
            );
            stream.send(json!({"op":"login","args":[{"apiKey":credential.api_key,"passphrase":credential.passphrase,"timestamp":timestamp,"sign":signature}]}));
            if stream.json().await?["code"] != "0" {
                return Err("OKX authentication rejected".into());
            }
            let mut topics = vec![json!({"channel":"account"})];
            if credential.spot {
                topics.push(json!({"channel":"orders","instType":"SPOT"}));
            }
            if credential.perp {
                topics.extend([
                    json!({"channel":"orders","instType":"SWAP"}),
                    json!({"channel":"positions","instType":"SWAP"}),
                ]);
            }
            stream.send(json!({"op":"subscribe","args":topics}));
            feeds.push((Feed::Okx, stream));
        }
        _ => return Err("Unsupported private exchange".into()),
    }
    Ok(feeds)
}
struct HistoryRequest {
    route: ReadRoute,
    params: Vec<(&'static str, String)>,
    kind: MarketKind,
}

async fn snapshot(
    credential: &Credential,
    store: &mut Store,
    markets: &[Market],
) -> Result<Vec<HistoryRequest>> {
    use ReadRoute::*;
    let exchange = credential.account.exchange;
    let now = wall_ms();
    let mut history = Vec::new();
    match exchange {
        Exchange::Binance => {
            if credential.spot {
                let balances = read(credential, BinanceSpotAccount, Vec::new()).await?;
                for row in balances["balances"]
                    .as_array()
                    .ok_or("Invalid Binance Spot balances")?
                {
                    store.wallet(exchange, row, now)?;
                }
                let orders = read(credential, BinanceSpotOrders, Vec::new()).await?;
                for row in orders.as_array().ok_or("Invalid Binance Spot orders")? {
                    store.order(exchange, MarketKind::Spot, row, now)?;
                }
                store.state.spot_updated_at_ms = now;
            }
            if credential.perp {
                let account = read(credential, BinancePerpAccount, Vec::new()).await?;
                for row in account.as_array().ok_or("Invalid Binance positions")? {
                    store.position(exchange, row, now)?;
                }
                let orders = read(credential, BinancePerpOrders, Vec::new()).await?;
                for row in orders.as_array().ok_or("Invalid Binance Futures orders")? {
                    store.order(exchange, MarketKind::Perp, row, now)?;
                }
                store.state.positions_updated_at_ms = now;
            }
            store.refresh();
            // shortcut: Binance backfill covers 32 visible/open-order symbols; queue on-demand history if larger workspaces need it.
            let mut selected: std::collections::HashSet<_> = markets
                .iter()
                .filter(|m| {
                    m.exchange == exchange
                        && if m.kind == MarketKind::Spot {
                            credential.spot
                        } else {
                            credential.perp
                        }
                })
                .cloned()
                .collect();
            selected.extend(store.state.orders.iter().filter_map(|o| {
                Some(Market {
                    exchange,
                    kind: o.kind?,
                    symbol: o.coin.clone(),
                })
            }));
            let mut selected: Vec<_> = selected.into_iter().collect();
            selected.sort_by(|a, b| {
                a.kind
                    .label()
                    .cmp(b.kind.label())
                    .then(a.symbol.cmp(&b.symbol))
            });
            for market in selected.into_iter().take(32) {
                history.push(HistoryRequest {
                    route: if market.kind == MarketKind::Spot {
                        BinanceSpotFills
                    } else {
                        BinancePerpFills
                    },
                    params: vec![("symbol", market.symbol), ("limit", "100".into())],
                    kind: market.kind,
                });
            }
        }

        Exchange::Bybit => {
            for kind in MarketKind::ALL {
                if kind == MarketKind::Spot && !credential.spot
                    || kind == MarketKind::Perp && !credential.perp
                {
                    continue;
                }
                let category = if kind == MarketKind::Spot {
                    "spot"
                } else {
                    "linear"
                };
                for settle in if kind == MarketKind::Spot {
                    vec![""]
                } else {
                    vec!["USDT", "USDC"]
                } {
                    let mut params = vec![
                        ("category", category.into()),
                        ("limit", "50".into()),
                        ("openOnly", "0".into()),
                    ];
                    if !settle.is_empty() {
                        params.push(("settleCoin", settle.into()));
                    }
                    for row in pages(credential, BybitOrders, params, usize::MAX).await? {
                        store.order(exchange, kind, &row, now)?;
                    }
                    if kind == MarketKind::Perp {
                        for row in pages(
                            credential,
                            BybitPositions,
                            vec![
                                ("category", "linear".into()),
                                ("settleCoin", settle.into()),
                                ("limit", "200".into()),
                            ],
                            usize::MAX,
                        )
                        .await?
                        {
                            store.position(exchange, &row, now)?;
                        }
                    }
                }
                history.push(HistoryRequest {
                    route: BybitFills,
                    params: vec![("category", category.into()), ("limit", "100".into())],
                    kind,
                });
            }

            let wallet = read(
                credential,
                BybitWallet,
                vec![("accountType", "UNIFIED".into())],
            )
            .await?;
            for account in wallet["result"]["list"]
                .as_array()
                .ok_or("Invalid Bybit wallet")?
            {
                for row in account["coin"].as_array().ok_or("Invalid Bybit coins")? {
                    store.wallet(exchange, row, now)?;
                }
            }
            if credential.spot {
                store.state.spot_updated_at_ms = now;
            }
            if credential.perp {
                store.state.positions_updated_at_ms = now;
            }
        }
        Exchange::Okx => {
            if credential.perp {
                let instruments = read(
                    credential,
                    OkxInstruments,
                    vec![("instType", "SWAP".into())],
                )
                .await?;
                for row in instruments["data"]
                    .as_array()
                    .ok_or("Invalid OKX contract metadata")?
                {
                    if row["ctType"] == "linear" {
                        let multiplier = text(row, &["ctVal"])
                            .parse()
                            .map_err(|_| "Invalid OKX contract multiplier")?;
                        store.multipliers.insert(text(row, &["instId"]), multiplier);
                    }
                }
                let positions =
                    read(credential, OkxPositions, vec![("instType", "SWAP".into())]).await?;
                for row in positions["data"]
                    .as_array()
                    .ok_or("Invalid OKX positions")?
                {
                    if store.multipliers.contains_key(&text(row, &["instId"])) {
                        store.position(exchange, row, now)?;
                    }
                }
                store.state.positions_updated_at_ms = now;
            }
            for kind in MarketKind::ALL {
                if kind == MarketKind::Spot && !credential.spot
                    || kind == MarketKind::Perp && !credential.perp
                {
                    continue;
                }
                let inst_type = if kind == MarketKind::Spot {
                    "SPOT"
                } else {
                    "SWAP"
                };
                for row in pages(
                    credential,
                    OkxOrders,
                    vec![("instType", inst_type.into()), ("limit", "100".into())],
                    usize::MAX,
                )
                .await?
                {
                    if kind == MarketKind::Spot
                        || store.multipliers.contains_key(&text(&row, &["instId"]))
                    {
                        store.order(exchange, kind, &row, now)?;
                    }
                }
                history.push(HistoryRequest {
                    route: OkxFills,
                    params: vec![("instType", inst_type.into()), ("limit", "100".into())],
                    kind,
                });
            }

            let wallet = read(credential, OkxWallet, Vec::new()).await?;
            for account in wallet["data"].as_array().ok_or("Invalid OKX wallet")? {
                for row in account["details"]
                    .as_array()
                    .ok_or("Invalid OKX balances")?
                {
                    store.wallet(exchange, row, now)?;
                }
            }
            if credential.spot {
                store.state.spot_updated_at_ms = now;
            }
        }
        _ => unreachable!(),
    }
    store
        .state
        .position_snapshot_times
        .insert(String::new(), store.state.positions_updated_at_ms);
    Ok(history)
}
fn apply(
    feed: Feed,
    value: &Value,
    store: &mut Store,
    credential: &Credential,
) -> Result<Vec<OwnFill>> {
    let mut fills = Vec::new();
    let now = wall_ms();
    match feed {
        Feed::BinanceSpot | Feed::BinancePerp => {
            let row = value.get("event").unwrap_or(value);
            match row["e"].as_str().unwrap_or("") {
                "executionReport" => {
                    store.order(Exchange::Binance, MarketKind::Spot, row, now)?;
                    if row["x"] == "TRADE" {
                        if let Some(fill) =
                            store.fill(Exchange::Binance, MarketKind::Spot, row, now)?
                        {
                            fills.push(fill);
                        }
                    }
                }
                "outboundAccountPosition" => {
                    for coin in row["B"].as_array().ok_or("Invalid Binance balance event")? {
                        store.wallet(Exchange::Binance, coin, time(row, &["u", "E"], now))?;
                    }
                }
                "ORDER_TRADE_UPDATE" => {
                    let order = &row["o"];
                    store.order(
                        Exchange::Binance,
                        MarketKind::Perp,
                        order,
                        time(row, &["T", "E"], now),
                    )?;
                    if order["x"] == "TRADE" {
                        if let Some(fill) =
                            store.fill(Exchange::Binance, MarketKind::Perp, order, now)?
                        {
                            fills.push(fill);
                        }
                    }
                }
                "ACCOUNT_UPDATE" => {
                    for position in row["a"]["P"]
                        .as_array()
                        .ok_or("Invalid Binance position event")?
                    {
                        store.position(Exchange::Binance, position, time(row, &["T", "E"], now))?;
                    }
                }
                "listenKeyExpired" | "eventStreamTerminated" => {
                    return Err("Binance private stream expired".into());
                }
                _ => {}
            }
        }
        Feed::BinanceApi => {
            if value.get("status").is_some_and(|s| *s != 200) {
                return Err("Binance stream keepalive rejected".into());
            }
        }
        Feed::Bybit => {
            if value["success"] == false {
                return Err("Bybit private subscription rejected".into());
            }
            if let Some(topic) = value["topic"].as_str() {
                let observed = time(value, &["creationTime"], now);
                for row in value["data"]
                    .as_array()
                    .ok_or("Invalid Bybit private event")?
                {
                    if topic == "wallet" {
                        for coin in row["coin"].as_array().ok_or("Invalid Bybit wallet event")? {
                            store.wallet(Exchange::Bybit, coin, observed)?;
                        }
                        continue;
                    }
                    let kind = match row["category"].as_str().or_else(|| topic.split('.').nth(1)) {
                        Some("spot") => MarketKind::Spot,
                        Some("linear") => MarketKind::Perp,
                        _ => continue,
                    };
                    if kind == MarketKind::Spot && !credential.spot
                        || kind == MarketKind::Perp && !credential.perp
                    {
                        continue;
                    }
                    if topic.starts_with("order") {
                        store.order(Exchange::Bybit, kind, row, observed)?;
                    }
                    if topic.starts_with("position") {
                        store.position(Exchange::Bybit, row, observed)?;
                    }
                    if topic.starts_with("execution") {
                        if let Some(fill) = store.fill(Exchange::Bybit, kind, row, observed)? {
                            fills.push(fill);
                        }
                    }
                }
            }
        }
        Feed::Okx => fills.extend(store.okx_event(value, now)?),
    }
    Ok(fills)
}
fn publish(
    output: &Rc<RefCell<Output>>,
    store: &mut Store,
    fills: Vec<OwnFill>,
    ctx: &egui::Context,
) {
    store.refresh();
    let mut output = output.borrow_mut();
    output.state = store.state.clone();
    terminal_core::merge_account_fills(&mut output.fills, fills);
    output.version += 1;
    ctx.request_repaint();
}
async fn connected(
    credential: &Credential,
    ctx: &egui::Context,
    output: &Rc<RefCell<Output>>,
    markets: &[Market],
) -> Result<()> {
    let mut feeds = connect(credential).await?;
    let mut store = Store::default();
    let history = snapshot(credential, &mut store, markets).await?;
    let mut fills = Vec::new();
    let mut acknowledgements = std::collections::HashSet::new();
    let mut replay_started = socket::now_ms();
    for (feed, stream) in &feeds {
        if stream.inbox.needs_resync(0) {
            return Err("Private snapshot overflowed; reconnecting".into());
        }
        while let Some(event) = stream.inbox.try_recv() {
            match event {
                WsEvent::Message(WsMessage::Text(text)) if text != "pong" => {
                    let value: Value =
                        serde_json::from_str(&text).map_err(|_| "Invalid private event")?;
                    if let Some(ack) =
                        super::parse::subscription_ack(credential.account.exchange, &value)
                    {
                        acknowledgements.insert(ack);
                    }
                    fills.extend(apply(*feed, &value, &mut store, credential)?);
                }
                WsEvent::Closed | WsEvent::Error(_) => {
                    return Err("Private stream disconnected while syncing".into());
                }
                _ => {}
            }
            if socket::now_ms().saturating_sub(replay_started) >= 4 {
                sleep(0).await;
                replay_started = socket::now_ms();
            }
        }
        if stream.inbox.needs_resync(0) {
            return Err("Private snapshot overflowed; reconnecting".into());
        }
    }
    let expected = match credential.account.exchange {
        Exchange::Bybit => 1,
        Exchange::Okx => 1 + usize::from(credential.spot) + 2 * usize::from(credential.perp),
        _ => 0,
    };
    let ack_started = socket::now_ms();
    while acknowledgements.len() < expected {
        if socket::now_ms().saturating_sub(ack_started) > 15_000 {
            return Err("Private subscription confirmation timed out".into());
        }
        let (feed, stream) = &feeds[0];
        let value = stream.json().await?;
        if let Some(ack) = super::parse::subscription_ack(credential.account.exchange, &value) {
            acknowledgements.insert(ack);
        }
        fills.extend(apply(*feed, &value, &mut store, credential)?);
        if socket::now_ms().saturating_sub(replay_started) >= 4 {
            sleep(0).await;
            replay_started = socket::now_ms();
        }
    }
    store.state.connected = true;
    store.state.updated_at_ms = wall_ms();
    output.borrow_mut().warning = None;
    publish(output, &mut store, fills, ctx);
    let _backfill = backfill(
        credential.clone(),
        history,
        store.multipliers.clone(),
        output.clone(),
        ctx.clone(),
    );
    let mut heartbeat = socket::now_ms() + 15_000;
    let mut renewal = socket::now_ms() + 25 * 60_000;
    let mut last_received = socket::now_ms();
    let mut since_yield = 0;
    let mut slice_started = socket::now_ms();
    let mut pending_fills = Vec::new();
    let mut dirty = false;
    let mut flush_at = socket::now_ms() + 16;
    loop {
        let waiting: Vec<_> = feeds
            .iter()
            .map(|(_, stream)| Box::pin(stream.next(true)))
            .collect();
        match select(
            Box::pin(select_all(waiting)),
            Box::pin(sleep(
                (if dirty {
                    heartbeat.min(flush_at)
                } else {
                    heartbeat
                })
                .saturating_sub(socket::now_ms())
                .max(1) as u32,
            )),
        )
        .await
        {
            Either::Left(((event, index, _), _)) => {
                let event = event?;
                last_received = socket::now_ms();
                match event {
                    WsEvent::Message(WsMessage::Text(text)) if text != "pong" => {
                        let value: Value = serde_json::from_str(&text)
                            .map_err(|_| "Invalid private stream message")?;
                        if value["relay"] != "heartbeat" {
                            let fills = apply(feeds[index].0, &value, &mut store, credential)?;
                            terminal_core::merge_account_fills(&mut pending_fills, fills);
                            dirty = true;
                        }
                    }
                    WsEvent::Closed | WsEvent::Error(_) => {
                        return Err("Private stream disconnected".into());
                    }
                    _ => {}
                }
            }
            Either::Right(_) => {}
        }
        if dirty && socket::now_ms() >= flush_at {
            store.state.updated_at_ms = wall_ms();
            publish(output, &mut store, std::mem::take(&mut pending_fills), ctx);
            dirty = false;
            flush_at = socket::now_ms() + 16;
        }
        if socket::now_ms() >= heartbeat {
            if socket::now_ms().saturating_sub(last_received) > 60_000 {
                return Err("Private stream heartbeat timed out".into());
            }
            for (feed, stream) in &mut feeds {
                match feed {
                    Feed::Bybit | Feed::Okx | Feed::BinanceSpot => {}
                    Feed::BinanceApi => {
                        if socket::now_ms() >= renewal {
                            stream.send(json!({"id":"renew","method":"userDataStream.ping","params":{"apiKey":credential.api_key}}));
                            renewal = socket::now_ms() + 25 * 60_000;
                        }
                    }
                    Feed::BinancePerp => {}
                }
            }
            heartbeat = socket::now_ms() + 15_000;
        }
        since_yield += 1;
        if since_yield >= 256 || socket::now_ms().saturating_sub(slice_started) >= 4 {
            since_yield = 0;
            sleep(0).await;
            slice_started = socket::now_ms();
        }
    }
}
fn backfill(
    credential: Credential,
    jobs: Vec<HistoryRequest>,
    multipliers: std::collections::HashMap<String, rust_decimal::Decimal>,
    output: Rc<RefCell<Output>>,
    ctx: egui::Context,
) -> Running {
    let target = output.clone();
    let (abort, registration) = AbortHandle::new_pair();
    spawn_local(async move {
        let _ = Abortable::new(
            async move {
                let mut parser = Store::default();
                parser.multipliers = multipliers;
                for job in jobs {
                    let result = async {
                        let rows = if credential.account.exchange == Exchange::Binance {
                            read(&credential, job.route, job.params)
                                .await?
                                .as_array()
                                .cloned()
                                .ok_or("Invalid Binance fill history")?
                        } else {
                            pages(&credential, job.route, job.params, 500).await?
                        };
                        let mut fills = Vec::new();
                        for row in rows {
                            if credential.account.exchange == Exchange::Okx
                                && job.kind == MarketKind::Perp
                                && !parser.multipliers.contains_key(&text(&row, &["instId"]))
                            {
                                continue;
                            }
                            if let Some(fill) = parser.fill(
                                credential.account.exchange,
                                job.kind,
                                &row,
                                wall_ms(),
                            )? {
                                fills.push(fill);
                            }
                        }
                        Ok::<_, String>(fills)
                    }
                    .await;
                    let mut output = target.borrow_mut();
                    match result {
                        Ok(fills) => {
                            terminal_core::merge_account_fills(&mut output.fills, fills);
                        }
                        Err(error) => {
                            output.warning =
                                Some(format!("Recent fill history incomplete: {error}"));
                            output.version += 1;
                            ctx.request_repaint();
                            break;
                        }
                    }
                    output.version += 1;
                    ctx.request_repaint();
                }
            },
            registration,
        )
        .await;
    });
    Running {
        abort,
        output,
        applied: 0,
    }
}
pub struct Running {
    abort: AbortHandle,
    pub output: Rc<RefCell<Output>>,
    pub applied: u64,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.abort.abort();
    }
}
pub fn start(credential: Credential, ctx: egui::Context, markets: Vec<Market>) -> Running {
    let output = Rc::new(RefCell::new(Output {
        version: 1,
        ..Default::default()
    }));
    let target = output.clone();
    let (abort, registration) = AbortHandle::new_pair();
    spawn_local(async move {
        let _ = Abortable::new(
            async move {
                let mut delay = 3_000;
                loop {
                    while web_sys::window()
                        .and_then(|w| w.document())
                        .is_some_and(|d| d.hidden())
                    {
                        sleep(1_000).await;
                    }
                    let started = socket::now_ms();
                    match connected(&credential, &ctx, &target, &markets).await {
                        Ok(()) => break,
                        Err(error) => {
                            let mut output = target.borrow_mut();
                            output.state.connected = false;
                            output.state.error = Some(error);
                            output.version += 1;
                            ctx.request_repaint();
                        }
                    }
                    if socket::now_ms().saturating_sub(started) >= 60_000 {
                        delay = 3_000;
                    }
                    let jitter = vault::random::<2>()
                        .map(|bytes| u16::from_le_bytes(bytes) as u32 % 1000)
                        .unwrap_or(500);
                    sleep(delay + jitter).await;
                    delay = (delay * 2).min(60_000);
                }
            },
            registration,
        )
        .await;
    });
    Running {
        abort,
        output,
        applied: 0,
    }
}
