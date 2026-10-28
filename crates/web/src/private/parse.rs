//! Exchange payloads become the same account state consumed by every widget.
use rust_decimal::Decimal;
use serde_json::Value;
use std::collections::HashMap;
use std::str::FromStr;
use terminal_core::{
    AccountState, Exchange, Market, MarketKind, OwnFill, OwnOrder, Position, SpotBalance, TradeSide,
};

pub fn text(value: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            value.get(*key).filter(|v| !v.is_null()).map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            })
        })
        .unwrap_or_default()
}
fn amount(value: &Value, keys: &[&str]) -> Result<Decimal, String> {
    Decimal::from_str(&text(value, keys)).map_err(|_| "Exchange returned an invalid amount".into())
}
fn decimal(value: Decimal) -> String {
    value.normalize().to_string()
}
pub fn time(value: &Value, keys: &[&str], fallback: i64) -> i64 {
    text(value, keys)
        .parse::<i64>()
        .ok()
        .filter(|v| *v > 0)
        .unwrap_or(fallback)
}
pub fn side(value: &str) -> TradeSide {
    match value {
        "Buy" | "BUY" | "buy" => TradeSide::Buy,
        "Sell" | "SELL" | "sell" => TradeSide::Sell,
        _ => TradeSide::Unknown,
    }
}

#[derive(Default)]
pub struct Store {
    pub state: AccountState,
    orders: HashMap<String, OwnOrder>,
    positions: HashMap<(String, String), Position>,
    versions: HashMap<String, i64>,
    pub multipliers: HashMap<String, Decimal>,
}
impl Store {
    pub fn okx_event(&mut self, value: &Value, now: i64) -> Result<Vec<OwnFill>, String> {
        let mut fills = Vec::new();
        if let Some(event) = value["event"].as_str() {
            // Subscription acknowledgements carry arg.channel, but no data array.
            if event == "error" || value.get("code").is_some_and(|code| code != "0") {
                return Err("OKX private subscription rejected".into());
            }
            match event {
                "channel-conn-count-error" => {
                    return Err("OKX private channel connection limit reached".into());
                }
                "unsubscribe" => return Err("OKX private subscription ended".into()),
                "notice" => return Err("OKX private stream notice; reconnecting".into()),
                _ => return Ok(fills),
            }
        }
        if let Some(channel) = value["arg"]["channel"].as_str() {
            for row in value["data"]
                .as_array()
                .ok_or("Invalid OKX private event")?
            {
                if channel == "account" {
                    for coin in row["details"]
                        .as_array()
                        .ok_or("Invalid OKX balance event")?
                    {
                        self.wallet(Exchange::Okx, coin, time(row, &["uTime"], now))?;
                    }
                    continue;
                }
                let kind =
                    if text(row, &["instType"]) == "SPOT" || value["arg"]["instType"] == "SPOT" {
                        MarketKind::Spot
                    } else {
                        MarketKind::Perp
                    };
                if kind == MarketKind::Perp
                    && !self.multipliers.contains_key(&text(row, &["instId"]))
                {
                    continue;
                }
                if channel == "positions" {
                    self.position(Exchange::Okx, row, now)?;
                }
                if channel == "orders" {
                    self.order(Exchange::Okx, kind, row, now)?;
                    if let Some(fill) = self.fill(Exchange::Okx, kind, row, now)? {
                        fills.push(fill);
                    }
                }
            }
        }
        Ok(fills)
    }

