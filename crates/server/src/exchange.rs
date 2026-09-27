use std::{
    collections::{BTreeMap, VecDeque},
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::{SinkExt, StreamExt};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde_json::{Value, json};
use terminal_core::{
    BestBidAsk, Book, BookChange, Candle, Exchange, Level, Market, MarketKind, Trade, TradeSide,
};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::HeaderValue},
};

use crate::AppState;

type Error = Box<dyn std::error::Error + Send + Sync>;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse().ok())
}

fn integer(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str()?.split('.').next()?.parse().ok())
}

async fn lighter_market_id(market: &Market, state: &AppState) -> Result<u32, Error> {
    state
        .symbols(Exchange::Lighter, market.kind)
        .await
        .map_err(std::io::Error::other)?
        .into_iter()
        .find(|info| info.symbol == market.symbol)
        .and_then(|info| info.market_id)
        .ok_or_else(|| std::io::Error::other("unknown Lighter market").into())
}

async fn base_size_multiplier(market: &Market, state: &AppState) -> Result<Decimal, Error> {
    if market.kind != MarketKind::Perp || !matches!(market.exchange, Exchange::Okx | Exchange::Gate)
    {
        return Ok(Decimal::ONE);
    }
    state
        .symbols(market.exchange, market.kind)
        .await
        .map_err(std::io::Error::other)?
        .into_iter()
        .find(|info| info.symbol == market.symbol)
        .and_then(|info| info.size_multiplier)
        .and_then(|text| Decimal::from_str(&text).ok())
        .filter(|value| *value > Decimal::ZERO)
        .ok_or_else(|| std::io::Error::other("missing perpetual contract size multiplier").into())
}

fn decimal(value: &Value) -> Option<Decimal> {
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    Decimal::from_str(&text).ok()
}

fn parse_trades(exchange: Exchange, value: &Value) -> Vec<Trade> {
    let (rows, price, size, time): (&[Value], &str, &str, &str) = match exchange {
        Exchange::Binance if value["e"] == "trade" || value["e"] == "aggTrade" => {
            (std::slice::from_ref(value), "p", "q", "T")
        }
        Exchange::Okx if value["arg"]["channel"] == "trades" => (
            value["data"].as_array().map_or(&[], Vec::as_slice),
            "px",
            "sz",
            "ts",
        ),
        Exchange::Bybit
            if value["topic"]
                .as_str()
                .is_some_and(|topic| topic.starts_with("publicTrade.")) =>
        {
            (
                value["data"].as_array().map_or(&[], Vec::as_slice),
                "p",
                "v",
                "T",
            )
        }
        Exchange::Hyperliquid if value["channel"] == "trades" => (
            value["data"].as_array().map_or(&[], Vec::as_slice),
            "px",
            "sz",
            "time",
        ),
        Exchange::Gate if value["channel"] == "spot.trades" && value["event"] == "update" => (
            std::slice::from_ref(&value["result"]),
            "price",
            "amount",
            "create_time_ms",
        ),
        Exchange::Gate if value["channel"] == "futures.trades" && value["event"] == "update" => (
            value["result"].as_array().map_or(&[], Vec::as_slice),
            "price",
            "size",
            "create_time_ms",
        ),
        Exchange::Lighter
            if value["type"] == "update/trade" || value["type"] == "subscribed/trade" =>
        {
            (
                value["trades"].as_array().map_or(&[], Vec::as_slice),
                "price",
                "size",
                "timestamp",
            )
        }
        _ => return Vec::new(),
    };
    rows.iter()
        .filter_map(|row| {
            let price = decimal(&row[price])?;
            let raw_size = decimal(&row[size])?;
            let size = if exchange == Exchange::Gate && value["channel"] == "futures.trades" {
                raw_size.abs()
            } else {
                raw_size
            };
            if price <= Decimal::ZERO || size <= Decimal::ZERO {
                return None;
            }
            Some(Trade {
                price: price.normalize().to_string(),
                size: size.normalize().to_string(),
                time_ms: integer(&row[time]).unwrap_or_else(now_ms),
                side: match exchange {
                    Exchange::Binance => match row["m"].as_bool() {
                        Some(true) => TradeSide::Sell,
                        Some(false) => TradeSide::Buy,
                        None => TradeSide::Unknown,
                    },
                    Exchange::Okx => match row["side"].as_str() {
                        Some("buy") => TradeSide::Buy,
                        Some("sell") => TradeSide::Sell,
                        _ => TradeSide::Unknown,
                    },
                    Exchange::Gate if value["channel"] == "futures.trades" => {
                        if raw_size > Decimal::ZERO {
                            TradeSide::Buy
                        } else {
                            TradeSide::Sell
                        }
                    }
                    Exchange::Gate => match row["side"].as_str() {
                        Some("buy") => TradeSide::Buy,
                        Some("sell") => TradeSide::Sell,
                        _ => TradeSide::Unknown,
                    },
                    Exchange::Bybit => match row["S"].as_str() {
                        Some("Buy") => TradeSide::Buy,
                        Some("Sell") => TradeSide::Sell,
                        _ => TradeSide::Unknown,
                    },
                    Exchange::Hyperliquid => match row["side"].as_str() {
                        Some("B") => TradeSide::Buy,
                        Some("A") => TradeSide::Sell,
                        _ => TradeSide::Unknown,
                    },
                    Exchange::Lighter => match row["is_maker_ask"].as_bool() {
                        Some(true) => TradeSide::Buy,
                        Some(false) => TradeSide::Sell,
                        None => TradeSide::Unknown,
                    },
                },
            })
        })
        .collect()
}

fn parse_hyperliquid_bbo(value: &Value) -> Option<BestBidAsk> {
    if value["channel"] != "bbo" {
        return None;
    }
    let bid = decimal(&value["data"]["bbo"][0]["px"])?;
    let ask = decimal(&value["data"]["bbo"][1]["px"])?;
    if bid <= Decimal::ZERO || ask <= bid {
        return None;
    }
    Some(BestBidAsk {
        bid: bid.normalize().to_string(),
        ask: ask.normalize().to_string(),
        time_ms: integer(&value["data"]["time"]).unwrap_or_else(now_ms),
    })
}

