use std::{
    collections::{HashMap, HashSet},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use terminal_core::{
    Account, AccountState, Exchange, LivePosition, Market, MarketKind, OwnFill, OwnOrder, Position,
    ServerMessage, SpotBalance, TradeSide,
};
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
    let rows = data["clearinghouseState"]["assetPositions"].as_array()?;
    let mut positions = Vec::new();
    for row in rows {
        let position = &row["position"];
        let coin = field(position, "coin")?;
        let size = field(position, "szi")?;
        let amount = size.parse::<rust_decimal::Decimal>().ok()?;
        if amount == rust_decimal::Decimal::ZERO {
            continue;
        }
        positions.push(Position {
            coin,
            size,
            entry_price: field(position, "entryPx").unwrap_or_default(),
            unrealized_pnl: field(position, "unrealizedPnl").unwrap_or_default(),
            notional_value: field(position, "positionValue").unwrap_or_default(),
        });
    }
    Some(positions)
}

fn parse_spot_balances(data: &Value) -> Option<Vec<SpotBalance>> {
    data["spotState"]["balances"]
        .as_array()?
        .iter()
        .map(|row| {
            let total = field(row, "total")?;
            total.parse::<rust_decimal::Decimal>().ok()?;
            Some(SpotBalance {
                coin: field(row, "coin")?,
                token: u32::try_from(row["token"].as_u64()?).ok()?,
                total,
                hold: field(row, "hold")?,
            })
        })
        .collect()
}

fn parse_all_positions(data: &Value) -> Option<Vec<Position>> {
    let states = data["clearinghouseStates"].as_array()?;
    let mut positions = Vec::new();
    for pair in states {
        let dex = pair[0].as_str()?;
        let mut parsed = parse_positions(&json!({"clearinghouseState": pair[1]}))?;
        for position in &mut parsed {
            if !dex.is_empty() && !position.coin.contains(':') {
                position.coin = format!("{dex}:{}", position.coin);
            }
        }
        positions.extend(parsed);
    }
    Some(positions)
}

fn apply_position_snapshot(data: &Value, current: &mut AccountState) -> Option<bool> {
    let positions = parse_all_positions(data)?;
    let times = data["clearinghouseStates"]
        .as_array()?
        .iter()
        .map(|pair| {
            let dex = pair[0].as_str()?.to_owned();
            let time = pair[1]["time"].as_i64()?;
            (time > 0).then_some((dex, time))
        })
        .collect::<Option<HashMap<_, _>>>()?;
    let mut changed = false;
    for (dex, time) in times {
        if time
            < current
                .position_snapshot_times
                .get(&dex)
                .copied()
                .unwrap_or(0)
        {
            continue;
        }
        let belongs = |coin: &str| coin.split_once(':').map_or("", |(dex, _)| dex) == dex;
        current.positions.retain(|p| !belongs(&p.coin));
        current
            .positions
            .extend(positions.iter().filter(|p| belongs(&p.coin)).cloned());
        current.position_snapshot_times.insert(dex, time);
        changed = true;
    }
    let times = &current.position_snapshot_times;
    current.live_positions.retain(|p| {
        let dex = p.coin.split_once(':').map_or("", |(dex, _)| dex);
        p.time_ms > times.get(dex).copied().unwrap_or(0)
    });
    Some(changed)
}

fn fill_position(fill: &OwnFill) -> Option<LivePosition> {
    use rust_decimal::Decimal;
    if fill.market.kind != MarketKind::Perp || fill.time_ms <= 0 {
        return None;
    }
    let start = fill.start_position.as_deref()?.parse::<Decimal>().ok()?;
    let size = fill.size.parse::<Decimal>().ok()?;
    if size <= Decimal::ZERO {
        return None;
    }
    let end = match fill.side {
        TradeSide::Buy => start.checked_add(size)?,
        TradeSide::Sell => start.checked_sub(size)?,
        _ => return None,
    };
    Some(LivePosition {
        coin: fill.market.symbol.clone(),
        size: end.normalize().to_string(),
        time_ms: fill.time_ms,
    })
}