    fn multiplier(
        &self,
        exchange: Exchange,
        kind: MarketKind,
        symbol: &str,
    ) -> Result<Decimal, String> {
        if exchange == Exchange::Okx && kind == MarketKind::Perp {
            self.multipliers
                .get(symbol)
                .copied()
                .ok_or_else(|| "Missing OKX linear contract metadata".into())
        } else {
            Ok(Decimal::ONE)
        }
    }
    pub fn order(
        &mut self,
        exchange: Exchange,
        kind: MarketKind,
        row: &Value,
        observed: i64,
    ) -> Result<(), String> {
        let symbol = text(row, &["symbol", "instId", "s"]);
        let id = text(row, &["orderId", "ordId", "i"]);
        if symbol.is_empty() || id.is_empty() {
            return Err("Exchange returned an unidentified order".into());
        }
        let key = format!("{kind:?}/{symbol}/{id}");
        let updated = time(
            row,
            &["updatedTime", "uTime", "updateTime", "T", "E"],
            observed,
        );
        if self
            .versions
            .get(&key)
            .is_some_and(|previous| updated < *previous)
        {
            return Ok(());
        }
        self.versions.insert(key.clone(), updated);
        let status = text(row, &["orderStatus", "state", "status", "X"]);
        if matches!(status.as_str(), "PENDING_NEW" | "PENDING_CANCEL") {
            return Ok(());
        }
        if !matches!(
            status.as_str(),
            "New"
                | "PartiallyFilled"
                | "NEW"
                | "PARTIALLY_FILLED"
                | "live"
                | "partially_filled"
                | "Filled"
                | "Cancelled"
                | "Rejected"
                | "PartiallyFilledCanceled"
                | "Untriggered"
                | "Triggered"
                | "Deactivated"
                | "FILLED"
                | "CANCELED"
                | "REJECTED"
                | "EXPIRED"
                | "EXPIRED_IN_MATCH"
                | "filled"
                | "canceled"
                | "mmp_canceled"
        ) {
            return Err("Unknown exchange order status".into());
        }
        if !matches!(
            status.as_str(),
            "New" | "PartiallyFilled" | "NEW" | "PARTIALLY_FILLED" | "live" | "partially_filled"
        ) {
            self.orders.remove(&key);
            return Ok(());
        }
        let price = amount(row, &["price", "px", "p"])?;
        // Trigger-only/market orders do not rest at an executable ladder price.
        if price <= Decimal::ZERO {
            self.orders.remove(&key);
            return Ok(());
        }
        let size = if row.get("leavesQty").is_some() {
            amount(row, &["leavesQty"])?
        } else {
            amount(row, &["origQty", "qty", "sz", "q"])?
                .checked_sub(amount(
                    row,
                    &["executedQty", "cumExecQty", "accFillSz", "z"],
                )?)
                .ok_or("Order amount overflow")?
        };
        let size = size
            .checked_mul(self.multiplier(exchange, kind, &symbol)?)
            .ok_or("Order size overflow")?;
        let side = side(&text(row, &["side", "S"]));
        if side == TradeSide::Unknown {
            return Err("Exchange returned an invalid order side".into());
        }
        if size > Decimal::ZERO {
            self.orders.insert(
                key,
                OwnOrder {
                    kind: Some(kind),
                    external_id: Some(id.clone()),
                    coin: symbol,
                    order_id: id.parse().unwrap_or(0),
                    side,
                    price: decimal(price),
                    size: decimal(size),
                },
            );
        } else {
            self.orders.remove(&key);
        }
        Ok(())
    }
    pub fn position(
        &mut self,
        exchange: Exchange,
        row: &Value,
        observed: i64,
    ) -> Result<(), String> {
        let symbol = text(row, &["symbol", "instId", "s"]);
        let leg = text(row, &["positionSide", "positionIdx", "posSide", "ps"]);
        if symbol.is_empty() {
            return Err("Exchange returned an unidentified position".into());
        }
        let key = (symbol.clone(), leg.clone());
        let version_key = format!("position/{symbol}/{leg}");
        let updated = time(row, &["updatedTime", "uTime", "updateTime", "T"], observed);
        if self
            .versions
            .get(&version_key)
            .is_some_and(|previous| updated < *previous)
        {
            return Ok(());
        }
        self.versions.insert(version_key, updated);
        self.state
            .position_snapshot_times
            .entry(symbol.clone())
            .and_modify(|time| *time = (*time).max(updated))
            .or_insert(updated);
        let mut size = amount(row, &["positionAmt", "size", "pos", "pa"])?;
        if exchange == Exchange::Bybit && text(row, &["side"]) == "Sell"
            || exchange == Exchange::Okx && leg == "short"
        {
            size = -size.abs();
        }
        size = size
            .checked_mul(self.multiplier(exchange, MarketKind::Perp, &symbol)?)
            .ok_or("Position size overflow")?;
        if size == Decimal::ZERO {
            self.positions.remove(&key);
        } else {
            self.positions.insert(
                key,
                Position {
                    leg,
                    coin: symbol,
                    size: decimal(size),
                    entry_price: text(row, &["entryPrice", "avgPrice", "avgPx", "ep"]),
                    unrealized_pnl: text(row, &["unRealizedProfit", "unrealisedPnl", "upl", "up"]),
                    notional_value: text(row, &["notional", "positionValue", "notionalUsd"]),
                },
            );
        }
        self.state.positions_updated_at_ms = self.state.positions_updated_at_ms.max(updated);
        self.state
            .position_snapshot_times
            .insert(String::new(), self.state.positions_updated_at_ms);
        Ok(())
    }
    pub fn wallet(&mut self, exchange: Exchange, row: &Value, observed: i64) -> Result<(), String> {
        let coin = text(row, &["coin", "asset", "ccy", "a"]);
        if coin.is_empty() {
            return Err("Exchange returned an unidentified balance".into());
        }
        let version = format!("wallet/{coin}");
        if self.versions.get(&version).is_some_and(|v| observed < *v) {
            return Ok(());
        }
        let (total, hold) = match exchange {
            Exchange::Binance => {
                let hold = amount(row, &["locked", "l"])?;
                (
                    amount(row, &["free", "f"])?
                        .checked_add(hold)
                        .ok_or("Balance overflow")?,
                    hold,
                )
            }
            Exchange::Bybit => (
                amount(row, &["walletBalance"])?
                    .checked_sub(
                        row.get("spotBorrow")
                            .map(|_| amount(row, &["spotBorrow"]))
                            .transpose()?
                            .unwrap_or_default(),
                    )
                    .ok_or("Balance overflow")?,
                amount(row, &["locked"]).unwrap_or_default(),
            ),
            Exchange::Okx => (
                amount(row, &["cashBal"])?,
                amount(row, &["frozenBal"]).unwrap_or_default(),
            ),
            _ => return Err("Unsupported private exchange".into()),
        };
        self.versions.insert(version, observed);
        self.state.spot_balances.retain(|b| b.coin != coin);
        self.state.spot_balances.push(SpotBalance {
            coin,
            token: 0,
            total: decimal(total),
            hold: decimal(hold),
        });
        self.state.spot_updated_at_ms = self.state.spot_updated_at_ms.max(observed);
        Ok(())
    }
    pub fn fill(
        &self,
        exchange: Exchange,
        kind: MarketKind,
        row: &Value,
        observed: i64,
    ) -> Result<Option<OwnFill>, String> {
        let symbol = text(row, &["symbol", "instId", "s"]);
        let id = text(row, &["execId", "tradeId", "id", "t"]);
        if id.is_empty()
            || id == "-1"
            || exchange == Exchange::Bybit
                && !text(row, &["execType"]).is_empty()
                && text(row, &["execType"]) != "Trade"
        {
            return Ok(None);
        }
        let size_text = text(row, &["execQty", "fillSz", "qty", "l"]);
        if size_text.is_empty() {
            return Ok(None);
        }
        let size = Decimal::from_str(&size_text)
            .map_err(|_| "Invalid fill size")?
            .checked_mul(self.multiplier(exchange, kind, &symbol)?)
            .ok_or("Fill size overflow")?;
        if size <= Decimal::ZERO {
            return Ok(None);
        }
        if amount(row, &["execPrice", "fillPx", "price", "L"])? <= Decimal::ZERO {
            return Err("Invalid fill price".into());
        }
        let fill_side = if row.get("isBuyer").or_else(|| row.get("buyer")).is_some() {
            if row
                .get("isBuyer")
                .or_else(|| row.get("buyer"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                TradeSide::Buy
            } else {
                TradeSide::Sell
            }
        } else {
            side(&text(row, &["side", "S"]))
        };
        if fill_side == TradeSide::Unknown {
            return Err("Invalid fill side".into());
        }
        Ok(Some(OwnFill {
            order_external_id: Some(text(row, &["orderId", "ordId", "i"])),
            execution_id: Some(id.clone()),
            market: Market {
                exchange,
                kind,
                symbol,
            },
            time_ms: time(row, &["execTime", "fillTime", "time", "T"], observed),
            trade_id: id.parse().unwrap_or(0),
            order_id: text(row, &["orderId", "ordId", "i"]).parse().unwrap_or(0),
            side: fill_side,
            price: text(row, &["execPrice", "fillPx", "price", "L"]),
            size: decimal(size),
            start_position: None,
            taker: !row
                .get("isMaker")
                .or_else(|| row.get("isBuyerMaker"))
                .or_else(|| row.get("m"))
                .and_then(Value::as_bool)
                .unwrap_or_else(|| text(row, &["execType"]) == "M"),
            fee: if exchange == Exchange::Okx && row.get("accFillSz").is_some() {
                text(row, &["fillFee"])
            } else {
                text(row, &["execFee", "fee", "commission", "n"])
            },
            fee_token: text(
                row,
                &[
                    "fillFeeCcy",
                    "feeCurrency",
                    "feeCcy",
                    "commissionAsset",
                    "N",
                ],
            ),
        }))
    }
    pub fn refresh(&mut self) {
        self.state.orders = self.orders.values().cloned().collect();
        self.state
            .orders
            .sort_by(|a, b| a.coin.cmp(&b.coin).then(a.external_id.cmp(&b.external_id)));
        self.state.positions = self.positions.values().cloned().collect();
        self.state
            .positions
            .sort_by(|a, b| a.coin.cmp(&b.coin).then(a.leg.cmp(&b.leg)));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn okx_private_subscription_ack_is_not_account_data() {
        let mut store = Store::default();
        for arg in [
            json!({"channel":"account"}),
            json!({"channel":"orders","instType":"SPOT"}),
            json!({"channel":"orders","instType":"SWAP"}),
            json!({"channel":"positions","instType":"SWAP"}),
        ] {
            let ack = json!({"event":"subscribe","arg":arg,"connId":"test-connection"});
            assert!(subscription_ack(Exchange::Okx, &ack).is_some());
            assert!(store.okx_event(&ack, 1000).unwrap().is_empty());
        }
        assert!(
            store
                .okx_event(
                    &json!({"event":"channel-conn-count","channel":"account","connCount":"1"}),
                    1000
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.state, AccountState::default());
        let update = json!({"arg":{"channel":"account"},"data":[{"uTime":"1000","details":[{"ccy":"BTC","cashBal":"2","frozenBal":"0.5"}]}]});
        assert!(store.okx_event(&update, 1000).unwrap().is_empty());
        assert_eq!(store.state.spot_balances[0].total, "2");
        assert_eq!(store.state.spot_balances[0].hold, "0.5");
        let ack = json!({"event":"subscribe","arg":{"channel":"account"}});
        store.okx_event(&ack, 1001).unwrap();
        assert_eq!(
            store.state.spot_balances[0].total, "2",
            "ack must not reset live data"
        );
    }

    #[test]
    fn okx_private_protocol_errors_are_not_silenced() {
        let mut store = Store::default();
        let missing_data = json!({"arg":{"channel":"account"}});
        assert_eq!(
            store.okx_event(&missing_data, 1000).unwrap_err(),
            "Invalid OKX private event"
        );
        for data in [Value::Null, json!({})] {
            assert!(
                store
                    .okx_event(&json!({"arg":{"channel":"orders"},"data":data}), 1000)
                    .is_err()
            );
        }
        for event in ["error", "channel-conn-count-error", "unsubscribe", "notice"] {
            assert!(store.okx_event(&json!({"event":event}), 1000).is_err());
        }
        let rejected = json!({"event":"subscribe","arg":{"channel":"account"},"code":"60012"});
        assert!(store.okx_event(&rejected, 1000).is_err());
        assert_eq!(subscription_ack(Exchange::Okx, &rejected), None);
    }

    #[test]
    fn only_successful_subscription_confirmations_mark_streams_ready() {
        assert_eq!(
            subscription_ack(Exchange::Bybit, &json!({"op":"subscribe","success":true})),
            Some("bybit".into())
        );
        assert_eq!(
            subscription_ack(Exchange::Bybit, &json!({"op":"subscribe","success":false})),
            None
        );
        assert_eq!(
            subscription_ack(
                Exchange::Okx,
                &json!({"event":"subscribe","arg":{"channel":"orders","instType":"SWAP"}})
            ),
            Some("orders:SWAP".into())
        );
        assert_eq!(
            subscription_ack(Exchange::Okx, &json!({"event":"login","code":"0"})),
            None
        );
    }
    #[test]
    fn permission_checks_fail_closed_for_trading_keys() {
        assert!(read_only(
            Exchange::Binance,
            &json!({"enableReading":true,"enableFutures":false})
        ));
        assert!(!read_only(
            Exchange::Binance,
            &json!({"enableReading":true,"enableFutures":true})
        ));
        assert!(!read_only(
            Exchange::Binance,
            &json!({"enableReading":true,"enableFutureUnknownTrading":true})
        ));
        assert!(!read_only(
            Exchange::Binance,
            &json!({"enableReading":true,"enableWithdrawals":"true"})
        ));
        assert!(read_only(
            Exchange::Bybit,
            &json!({"result":{"readOnly":1}})
        ));
        assert!(!read_only(
            Exchange::Bybit,
            &json!({"result":{"readOnly":0}})
        ));
        assert!(read_only(
            Exchange::Okx,
            &json!({"data":[{"perm":"read_only"}]})
        ));
        assert!(!read_only(
            Exchange::Okx,
            &json!({"data":[{"perm":"read_only,trade"}]})
        ));
        for exchange in [Exchange::Binance, Exchange::Bybit, Exchange::Okx] {
            assert!(!read_only(exchange, &Value::Null));
        }
    }
    #[test]
    fn balances_use_base_assets_and_ignore_old_updates() {
        let mut store = Store::default();
        store
            .wallet(
                Exchange::Binance,
                &json!({"asset":"BTC","free":"1","locked":"2"}),
                20,
            )
            .unwrap();
        store
            .wallet(Exchange::Binance, &json!({"a":"BTC","f":"99","l":"0"}), 10)
            .unwrap();
        assert_eq!(store.state.spot_balances[0].total, "3");
        store
            .wallet(
                Exchange::Bybit,
                &json!({"coin":"ETH","walletBalance":"5","spotBorrow":"2","locked":"1"}),
                30,
            )
            .unwrap();
        assert_eq!(store.state.spot_balances[1].total, "3");
        store
            .wallet(
                Exchange::Okx,
                &json!({"ccy":"SOL","cashBal":"10","frozenBal":"4"}),
                40,
            )
            .unwrap();
        assert_eq!(store.state.spot_balances[2].hold, "4");
    }
    #[test]
    fn private_orders_keep_market_identity_and_ignore_stale_updates() {
        let mut store = Store::default();
        let row = json!({"symbol":"BTCUSDT","orderId":"uuid-order","price":"100","qty":"5","leavesQty":"3","orderStatus":"PartiallyFilled","side":"Buy","updatedTime":"20"});
        for kind in MarketKind::ALL {
            store.order(Exchange::Bybit, kind, &row, 20).unwrap();
        }
        store.refresh();
        assert_eq!(store.state.orders.len(), 2);
        assert_eq!(store.state.orders[0].size, "3");
        let mut canceled = row.clone();
        canceled["orderStatus"] = "Cancelled".into();
        canceled["updatedTime"] = "21".into();
        store
            .order(Exchange::Bybit, MarketKind::Spot, &canceled, 21)
            .unwrap();
        store
            .order(Exchange::Bybit, MarketKind::Spot, &row, 20)
            .unwrap();
        store.refresh();
        assert_eq!(store.state.orders.len(), 1);
        assert_eq!(store.state.orders[0].kind, Some(MarketKind::Perp));
    }
    #[test]
    fn okx_contract_sizes_and_hedge_legs_are_not_confused() {
        let mut store = Store::default();
        store
            .multipliers
            .insert("BTC-USDT-SWAP".into(), Decimal::from_str("0.01").unwrap());
        for leg in ["long", "short"] {
            store.position(Exchange::Okx, &json!({"instId":"BTC-USDT-SWAP","pos":"3","posSide":leg,"avgPx":"100","upl":"2","uTime":"30"}), 30).unwrap();
        }
        store.refresh();
        assert_eq!(store.state.positions.len(), 2);
        assert_eq!(store.state.positions[0].size, "0.03");
        assert_eq!(store.state.positions[1].size, "-0.03");
    }
    #[test]
    fn uuid_executions_preserve_partial_fills() {
        let store = Store::default();
        let mut history = Vec::new();
        for id in ["exec-a", "exec-b", "exec-a"] {
            let fill = store.fill(Exchange::Bybit, MarketKind::Perp, &json!({"symbol":"BTCUSDT","execId":id,"orderId":"same-order","execTime":"50","execType":"Trade","execQty":"1","execPrice":"100","side":"Buy","isMaker":true}),50).unwrap().unwrap();
            terminal_core::merge_account_fills(&mut history, vec![fill]);
        }
        assert_eq!(history.len(), 2);
        assert!(!history[0].taker);
    }
}

pub fn read_only(exchange: Exchange, value: &Value) -> bool {
    match exchange {
        Exchange::Binance => {
            value["enableReading"] == true
                && value.as_object().is_some_and(|fields| {
                    fields.iter().all(|(name, v)| {
                        if name.starts_with("enable") || name == "permitsUniversalTransfer" {
                            v == false
                                || (v == true
                                    && matches!(
                                        name.as_str(),
                                        "enableReading" | "enableFixReadOnly"
                                    ))
                        } else {
                            true
                        }
                    })
                })
        }
        Exchange::Bybit => value["result"]["readOnly"] == 1,
        Exchange::Okx => value["data"][0]["perm"]
            .as_str()
            .is_some_and(|permissions| {
                permissions
                    .split(',')
                    .all(|p| matches!(p.trim(), "read" | "read_only"))
            }),
        _ => false,
    }
}

pub fn subscription_ack(exchange: Exchange, value: &Value) -> Option<String> {
    match exchange {
        Exchange::Bybit if value["op"] == "subscribe" && value["success"] == true => {
            Some("bybit".into())
        }
        Exchange::Okx
            if value["event"] == "subscribe"
                && value.get("code").is_none_or(|code| code == "0") =>
        {
            Some(format!(
                "{}:{}",
                text(&value["arg"], &["channel"]),
                text(&value["arg"], &["instType"])
            ))
        }
        _ => None,
    }
}