fn parse_gate_bbo(value: &Value) -> Option<BestBidAsk> {
    if !matches!(
        value["channel"].as_str(),
        Some("spot.book_ticker" | "futures.book_ticker")
    ) || value["event"] != "update"
    {
        return None;
    }
    quote_from_values(
        &value["result"]["b"],
        &value["result"]["a"],
        &value["result"]["t"],
    )
}

fn parse_lighter_bbo(value: &Value) -> Option<BestBidAsk> {
    if value["type"] != "update/ticker" && value["type"] != "subscribed/ticker" {
        return None;
    }
    quote_from_values(
        &value["ticker"]["b"]["price"],
        &value["ticker"]["a"]["price"],
        &value["timestamp"],
    )
}

fn quote_from_values(bid: &Value, ask: &Value, time: &Value) -> Option<BestBidAsk> {
    let bid = decimal(bid)?;
    let ask = decimal(ask)?;
    if bid <= Decimal::ZERO || ask <= bid {
        return None;
    }
    Some(BestBidAsk {
        bid: bid.normalize().to_string(),
        ask: ask.normalize().to_string(),
        time_ms: integer(time).unwrap_or_else(now_ms),
    })
}

#[derive(Default)]
struct BookAccumulator {
    bids: BTreeMap<Decimal, Decimal>,
    asks: BTreeMap<Decimal, Decimal>,
    sequence: Option<i64>,
    ready: bool,
}

impl BookAccumulator {
    fn replace(&mut self, bids: &Value, asks: &Value, sequence: Option<i64>) -> bool {
        let (Some(bids), Some(asks)) = (bids.as_array(), asks.as_array()) else {
            return false;
        };
        self.bids.clear();
        self.asks.clear();
        Self::update_side(&mut self.bids, bids);
        Self::update_side(&mut self.asks, asks);
        self.sequence = sequence;
        self.ready = true;
        true
    }

    fn update(&mut self, bids: &Value, asks: &Value) -> bool {
        if !self.ready {
            return false;
        }
        let (Some(bids), Some(asks)) = (bids.as_array(), asks.as_array()) else {
            return false;
        };
        Self::update_side(&mut self.bids, bids);
        Self::update_side(&mut self.asks, asks);
        true
    }

    fn update_side(side: &mut BTreeMap<Decimal, Decimal>, levels: &[Value]) {
        for row in levels {
            let pair = if row.is_array() {
                (decimal(&row[0]), decimal(&row[1]))
            } else {
                (
                    decimal(&row["px"])
                        .or_else(|| decimal(&row["price"]))
                        .or_else(|| decimal(&row["p"])),
                    decimal(&row["sz"])
                        .or_else(|| decimal(&row["size"]))
                        .or_else(|| decimal(&row["s"])),
                )
            };
            if let (Some(price), Some(size)) = pair {
                if size.is_zero() {
                    side.remove(&price);
                } else {
                    side.insert(price, size);
                }
            }
        }
    }

    fn retain_top(&mut self, limit: usize) {
        while self.bids.len() > limit {
            self.bids.pop_first();
        }
        while self.asks.len() > limit {
            self.asks.pop_last();
        }
    }

    fn is_crossed(&self) -> bool {
        self.bids
            .last_key_value()
            .zip(self.asks.first_key_value())
            .is_some_and(|((bid, _), (ask, _))| bid >= ask)
    }

    fn book(&self, updated_at_ms: i64) -> Book {
        self.book_scaled(updated_at_ms, Decimal::ONE)
    }

    fn book_scaled(&self, updated_at_ms: i64, multiplier: Decimal) -> Book {
        let bids = Self::levels(self.bids.iter().rev(), multiplier);
        let asks = Self::levels(self.asks.iter(), multiplier);
        Book {
            bids,
            asks,
            updated_at_ms,
        }
    }

    fn levels<'a>(
        entries: impl Iterator<Item = (&'a Decimal, &'a Decimal)>,
        multiplier: Decimal,
    ) -> Vec<Level> {
        let mut depth_base = Decimal::ZERO;
        let mut depth_quote = Decimal::ZERO;
        entries
            .map(|(price, size)| {
                let size = *size * multiplier;
                let quote_size = *price * size;
                depth_base += size;
                depth_quote += quote_size;
                Level {
                    price: price.to_string(),
                    size: size.normalize().to_string(),
                    quote_size: quote_size.normalize().to_string(),
                    depth_base: depth_base.normalize().to_string(),
                    depth_quote: depth_quote.normalize().to_string(),
                }
            })
            .collect()
    }
}

fn book_changes(rows: &Value, multiplier: Decimal) -> Vec<BookChange> {
    rows.as_array().map_or_else(Vec::new, |rows| {
        rows.iter()
            .filter_map(|row| {
                let price = decimal(&row[0])
                    .or_else(|| decimal(&row["px"]))
                    .or_else(|| decimal(&row["price"]))
                    .or_else(|| decimal(&row["p"]))?;
                let size = decimal(&row[1])
                    .or_else(|| decimal(&row["sz"]))
                    .or_else(|| decimal(&row["size"]))
                    .or_else(|| decimal(&row["s"]))?;
                Some(BookChange {
                    price: price.normalize().to_string(),
                    size: (size * multiplier).normalize().to_string(),
                })
            })
            .collect()
    })
}

fn spawn_deep_snapshot(
    market: &Market,
    state: &AppState,
) -> tokio::task::JoinHandle<Result<Value, String>> {
    let client = state.http.clone();
    let market = market.clone();
    tokio::spawn(async move {
        let response = match market.exchange {
            Exchange::Binance => {
                client
                    .get(if market.kind == MarketKind::Spot {
                        "https://api.binance.com/api/v3/depth"
                    } else {
                        "https://fapi.binance.com/fapi/v1/depth"
                    })
                    .query(&[
                        ("symbol", market.symbol.as_str()),
                        (
                            "limit",
                            if market.kind == MarketKind::Spot {
                                "5000"
                            } else {
                                "1000"
                            },
                        ),
                    ])
                    .send()
                    .await
            }
            Exchange::Bybit => {
                client
                    .get("https://api.bybit.com/v5/market/full_orderbook")
                    .query(&[
                        (
                            "category",
                            if market.kind == MarketKind::Spot {
                                "spot"
                            } else {
                                "linear"
                            },
                        ),
                        ("symbol", market.symbol.as_str()),
                    ])
                    .send()
                    .await
            }
            _ => return Err("no deep snapshot for exchange".to_owned()),
        }
        .map_err(|error| error.to_string())?;
        response
            .error_for_status()
            .map_err(|error| error.to_string())?
            .json::<Value>()
            .await
            .map_err(|error| error.to_string())
    })
}

