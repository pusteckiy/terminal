use std::{
    collections::{HashMap, HashSet},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use terminal_core::{Account, AccountState, OwnOrder, Position, TradeSide};
use tokio::sync::watch;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::AppState;

const HYPERLIQUID_WS: &str = "wss://api.hyperliquid.xyz/ws";
const MAX_LIVE_ACCOUNTS: usize = 10; // Hyperliquid's user-specific WebSocket limit.

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn field(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

fn parse_orders(data: &Value) -> Option<Vec<OwnOrder>> {
    Some(
        data.get("orders")?
            .as_array()?
            .iter()
            .filter_map(|row| {
                if row["isTrigger"] == true || row["sz"].as_str()?.parse::<f64>().ok()? <= 0.0 {
                    return None;
                }
                let side = match row.get("side")?.as_str()? {
                    "B" => TradeSide::Buy,
                    "A" => TradeSide::Sell,
                    _ => return None,
                };
                Some(OwnOrder {
                    coin: field(row, "coin")?,
                    order_id: row.get("oid")?.as_u64()?,
                    side,
                    price: field(row, "limitPx")?,
                    size: field(row, "sz")?,
                })
            })
            .collect(),
    )
}

fn parse_positions(data: &Value) -> Option<Vec<Position>> {
    Some(
        data.get("clearinghouseState")?
            .get("assetPositions")?
            .as_array()?
            .iter()
            .filter_map(|row| {
                let position = row.get("position")?;
                let size = field(position, "szi")?;
                if size.parse::<f64>().ok()? == 0.0 {
                    return None;
                }
                Some(Position {
                    coin: field(position, "coin")?,
                    size,
                    entry_price: field(position, "entryPx").unwrap_or_default(),
                    unrealized_pnl: field(position, "unrealizedPnl").unwrap_or_default(),
                })
            })
            .collect(),
    )
}

fn subscription(kind: &str, account: &Account, method: &str) -> Message {
    Message::Text(
        json!({
            "method": method,
            "subscription": {"type": kind, "user": account.address}
        })
        .to_string()
        .into(),
    )
}

async fn clear(state: &AppState, account: &Account, error: Option<String>) {
    state
        .publish_account(
            account,
            AccountState {
                error,
                ..Default::default()
            },
        )
        .await;
}

pub async fn run(state: AppState, mut control: watch::Receiver<Vec<Account>>) {
    loop {
        while control.borrow().is_empty() {
            if control.changed().await.is_err() {
                return;
            }
        }
        let connection = connect_async(HYPERLIQUID_WS).await;
        let Ok((mut socket, _)) = connection else {
            let accounts = control.borrow().clone();
            for account in accounts {
                clear(
                    &state,
                    &account,
                    Some("Hyperliquid connection failed; retrying".into()),
                )
                .await;
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        };
        let mut subscribed = HashSet::<Account>::new();
        let mut received = HashMap::<Account, (bool, bool)>::new();
        let mut ping = tokio::time::interval(Duration::from_secs(25));
        'connected: loop {
            let desired: HashSet<_> = control
                .borrow()
                .iter()
                .take(MAX_LIVE_ACCOUNTS)
                .cloned()
                .collect();
            for account in subscribed.difference(&desired) {
                for kind in ["openOrders", "clearinghouseState"] {
                    if socket
                        .send(subscription(kind, account, "unsubscribe"))
                        .await
                        .is_err()
                    {
                        break 'connected;
                    }
                }
                received.remove(account);
            }
            for account in desired.difference(&subscribed) {
                clear(&state, account, None).await;
                for kind in ["openOrders", "clearinghouseState"] {
                    if socket
                        .send(subscription(kind, account, "subscribe"))
                        .await
                        .is_err()
                    {
                        break 'connected;
                    }
                }
                received.insert(account.clone(), (false, false));
            }
            subscribed = desired;
            let excess = control
                .borrow()
                .iter()
                .skip(MAX_LIVE_ACCOUNTS)
                .cloned()
                .collect::<Vec<_>>();
            for account in excess {
                let already_reported =
                    state
                        .accounts
                        .read()
                        .await
                        .get(&account)
                        .is_some_and(|snapshot| {
                            snapshot.error.as_deref()
                                == Some("Hyperliquid allows 10 live accounts per connection")
                        });
                if !already_reported {
                    clear(
                        &state,
                        &account,
                        Some("Hyperliquid allows 10 live accounts per connection".into()),
                    )
                    .await;
                }
            }
            if subscribed.is_empty() {
                break 'connected;
            }
            tokio::select! {
                changed = control.changed() => if changed.is_err() { return; },
                _ = ping.tick() => {
                    if socket.send(Message::Text(r#"{"method":"ping"}"#.into())).await.is_err() { break 'connected; }
                }
                frame = socket.next() => {
                    let Some(Ok(frame)) = frame else { break 'connected; };
                    let Message::Text(text) = frame else { continue; };
                    let Ok(value) = serde_json::from_str::<Value>(&text) else { continue; };
                    let channel = value["channel"].as_str().unwrap_or_default();
                    if channel != "openOrders" && channel != "clearinghouseState" { continue; }
                    let data = &value["data"];
                    let Some(user) = data["user"].as_str() else { continue; };
                    let Some(account) = subscribed.iter().find(|account| account.address.eq_ignore_ascii_case(user)) else { continue; };
                    let Some(flags) = received.get_mut(account) else { continue; };
                    let mut current = state.accounts.read().await.get(account).cloned().unwrap_or_default();
                    if channel == "openOrders" {
                        let Some(orders) = parse_orders(data) else { continue; };
                        current.orders = orders;
                        flags.0 = true;
                    } else {
                        let Some(positions) = parse_positions(data) else { continue; };
                        current.positions = positions;
                        flags.1 = true;
                    }
                    current.connected = flags.0 && flags.1;
                    current.updated_at_ms = now_ms();
                    current.error = None;
                    state.publish_account(account, current).await;
                }
            }
        }
        for account in subscribed {
            clear(
                &state,
                &account,
                Some("Hyperliquid disconnected; reconnecting".into()),
            )
            .await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_orders_and_positions() {
        let orders = json!({"orders":[
            {"coin":"BTC","oid":42,"side":"B","limitPx":"100.0","sz":"0.2"},
            {"coin":"BTC","oid":43,"side":"A","limitPx":"102.0","sz":"0.2","isTrigger":true}
        ]});
        assert_eq!(parse_orders(&orders).unwrap()[0].order_id, 42);
        assert_eq!(parse_orders(&orders).unwrap().len(), 1);
        let state = json!({"clearinghouseState":{"assetPositions":[{"position":{"coin":"BTC","szi":"-0.2","entryPx":"101","unrealizedPnl":"2"}}]}});
        assert_eq!(parse_positions(&state).unwrap()[0].size, "-0.2");
    }
}