fn subscription(kind: &str, account: &Account, method: &str) -> Message {
    Message::Text(
        json!({
            "method": method,
            "subscription": if kind == "userFills" {
                json!({"type": kind, "user": account.address, "aggregateByTime": false})
            } else {
                json!({"type": kind, "user": account.address})
            }
        })
        .to_string()
        .into(),
    )
}

fn parse_fills(data: &Value) -> Option<Vec<OwnFill>> {
    Some(
        data["fills"]
            .as_array()?
            .iter()
            .filter_map(|row| {
                let coin = field(row, "coin")?;
                let kind = if coin.starts_with('@') || coin.contains('/') {
                    MarketKind::Spot
                } else {
                    MarketKind::Perp
                };
                let price = field(row, "px")?;
                let size = field(row, "sz")?;
                if [price.as_str(), size.as_str()].iter().any(|value| {
                    value
                        .parse::<rust_decimal::Decimal>()
                        .ok()
                        .is_none_or(|value| value <= rust_decimal::Decimal::ZERO)
                }) {
                    return None;
                }
                Some(OwnFill {
                    market: Market {
                        exchange: Exchange::Hyperliquid,
                        kind,
                        symbol: coin,
                    },
                    time_ms: row["time"].as_i64()?,
                    trade_id: row["tid"].as_u64()?,
                    order_id: row["oid"].as_u64()?,
                    side: match row["side"].as_str()? {
                        "B" => TradeSide::Buy,
                        "A" => TradeSide::Sell,
                        _ => return None,
                    },
                    price,
                    size,
                    start_position: field(row, "startPosition"),
                    taker: row["crossed"].as_bool()?,
                    fee: field(row, "fee").unwrap_or_default(),
                    fee_token: field(row, "feeToken").unwrap_or_default(),
                })
            })
            .collect(),
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
                for kind in [
                    "openOrders",
                    "allDexsClearinghouseState",
                    "userFills",
                    "spotState",
                ] {
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
                for kind in [
                    "openOrders",
                    "allDexsClearinghouseState",
                    "userFills",
                    "spotState",
                ] {
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
                    if !["openOrders", "allDexsClearinghouseState", "userFills", "spotState"].contains(&channel) { continue; }
                    let data = &value["data"];
                    let Some(user) = data["user"].as_str() else { continue; };
                    let Some(account) = subscribed.iter().find(|account| account.address.eq_ignore_ascii_case(user)) else { continue; };
                    if channel == "userFills" {
                        if let Some(fills) = parse_fills(data) {
                            let added = state.publish_fills(account, fills, data["isSnapshot"] == true).await;
                            for fill in added {
                                let Some(position) = fill_position(&fill) else { continue; };
                                let applied = {
                                    let mut accounts = state.accounts.write().await;
                                    let current = accounts.entry(account.clone()).or_default();
                                    let applied = current.apply_live_position(position.clone());
                                    if applied { current.updated_at_ms = now_ms(); }
                                    applied
                                };
                                if applied {
                                    // Send a compact delta for each execution, rather than resending
                                    // the entire account's orders and positions on every fill.
                                    let _ = state.publish(ServerMessage::AccountPosition {
                                        account: account.clone(), position,
                                    });
                                }
                            }
                        }
                        continue;
                    }
                    let Some(flags) = received.get_mut(account) else { continue; };
                    let mut current = state.accounts.read().await.get(account).cloned().unwrap_or_default();
                    if channel == "openOrders" {
                        let Some(orders) = parse_orders(data) else { continue; };
                        current.orders = orders;
                        flags.0 = true;
                    } else if channel == "allDexsClearinghouseState" {
                        let Some(changed) = apply_position_snapshot(data, &mut current) else { continue; };
                        if changed { current.positions_updated_at_ms = now_ms(); }
                        flags.1 = true;
                    } else {
                        let Some(balances) = parse_spot_balances(data) else { continue; };
                        current.spot_balances = balances;
                        current.spot_updated_at_ms = now_ms();
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

    fn fill(start: &str, size: &str, side: TradeSide, time_ms: i64) -> OwnFill {
        OwnFill {
            market: Market::for_exchange(Exchange::Hyperliquid),
            time_ms,
            trade_id: 1,
            order_id: 2,
            side,
            price: "100".into(),
            size: size.into(),
            start_position: Some(start.into()),
            taker: false,
            fee: "0".into(),
            fee_token: "USDC".into(),
        }
    }

    fn snapshot(time: i64, size: &str) -> Value {
        json!({"clearinghouseStates":[["", {"time":time,"assetPositions":[
            {"position":{"coin":"BTC","szi":size}}
        ]}]]})
    }

    #[test]
    fn live_size_does_not_wait_for_snapshot_or_roll_back_when_old_state_arrives() {
        let mut state = AccountState::default();
        apply_position_snapshot(&snapshot(1_000, "-188"), &mut state).unwrap();
        let update = fill_position(&fill("-188", "0.01", TradeSide::Buy, 1_001)).unwrap();
        assert_eq!(update.size, "-187.99");
        assert!(state.apply_live_position(update.clone()));
        assert!(!state.apply_live_position(update));
        apply_position_snapshot(&snapshot(1_000, "-188"), &mut state).unwrap();
        assert_eq!(state.live_positions[0].size, "-187.99");
        // Different fills in the same block retain stream order; no float drift.
        let update = fill_position(&fill("-187.99", "0.02", TradeSide::Buy, 1_001)).unwrap();
        assert!(state.apply_live_position(update));
        assert_eq!(state.live_positions[0].size, "-187.97");
        assert!(!state.apply_live_position(
            fill_position(&fill("-188", "0.01", TradeSide::Buy, 999)).unwrap()
        ));
        apply_position_snapshot(&snapshot(1_001, "-187.97"), &mut state).unwrap();
        assert!(state.live_positions.is_empty());
        assert_eq!(state.positions[0].size, "-187.97");
        apply_position_snapshot(&snapshot(999, "-188"), &mut state).unwrap();
        assert_eq!(state.positions[0].size, "-187.97");
        let old = state.clone();
        assert!(
            apply_position_snapshot(
                &json!({"clearinghouseStates":[["", {"assetPositions":[]}]]}),
                &mut state
            )
            .is_none()
        );
        assert_eq!(state, old);
    }

    #[test]
    fn fill_size_handles_closing_and_flipping_and_does_not_guess_missing_data() {
        assert_eq!(
            fill_position(&fill("-0.3", "0.3", TradeSide::Buy, 1))
                .unwrap()
                .size,
            "0"
        );
        assert_eq!(
            fill_position(&fill("-0.3", "0.4", TradeSide::Buy, 1))
                .unwrap()
                .size,
            "0.1"
        );
        assert_eq!(
            fill_position(&fill("0.3", "0.4", TradeSide::Sell, 1))
                .unwrap()
                .size,
            "-0.1"
        );
        let mut invalid = fill("-188", "0.01", TradeSide::Buy, 1);
        invalid.start_position = None;
        assert!(fill_position(&invalid).is_none());
        invalid.start_position = Some("NaN".into());
        assert!(fill_position(&invalid).is_none());
        invalid.start_position = Some("10".into());
        invalid.market.kind = MarketKind::Spot;
        assert!(fill_position(&invalid).is_none());
    }

    #[test]
    fn snapshot_reconciliation_uses_the_correct_builder_dex_clock() {
        let mut state = AccountState::default();
        state.apply_live_position(LivePosition {
            coin: "xyz:CL".into(),
            size: "3".into(),
            time_ms: 1_500,
        });
        let data = json!({"clearinghouseStates":[
            ["", {"time":2_000,"assetPositions":[]}],
            ["xyz", {"time":1_000,"assetPositions":[]}]
        ]});
        apply_position_snapshot(&data, &mut state).unwrap();
        assert_eq!(state.live_positions.len(), 1);
        apply_position_snapshot(
            &json!({"clearinghouseStates":[["xyz", {"time":1_500,"assetPositions":[]}]]}),
            &mut state,
        )
        .unwrap();
        assert!(state.live_positions.is_empty());
    }

    #[test]
    fn position_state_covers_native_and_builder_dexes_and_empty_snapshots() {
        let value = json!({"clearinghouseStates":[
            ["", {"assetPositions":[{"position":{"coin":"BTC","szi":"-2","entryPx":"100","positionValue":"202","unrealizedPnl":"-2"}}]}],
            ["xyz", {"assetPositions":[{"position":{"coin":"CL","szi":"3","entryPx":"80","positionValue":"246","unrealizedPnl":"6"}}]}]
        ]});
        let positions = parse_all_positions(&value).unwrap();
        assert_eq!(positions.len(), 2);
        assert_eq!(positions[0].notional_value, "202");
        assert_eq!(positions[1].coin, "xyz:CL");
        assert!(
            parse_all_positions(&json!({"clearinghouseStates":[["", {"assetPositions":[]}]]}))
                .unwrap()
                .is_empty()
        );
        assert!(parse_all_positions(&json!({"clearinghouseStates":[["xyz", {}]]})).is_none());
        assert!(
            parse_positions(&json!({"clearinghouseState":{"assetPositions":[
                {"position":{"coin":"BTC","szi":"invalid"}}
            ]}}))
            .is_none()
        );
    }

    #[test]
    fn spot_state_preserves_total_held_and_token_identity() {
        let balances = parse_spot_balances(&json!({"spotState":{"balances":[
            {"coin":"PURR","token":1,"total":"10","hold":"4"},
            {"coin":"PURR","token":99,"total":"500","hold":"0"}
        ]}}))
        .unwrap();
        assert_eq!(balances[0].token, 1);
        assert_eq!(balances[0].total, "10");
        assert_eq!(balances[0].hold, "4");
        assert_eq!(balances[1].token, 99);
        assert!(
            parse_spot_balances(&json!({"spotState":{"balances":[
                {"coin":"PURR","token":1,"total":"NaN","hold":"0"}
            ]}}))
            .is_none()
        );
        assert!(
            parse_spot_balances(&json!({"spotState":{"balances":[]}}))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn parses_individual_fills_and_preserves_side_fees_and_market_kind() {
        let value = json!({"fills":[
            {"coin":"BTC","px":"100.2","sz":"0.01","startPosition":"-188","side":"B","time":1000,"tid":1,"oid":2,"crossed":false,"fee":"-0.001","feeToken":"USDC"},
            {"coin":"@107","px":"100.3","sz":"0.02","side":"A","time":1001,"tid":3,"oid":4,"crossed":true,"fee":"0.002","feeToken":"UBTC"},
            {"coin":"BTC","px":"NaN","sz":"0.01","side":"B","time":1000,"tid":5,"oid":6,"crossed":false}
        ]});
        let fills = parse_fills(&value).unwrap();
        assert_eq!(fills.len(), 2);
        assert_eq!(fills[0].market.kind, MarketKind::Perp);
        assert_eq!(fills[0].side, TradeSide::Buy);
        assert!(!fills[0].taker);
        assert_eq!(fills[0].fee, "-0.001");
        assert_eq!(fills[0].start_position.as_deref(), Some("-188"));
        assert_eq!(fills[1].market.kind, MarketKind::Spot);
        assert_eq!(fills[1].market.symbol, "@107");
        assert_eq!(fills[1].side, TradeSide::Sell);
        assert!(fills[1].taker);
        let Message::Text(subscription) = subscription(
            "userFills",
            &Account {
                exchange: Exchange::Hyperliquid,
                address: "0x1234".into(),
            },
            "subscribe",
        ) else {
            panic!()
        };
        let value: Value = serde_json::from_str(&subscription).unwrap();
        assert_eq!(value["subscription"]["aggregateByTime"], false);
    }

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