fn deep_update_ids(exchange: Exchange, value: &Value) -> Option<(i64, i64)> {
    match exchange {
        Exchange::Binance => Some((integer(&value["U"])?, integer(&value["u"])?)),
        Exchange::Bybit => {
            let id = integer(&value["data"]["u"])?;
            Some((id, id))
        }
        _ => None,
    }
}

fn apply_deep_delta(
    exchange: Exchange,
    kind: MarketKind,
    value: &Value,
    book: &mut BookAccumulator,
) -> Result<bool, Error> {
    let (first, last) = deep_update_ids(exchange, value)
        .ok_or_else(|| std::io::Error::other("missing book update ID"))?;
    let previous = book
        .sequence
        .ok_or_else(|| std::io::Error::other("book lacks snapshot ID"))?;
    if exchange == Exchange::Bybit && last == 1 && previous != 1 {
        return Err(std::io::Error::other("Bybit book reset").into());
    }
    if last <= previous {
        return Ok(false);
    }
    if exchange == Exchange::Binance
        && kind == MarketKind::Perp
        && integer(&value["pu"]) != Some(previous)
    {
        return Err(std::io::Error::other("Binance perpetual book sequence gap").into());
    }
    if first > previous + 1 || (exchange == Exchange::Bybit && last != previous + 1) {
        return Err(std::io::Error::other("deep book sequence gap").into());
    }
    let (bids, asks) = match exchange {
        Exchange::Binance => (&value["b"], &value["a"]),
        Exchange::Bybit => (&value["data"]["b"], &value["data"]["a"]),
        _ => unreachable!(),
    };
    if !book.update(bids, asks) {
        return Err(std::io::Error::other("invalid deep book update").into());
    }
    book.sequence = Some(last);
    Ok(true)
}

fn initialize_deep_book(
    exchange: Exchange,
    kind: MarketKind,
    snapshot: &Value,
    buffered: &mut VecDeque<Value>,
    book: &mut BookAccumulator,
) -> Result<bool, Error> {
    let (bids, asks, id) = match exchange {
        Exchange::Binance => (
            &snapshot["bids"],
            &snapshot["asks"],
            integer(&snapshot["lastUpdateId"]),
        ),
        Exchange::Bybit if snapshot["retCode"] == 0 => {
            let data = &snapshot["result"];
            (&data["b"], &data["a"], integer(&data["u"]))
        }
        _ => return Err(std::io::Error::other("invalid deep book snapshot").into()),
    };
    let id = id.ok_or_else(|| std::io::Error::other("snapshot lacks update ID"))?;
    let mut candidate = BookAccumulator::default();
    if !candidate.replace(bids, asks, Some(id)) {
        return Err(std::io::Error::other("snapshot lacks book levels").into());
    }
    if exchange == Exchange::Binance
        && kind == MarketKind::Perp
        && !buffered
            .iter()
            .any(|event| deep_update_ids(exchange, event).is_some_and(|(_, last)| last >= id))
    {
        return Ok(false);
    }
    if let Some(first_new) = buffered
        .iter()
        .find(|event| deep_update_ids(exchange, event).is_some_and(|(_, last)| last > id))
    {
        let (first, _) = deep_update_ids(exchange, first_new).unwrap();
        if first > id + 1
            || (exchange == Exchange::Binance && kind == MarketKind::Perp && first > id)
        {
            return Ok(false);
        }
    }
    // Futures' first event overlaps the REST snapshot; its `pu` refers to a
    // preceding stream event rather than the snapshot ID.
    let mut first_applied = false;
    for event in buffered.iter() {
        if exchange == Exchange::Binance && kind == MarketKind::Perp && !first_applied {
            let Some((_, last)) = deep_update_ids(exchange, event) else {
                continue;
            };
            if last <= id {
                continue;
            }
            let (bids, asks) = (&event["b"], &event["a"]);
            if !candidate.update(bids, asks) {
                return Err(std::io::Error::other("invalid Binance perpetual book update").into());
            }
            candidate.sequence = Some(last);
            first_applied = true;
            continue;
        }
        if apply_deep_delta(exchange, kind, event, &mut candidate)? {
            first_applied = true;
        }
    }
    *book = candidate;
    buffered.clear();
    Ok(true)
}

fn apply_gate_obu(data: &Value, book: &mut BookAccumulator) -> Result<bool, Error> {
    if data["full"] == true {
        let id = integer(&data["u"])
            .ok_or_else(|| std::io::Error::other("Gate snapshot lacks sequence ID"))?;
        if !book.replace(&data["b"], &data["a"], Some(id)) || book.is_crossed() {
            return Err(std::io::Error::other("invalid Gate full book snapshot").into());
        }
        return Ok(true);
    }
    if !book.ready {
        return Ok(false);
    }
    let previous = book
        .sequence
        .ok_or_else(|| std::io::Error::other("Gate book lacks sequence ID"))?;
    let first = integer(&data["U"])
        .ok_or_else(|| std::io::Error::other("Gate book update lacks first ID"))?;
    let last = integer(&data["u"])
        .ok_or_else(|| std::io::Error::other("Gate book update lacks last ID"))?;
    if first != previous + 1 || last < first {
        return Err(std::io::Error::other("Gate book sequence gap").into());
    }
    // Gate sends null for a side with no changes, including on empty updates.
    let empty = Value::Array(Vec::new());
    let bids = if data["b"].is_null() {
        &empty
    } else {
        &data["b"]
    };
    let asks = if data["a"].is_null() {
        &empty
    } else {
        &data["a"]
    };
    let changed = bids.as_array().is_some_and(|rows| !rows.is_empty())
        || asks.as_array().is_some_and(|rows| !rows.is_empty());
    if !book.update(bids, asks) || book.is_crossed() {
        return Err(std::io::Error::other("invalid Gate book update").into());
    }
    book.sequence = Some(last);
    Ok(changed)
}

pub async fn run_books(market: Market, state: AppState) {
    loop {
        let result = if market.exchange == Exchange::Binance && market.kind == MarketKind::Perp {
            tokio::select! {
                result = stream_books(&market, &state) => result,
                _ = run_binance_perp_trades(&market, &state) => Err(std::io::Error::other("Binance perpetual trade stream stopped").into()),
            }
        } else {
            stream_books(&market, &state).await
        };
        if let Err(error) = result {
            eprintln!(
                "{} {} book: {error}",
                market.exchange.label(),
                market.symbol
            );
        }
        state.disconnected(&market).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn run_binance_perp_trades(market: &Market, state: &AppState) {
    let symbol = market.symbol.to_ascii_lowercase();
    let url = format!("wss://fstream.binance.com/market/stream?streams={symbol}@aggTrade");
    loop {
        let result: Result<(), Error> = async {
            let (mut socket, _) = connect_async(&url).await?;
            while let Some(frame) = socket.next().await {
                match frame? {
                    Message::Text(text) => {
                        let envelope: Value = serde_json::from_str(&text)?;
                        let trades = parse_trades(Exchange::Binance, &envelope["data"]);
                        if !trades.is_empty() {
                            state.publish_trades(market, trades).await;
                        }
                    }
                    Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            eprintln!("Binance {} perpetual trades: {error}", market.symbol);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn stream_books(market: &Market, state: &AppState) -> Result<(), Error> {
    let symbol = market.symbol.as_str();
    let multiplier = base_size_multiplier(market, state).await?;
    let lighter_id = if market.exchange == Exchange::Lighter {
        Some(lighter_market_id(market, state).await?)
    } else {
        None
    };
    let url = match market.exchange {
        Exchange::Binance => {
            if market.kind == MarketKind::Spot {
                format!(
                    "wss://stream.binance.com:9443/stream?streams={symbol}@depth@100ms/{symbol}@trade",
                    symbol = symbol.to_ascii_lowercase()
                )
            } else {
                format!(
                    "wss://fstream.binance.com/public/stream?streams={symbol}@depth@100ms",
                    symbol = symbol.to_ascii_lowercase()
                )
            }
        }
        Exchange::Okx => "wss://ws.okx.com:8443/ws/v5/public".to_owned(),
        Exchange::Bybit => format!(
            "wss://stream.bybit.com/v5/public/{}",
            if market.kind == MarketKind::Spot {
                "spot"
            } else {
                "linear"
            }
        ),
        Exchange::Hyperliquid => "wss://api.hyperliquid.xyz/ws".to_owned(),
        Exchange::Gate => if market.kind == MarketKind::Spot {
            "wss://api.gateio.ws/ws/v4/"
        } else {
            "wss://fx-ws.gateio.ws/v4/ws/usdt"
        }
        .to_owned(),
        Exchange::Lighter => "wss://mainnet.zklighter.elliot.ai/stream".to_owned(),
    };
    let mut request = url.into_client_request()?;
    if market.exchange == Exchange::Gate && market.kind == MarketKind::Perp {
        request
            .headers_mut()
            .insert("X-Gate-Size-Decimal", HeaderValue::from_static("1"));
    }
    let (mut socket, _) = connect_async(request).await?;
    let subscriptions = match market.exchange {
        Exchange::Binance => vec![],
        Exchange::Okx => vec![json!({"op":"subscribe","args":[
            {"channel":"books","instId":symbol},
            {"channel":"trades","instId":symbol}
        ]})],
        Exchange::Bybit => vec![json!({"op":"subscribe","args":[
            format!("orderbook.full.{symbol}"),format!("publicTrade.{symbol}")
        ]})],
        Exchange::Hyperliquid => vec![
            json!({"method":"subscribe","subscription":{"type":"l2Book","coin":symbol}}),
            json!({"method":"subscribe","subscription":{"type":"bbo","coin":symbol}}),
            json!({"method":"subscribe","subscription":{"type":"trades","coin":symbol}}),
        ],
        Exchange::Gate => [
            (
                if market.kind == MarketKind::Spot {
                    "spot.obu"
                } else {
                    "futures.obu"
                },
                json!([format!("ob.{symbol}.400")]),
            ),
            (
                if market.kind == MarketKind::Spot {
                    "spot.book_ticker"
                } else {
                    "futures.book_ticker"
                },
                json!([symbol]),
            ),
            (
                if market.kind == MarketKind::Spot {
                    "spot.trades"
                } else {
                    "futures.trades"
                },
                json!([symbol]),
            ),
        ]
        .into_iter()
        .map(|(channel, payload)| {
            json!({
                "time": now_ms() / 1000,
                "channel": channel,
                "event": "subscribe",
                "payload": payload,
            })
        })
        .collect(),
        Exchange::Lighter => ["order_book", "ticker", "trade"]
            .into_iter()
            .map(|channel| {
                json!({
                    "type": "subscribe",
                    "channel": format!("{channel}/{}", lighter_id.unwrap()),
                })
            })
            .collect(),
    };
    for subscription in subscriptions {
        socket
            .send(Message::Text(subscription.to_string().into()))
            .await?;
    }
    let mut book = BookAccumulator::default();
    let mut buffered = VecDeque::<Value>::new();
    let mut snapshot_task: Option<tokio::task::JoinHandle<Result<Value, String>>> = None;
    let mut keepalive = tokio::time::interval(Duration::from_secs(60));
    keepalive.tick().await;
    loop {
        let frame = tokio::select! {
            frame = socket.next() => frame,
            result = async { snapshot_task.as_mut().expect("snapshot task").await }, if snapshot_task.is_some() => {
                snapshot_task = None;
                let snapshot = result.map_err(std::io::Error::other)?
                    .map_err(std::io::Error::other)?;
                if initialize_deep_book(market.exchange, market.kind, &snapshot, &mut buffered, &mut book)? {
                    state.publish_book(market, book.book(now_ms())).await;
                } else {
                    snapshot_task = Some(spawn_deep_snapshot(market, state));
                }
                continue;
            }
            _ = keepalive.tick() => {
                socket.send(Message::Ping(Vec::new().into())).await?;
                continue;
            }
        };
        let Some(frame) = frame else { break };
        match frame? {
            Message::Text(text) => {
                let envelope: Value = serde_json::from_str(&text)?;
                let value = if market.exchange == Exchange::Binance {
                    &envelope["data"]
                } else {
                    &envelope
                };
                let mut trades = parse_trades(market.exchange, value);
                if !trades.is_empty() {
                    if multiplier != Decimal::ONE {
                        for trade in &mut trades {
                            if let Ok(size) = Decimal::from_str(&trade.size) {
                                trade.size = (size * multiplier).normalize().to_string();
                            }
                        }
                    }
                    state.publish_trades(market, trades).await;
                }
                if market.exchange == Exchange::Hyperliquid
                    && let Some(quote) = parse_hyperliquid_bbo(value)
                {
                    state.publish_best_bid_ask(market, quote).await;
                }
                let quote = match market.exchange {
                    Exchange::Gate => parse_gate_bbo(value),
                    Exchange::Lighter => parse_lighter_bbo(value),
                    _ => None,
                };
                if let Some(quote) = quote {
                    state.publish_best_bid_ask(market, quote).await;
                }
                let deep_delta = match market.exchange {
                    Exchange::Binance => value["e"] == "depthUpdate",
                    Exchange::Bybit => value["topic"]
                        .as_str()
                        .is_some_and(|topic| topic.starts_with("orderbook.full.")),
                    _ => false,
                };
                if deep_delta && !book.ready {
                    if buffered.len() >= 10_000 {
                        return Err(std::io::Error::other("book snapshot buffer overflow").into());
                    }
                    buffered.push_back(value.clone());
                    if snapshot_task.is_none() {
                        snapshot_task = Some(spawn_deep_snapshot(market, state));
                    }
                    continue;
                }
                let stamp = match market.exchange {
                    Exchange::Binance => integer(&value["E"]),
                    Exchange::Okx => integer(&value["data"][0]["ts"]),
                    Exchange::Bybit => integer(&value["ts"]),
                    Exchange::Hyperliquid => integer(&value["data"]["time"]),
                    Exchange::Gate => integer(&value["result"]["t"]),
                    Exchange::Lighter => integer(&value["timestamp"]),
                }
                .unwrap_or_else(now_ms);
                let mut delta_rows: Option<(&Value, &Value)> = None;
                let ready = match market.exchange {
                    Exchange::Binance => {
                        if deep_delta
                            && apply_deep_delta(market.exchange, market.kind, value, &mut book)?
                        {
                            delta_rows = Some((&value["b"], &value["a"]));
                            true
                        } else {
                            false
                        }
                    }
                    Exchange::Okx => {
                        let data = &value["data"][0];
                        if data.is_null() {
                            false
                        } else if value["action"] == "snapshot" {
                            book.replace(&data["bids"], &data["asks"], integer(&data["seqId"]))
                        } else if value["action"] == "update" {
                            let previous = integer(&data["prevSeqId"]);
                            if book.ready && previous != book.sequence {
                                return Err(std::io::Error::other("OKX book sequence gap").into());
                            }
                            let updated = book.update(&data["bids"], &data["asks"]);
                            if updated {
                                book.sequence = integer(&data["seqId"]);
                                delta_rows = Some((&data["bids"], &data["asks"]));
                            }
                            updated
                        } else {
                            false
                        }
                    }
                    Exchange::Bybit => {
                        let data = &value["data"];
                        if deep_delta
                            && apply_deep_delta(market.exchange, market.kind, value, &mut book)?
                        {
                            delta_rows = Some((&data["b"], &data["a"]));
                            true
                        } else {
                            false
                        }
                    }
                    Exchange::Hyperliquid => {
                        if value["channel"] == "l2Book" {
                            book.replace(
                                &value["data"]["levels"][0],
                                &value["data"]["levels"][1],
                                None,
                            )
                        } else {
                            false
                        }
                    }
                    Exchange::Gate => {
                        if matches!(value["channel"].as_str(), Some("spot.obu" | "futures.obu"))
                            && (value["event"] == "update"
                                || (value["channel"] == "futures.obu" && value["event"].is_null()))
                        {
                            let data = &value["result"];
                            apply_gate_obu(data, &mut book)?
                        } else {
                            false
                        }
                    }
                    Exchange::Lighter => {
                        if value["type"] == "update/order_book"
                            || value["type"] == "subscribed/order_book"
                        {
                            let data = &value["order_book"];
                            let nonce = integer(&data["nonce"]);
                            let previous = integer(&data["begin_nonce"]);
                            let snapshot = value["type"] == "subscribed/order_book";
                            if book.ready && !snapshot && previous != book.sequence {
                                return Err(std::io::Error::other("Lighter book nonce gap").into());
                            }
                            let updated = if snapshot || !book.ready {
                                book.replace(&data["bids"], &data["asks"], nonce)
                            } else {
                                book.update(&data["bids"], &data["asks"])
                            };
                            if updated {
                                book.sequence = nonce;
                                if !snapshot {
                                    delta_rows = Some((&data["bids"], &data["asks"]));
                                }
                            }
                            updated
                        } else {
                            false
                        }
                    }
                };
                if ready {
                    if matches!(market.exchange, Exchange::Okx | Exchange::Gate) {
                        book.retain_top(400);
                    }
                    let current = book.book_scaled(stamp, multiplier);
                    if let Some((bids, asks)) = delta_rows
                        .filter(|_| !matches!(market.exchange, Exchange::Okx | Exchange::Gate))
                    {
                        state
                            .publish_book_delta(
                                market,
                                current,
                                book_changes(bids, multiplier),
                                book_changes(asks, multiplier),
                            )
                            .await;
                    } else {
                        state.publish_book(market, current).await;
                    }
                }
            }
            Message::Ping(payload) => {
                socket.send(Message::Pong(payload)).await?;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}

pub async fn run_candles(market: Market, state: AppState, client: reqwest::Client) {
    loop {
        let requested_at_ms = now_ms();
        match fetch_candles(&market, &state, &client).await {
            Ok(candles) => {
                state
                    .publish_candles(&market, candles, requested_at_ms)
                    .await
            }
            Err(error) => eprintln!(
                "{} {} candles: {error}",
                market.exchange.label(),
                market.symbol
            ),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn fetch_candles(
    market: &Market,
    state: &AppState,
    client: &reqwest::Client,
) -> Result<Vec<Candle>, Error> {
    let symbol = market.symbol.as_str();
    let multiplier = base_size_multiplier(market, state)
        .await?
        .to_f64()
        .unwrap_or(1.0);
    let value: Value = match market.exchange {
        Exchange::Binance => {
            client
                .get(if market.kind == MarketKind::Spot {
                    "https://api.binance.com/api/v3/klines"
                } else {
                    "https://fapi.binance.com/fapi/v1/klines"
                })
                .query(&[("symbol", symbol), ("interval", "1m"), ("limit", "300")])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        Exchange::Okx => {
            client
                .get("https://www.okx.com/api/v5/market/candles")
                .query(&[("instId", symbol), ("bar", "1m"), ("limit", "300")])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        Exchange::Bybit => {
            client
                .get("https://api.bybit.com/v5/market/kline")
                .query(&[
                    (
                        "category",
                        if market.kind == MarketKind::Spot {
                            "spot"
                        } else {
                            "linear"
                        },
                    ),
                    ("symbol", symbol),
                    ("interval", "1"),
                    ("limit", "300"),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        Exchange::Hyperliquid => {
            client
                .post("https://api.hyperliquid.xyz/info")
                .json(&json!({"type":"candleSnapshot","req":{
                    "coin":symbol,"interval":"1m","startTime":now_ms()-300*60_000,
                    "endTime":now_ms()
                }}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        Exchange::Gate => {
            client
                .get(if market.kind == MarketKind::Spot {
                    "https://api.gateio.ws/api/v4/spot/candlesticks"
                } else {
                    "https://api.gateio.ws/api/v4/futures/usdt/candlesticks"
                })
                .query(&[
                    (
                        if market.kind == MarketKind::Spot {
                            "currency_pair"
                        } else {
                            "contract"
                        },
                        symbol,
                    ),
                    ("interval", "1m"),
                    ("limit", "300"),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        Exchange::Lighter => {
            let market_id = lighter_market_id(market, state).await?;
            let end = now_ms() / 1000;
            client
                .get("https://mainnet.zklighter.elliot.ai/api/v1/candles")
                .query(&[
                    ("market_id", market_id.to_string()),
                    ("resolution", "1m".to_owned()),
                    ("start_timestamp", (end - 300 * 60).to_string()),
                    ("end_timestamp", end.to_string()),
                    ("count_back", "300".to_owned()),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
    };
    let rows = match market.exchange {
        Exchange::Binance | Exchange::Hyperliquid => &value,
        Exchange::Okx => &value["data"],
        Exchange::Bybit => &value["result"]["list"],
        Exchange::Gate => &value,
        Exchange::Lighter => &value["c"],
    };
    let mut candles = rows
        .as_array()
        .ok_or_else(|| std::io::Error::other("invalid candle response"))?
        .iter()
        .filter_map(|row| parse_candle(market.exchange, market.kind, row))
        .collect::<Vec<_>>();
    if multiplier != 1.0 {
        for candle in &mut candles {
            candle.volume *= multiplier;
        }
    }
    candles.sort_by_key(|c| c.time);
    candles.dedup_by_key(|c| c.time);
    Ok(candles)
}

fn parse_candle(exchange: Exchange, kind: MarketKind, row: &Value) -> Option<Candle> {
    let (time, open, high, low, close, volume) =
        if exchange == Exchange::Hyperliquid || exchange == Exchange::Lighter {
            (
                &row["t"], &row["o"], &row["h"], &row["l"], &row["c"], &row["v"],
            )
        } else if exchange == Exchange::Gate && kind == MarketKind::Perp {
            (
                &row["t"], &row["o"], &row["h"], &row["l"], &row["c"], &row["v"],
            )
        } else if exchange == Exchange::Gate {
            (&row[0], &row[5], &row[3], &row[4], &row[2], &row[6])
        } else {
            (&row[0], &row[1], &row[2], &row[3], &row[4], &row[5])
        };
    Some(Candle {
        time: if exchange == Exchange::Gate {
            integer(time)?
        } else {
            integer(time)? / 1000
        },
        open: number(open)?,
        high: number(high)?,
        low: number(low)?,
        close: number(close)?,
        volume: number(volume)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hyperliquid_bbo_for_live_price_comparison() {
        let value = json!({
            "channel":"bbo",
            "data":{
                "coin":"BTC",
                "time":1_700_000_000_123_i64,
                "bbo":[{"px":"100.10","sz":"2","n":1},{"px":"100.20","sz":"3","n":2}]
            }
        });
        let quote = parse_hyperliquid_bbo(&value).expect("BBO update");
        assert_eq!((quote.bid.as_str(), quote.ask.as_str()), ("100.1", "100.2"));
        assert_eq!(quote.time_ms, 1_700_000_000_123);
    }

    #[test]
    fn parses_every_trade_with_size_from_each_exchange() {
        let examples = [
            (
                Exchange::Binance,
                json!({"e":"trade","p":"101.25","q":"0.75","T":1700000000001_i64}),
            ),
            (
                Exchange::Okx,
                json!({"arg":{"channel":"trades"},"data":[{"px":"99","sz":"2","ts":"1700000000000"},{"px":"101.25","sz":"0.75","ts":"1700000000001"}]}),
            ),
            (
                Exchange::Bybit,
                json!({"topic":"publicTrade.BTCUSDT","data":[{"p":"99","v":"2","T":1700000000000_i64},{"p":"101.25","v":"0.75","T":1700000000001_i64}]}),
            ),
            (
                Exchange::Hyperliquid,
                json!({"channel":"trades","data":[{"px":"99","sz":"2","time":1700000000000_i64},{"px":"101.25","sz":"0.75","time":1700000000001_i64}]}),
            ),
        ];
        for (exchange, payload) in examples {
            let trades = parse_trades(exchange, &payload);
            assert_eq!(trades.last().unwrap().price, "101.25");
            assert_eq!(trades.last().unwrap().size, "0.75");
            assert_eq!(trades.last().unwrap().time_ms, 1_700_000_000_001);
            assert_eq!(
                trades.len(),
                if exchange == Exchange::Binance { 1 } else { 2 }
            );
        }
    }

    #[test]
    fn normalizes_aggressor_side_for_each_exchange() {
        let examples = [
            (
                Exchange::Binance,
                json!({"e":"trade","p":"100","q":"1","T":1,"m":true}),
                TradeSide::Sell,
            ),
            (
                Exchange::Binance,
                json!({"e":"trade","p":"100","q":"1","T":1,"m":false}),
                TradeSide::Buy,
            ),
            (
                Exchange::Okx,
                json!({"arg":{"channel":"trades"},"data":[{"px":"100","sz":"1","ts":"1","side":"buy"}]}),
                TradeSide::Buy,
            ),
            (
                Exchange::Bybit,
                json!({"topic":"publicTrade.BTCUSDT","data":[{"p":"100","v":"1","T":1,"S":"Sell"}]}),
                TradeSide::Sell,
            ),
            (
                Exchange::Hyperliquid,
                json!({"channel":"trades","data":[{"px":"100","sz":"1","time":1,"side":"B"}]}),
                TradeSide::Buy,
            ),
            (
                Exchange::Gate,
                json!({"channel":"spot.trades","event":"update","result":{"price":"100","amount":"1","create_time_ms":"1","side":"sell"}}),
                TradeSide::Sell,
            ),
            (
                Exchange::Lighter,
                json!({"type":"update/trade","trades":[{"price":"100","size":"1","timestamp":1,"is_maker_ask":true}]}),
                TradeSide::Buy,
            ),
            (
                Exchange::Lighter,
                json!({"type":"update/trade","trades":[{"price":"100","size":"1","timestamp":1,"is_maker_ask":false}]}),
                TradeSide::Sell,
            ),
        ];
        for (exchange, payload, expected) in examples {
            assert_eq!(parse_trades(exchange, &payload)[0].side, expected);
        }
    }

    #[test]
    fn book_updates_keep_price_order_and_delete_zero_size() {
        let mut book = BookAccumulator::default();
        assert!(book.replace(
            &json!([["101", "2"], ["100", "1"]]),
            &json!([["102", "3"]]),
            None
        ));
        assert!(book.update(&json!([["101", "0"], ["99", "4"]]), &json!([["103", "1"]])));
        let view = book.book(1);
        assert_eq!(
            view.bids
                .iter()
                .map(|level| level.price.as_str())
                .collect::<Vec<_>>(),
            ["100", "99"]
        );
        assert_eq!(
            view.asks
                .iter()
                .map(|level| level.price.as_str())
                .collect::<Vec<_>>(),
            ["102", "103"]
        );
        assert_eq!(view.bids[1].quote_size, "396");
        assert_eq!(view.asks[0].quote_size, "306");
        assert_eq!(view.bids[1].depth_base, "5");
        assert_eq!(view.bids[1].depth_quote, "496");
        assert_eq!(view.asks[1].depth_base, "4");
        assert_eq!(view.asks[1].depth_quote, "409");
    }

    #[test]
    fn book_keeps_every_level_from_a_deep_snapshot() {
        let bids = (1..=150)
            .map(|price| json!([price.to_string(), "1"]))
            .collect::<Vec<_>>();
        let asks = (151..=300)
            .map(|price| json!([price.to_string(), "1"]))
            .collect::<Vec<_>>();
        let mut book = BookAccumulator::default();
        assert!(book.replace(&json!(bids), &json!(asks), Some(1)));
        assert_eq!(book.book(1).bids.len(), 150);
        assert_eq!(book.book(1).asks.len(), 150);
        book.retain_top(100);
        assert_eq!(book.book(1).bids.len(), 100);
        assert_eq!(book.book(1).asks.len(), 100);
        assert_eq!(book.book(1).bids.last().unwrap().price, "51");
        assert_eq!(book.book(1).asks.last().unwrap().price, "250");
    }

    #[test]
    fn binance_snapshot_replays_buffered_updates_and_retries_stale_snapshot() {
        let snapshot =
            json!({"lastUpdateId":100,"bids":[["100","2"],["99","1"]],"asks":[["101","2"]]});
        let update = json!({"U":101,"u":102,"b":[["100","0"],["98","3"]],"a":[["101","4"]]});
        let mut buffered = VecDeque::from([update.clone()]);
        let mut book = BookAccumulator::default();
        assert!(
            initialize_deep_book(
                Exchange::Binance,
                MarketKind::Spot,
                &snapshot,
                &mut buffered,
                &mut book
            )
            .unwrap()
        );
        assert_eq!(book.sequence, Some(102));
        assert_eq!(
            book.book(1)
                .bids
                .iter()
                .map(|level| level.price.as_str())
                .collect::<Vec<_>>(),
            ["99", "98"]
        );
        assert_eq!(book.book(1).asks[0].size, "4");
        assert!(buffered.is_empty());

        let mut behind = VecDeque::from([update]);
        let old = json!({"lastUpdateId":90,"bids":[["100","2"]],"asks":[["101","2"]]});
        assert!(
            !initialize_deep_book(
                Exchange::Binance,
                MarketKind::Spot,
                &old,
                &mut behind,
                &mut book
            )
            .unwrap()
        );
        assert_eq!(behind.len(), 1);
    }

    #[test]
    fn binance_perpetual_book_checks_previous_update_id() {
        let snapshot = json!({"lastUpdateId":100,"bids":[["100","2"]],"asks":[["101","2"]]});
        let first = json!({"U":99,"u":101,"pu":98,"b":[["100","3"]],"a":[]});
        let mut buffered = VecDeque::from([first]);
        let mut book = BookAccumulator::default();
        assert!(
            initialize_deep_book(
                Exchange::Binance,
                MarketKind::Perp,
                &snapshot,
                &mut buffered,
                &mut book
            )
            .unwrap()
        );
        assert_eq!(book.sequence, Some(101));
        let next = json!({"U":102,"u":103,"pu":101,"b":[],"a":[["101","4"]]});
        assert!(apply_deep_delta(Exchange::Binance, MarketKind::Perp, &next, &mut book).unwrap());
        let gap = json!({"U":104,"u":105,"pu":102,"b":[],"a":[]});
        assert!(apply_deep_delta(Exchange::Binance, MarketKind::Perp, &gap, &mut book).is_err());
        assert_eq!(book.book(1).asks[0].size, "4");
    }

    #[test]
    fn gate_perpetual_contract_sizes_are_converted_to_base() {
        let trade = json!({"channel":"futures.trades","event":"update","result":[
            {"price":"100","size":"-25","create_time_ms":1700000000000_i64}
        ]});
        let trade = &parse_trades(Exchange::Gate, &trade)[0];
        assert_eq!(trade.side, TradeSide::Sell);
        assert_eq!(trade.size, "25");
        let mut book = BookAccumulator::default();
        assert!(book.replace(&json!([["100", "25"]]), &json!([["101", "10"]]), Some(1)));
        let scaled = book.book_scaled(1, Decimal::from_str("0.01").unwrap());
        assert_eq!(scaled.bids[0].size, "0.25");
        assert_eq!(scaled.bids[0].quote_size, "25");
    }

    #[test]
    fn bybit_full_book_requires_contiguous_updates() {
        let snapshot = json!({"retCode":0,"result":{"u":10,"b":[["100","1"]],"a":[["101","1"]]}});
        let update = json!({"data":{"u":11,"b":[["100","2"]],"a":[]}});
        let mut buffered = VecDeque::from([update]);
        let mut book = BookAccumulator::default();
        assert!(
            initialize_deep_book(
                Exchange::Bybit,
                MarketKind::Spot,
                &snapshot,
                &mut buffered,
                &mut book
            )
            .unwrap()
        );
        assert_eq!(book.book(1).bids[0].size, "2");
        let gap = json!({"data":{"u":13,"b":[],"a":[]}});
        assert!(apply_deep_delta(Exchange::Bybit, MarketKind::Spot, &gap, &mut book).is_err());
    }

    #[test]
    fn gate_gap_must_not_leave_a_crossed_phantom_level() {
        let mut book = BookAccumulator::default();
        let snapshot = json!({"full":true,"u":100,"b":[["0.6201","10"]],"a":[["0.6203","10"]]});
        assert!(apply_gate_obu(&snapshot, &mut book).unwrap());
        // Update 101 removed the old ask, but it was missed. Applying update 102
        // alone would leave that ask below the new bid.
        let after_gap = json!({"U":102,"u":102,"b":[["0.6205","10"]],"a":[["0.6207","10"]]});
        assert!(apply_gate_obu(&after_gap, &mut book).is_err());
    }

    #[test]
    fn gate_crossed_update_requires_a_fresh_snapshot() {
        let mut book = BookAccumulator::default();
        let snapshot = json!({"full":true,"u":100,"b":[["0.6201","10"]],"a":[["0.6203","10"]]});
        assert!(apply_gate_obu(&snapshot, &mut book).unwrap());
        let crossed = json!({"U":101,"u":101,"b":[["0.6205","10"]],"a":[]});
        assert!(apply_gate_obu(&crossed, &mut book).is_err());
    }

    #[test]
    fn gate_one_sided_updates_remove_old_levels_and_advance_sequence() {
        let mut book = BookAccumulator::default();
        let snapshot = json!({"full":true,"u":100,"b":[["0.6201","10"]],"a":[["0.6203","10"]]});
        assert!(apply_gate_obu(&snapshot, &mut book).unwrap());
        let asks_only = json!({"U":101,"u":101,"b":null,"a":[["0.6203","0"],["0.6207","10"]]});
        assert!(apply_gate_obu(&asks_only, &mut book).unwrap());
        let bids_only = json!({"U":102,"u":102,"b":[["0.6205","10"]],"a":null});
        assert!(apply_gate_obu(&bids_only, &mut book).unwrap());
        let no_levels = json!({"U":103,"u":103,"b":null,"a":null});
        assert!(!apply_gate_obu(&no_levels, &mut book).unwrap());
        assert_eq!(book.sequence, Some(103));
        assert_eq!(book.book(1).bids[0].price, "0.6205");
        assert_eq!(book.book(1).asks[0].price, "0.6207");
        assert!(!book.is_crossed());
    }

    #[test]
    fn parses_hyperliquid_candle() {
        let value = json!({"t": 1_700_000_000_000_i64, "o":"100", "h":"105", "l":"99", "c":"102", "v":"42"});
        let candle = parse_candle(Exchange::Hyperliquid, MarketKind::Perp, &value).unwrap();
        assert_eq!(candle.time, 1_700_000_000);
        assert_eq!(candle.close, 102.0);
    }

    #[test]
    fn parses_gate_and_lighter_market_data() {
        let gate_trade = json!({"channel":"spot.trades","event":"update","result":{
            "price":"100.25","amount":"0.75","create_time_ms":"1700000000123.456"
        }});
        assert_eq!(
            parse_trades(Exchange::Gate, &gate_trade)[0].time_ms,
            1_700_000_000_123
        );
        let lighter_trades = json!({"type":"update/trade","trades":[
            {"price":"99","size":"1","timestamp":1700000000122_i64},
            {"price":"100.25","size":"0.75","timestamp":1700000000123_i64}
        ]});
        assert_eq!(parse_trades(Exchange::Lighter, &lighter_trades).len(), 2);
        let mut lighter_snapshot = lighter_trades.clone();
        lighter_snapshot["type"] = json!("subscribed/trade");
        assert_eq!(parse_trades(Exchange::Lighter, &lighter_snapshot).len(), 2);
        let gate_quote = json!({"channel":"spot.book_ticker","event":"update","result":{
            "b":"100","a":"101","t":1700000000123_i64
        }});
        assert_eq!(parse_gate_bbo(&gate_quote).unwrap().ask, "101");
        let lighter_quote = json!({"type":"update/ticker","timestamp":1700000000123_i64,
            "ticker":{"b":{"price":"100"},"a":{"price":"101"}}});
        assert_eq!(parse_lighter_bbo(&lighter_quote).unwrap().bid, "100");
        let mut lighter_ticker_snapshot = lighter_quote.clone();
        lighter_ticker_snapshot["type"] = json!("subscribed/ticker");
        assert_eq!(
            parse_lighter_bbo(&lighter_ticker_snapshot).unwrap().ask,
            "101"
        );
        let gate_candle = json!(["1700000000", "400", "101", "102", "99", "100", "4"]);
        let candle = parse_candle(Exchange::Gate, MarketKind::Spot, &gate_candle).unwrap();
        assert_eq!(
            (candle.time, candle.open, candle.close, candle.volume),
            (1_700_000_000, 100.0, 101.0, 4.0)
        );
        let lighter_candle = json!({"t":1700000000000_i64,"o":100,"h":102,"l":99,"c":101,"v":4});
        assert_eq!(
            parse_candle(Exchange::Lighter, MarketKind::Perp, &lighter_candle)
                .unwrap()
                .time,
            1_700_000_000
        );
        let mut book = BookAccumulator::default();
        assert!(book.replace(
            &json!([{"price":"100","size":"2"}]),
            &json!([{"price":"101","size":"3"}]),
            Some(1)
        ));
        assert!(book.update(
            &json!([{"price":"100","size":"0"}]),
            &json!([{"price":"102","size":"1"}])
        ));
        assert!(book.book(1).bids.is_empty());
    }
}
