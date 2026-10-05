//! Public KuCoin, Kraken and Pacifica market data. Sizes exposed to the UI are base units.
use super::*;
use terminal_core::SymbolInfo;

pub(crate) fn handles(exchange: Exchange) -> bool {
    matches!(
        exchange,
        Exchange::Kucoin | Exchange::Kraken | Exchange::Pacifica
    )
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.is_number().then(|| value.to_string()))
}

pub(crate) async fn catalog(
    exchange: Exchange,
    kind: MarketKind,
    client: &crate::http::Client,
) -> Result<Vec<SymbolInfo>, Error> {
    let url = match (exchange, kind) {
        (Exchange::Kucoin, MarketKind::Spot) => "https://api.kucoin.com/api/v2/symbols",
        (Exchange::Kucoin, MarketKind::Perp) => {
            "https://api-futures.kucoin.com/api/v1/contracts/active"
        }
        (Exchange::Kraken, MarketKind::Spot) => {
            "https://api.kraken.com/0/public/AssetPairs?assetVersion=1"
        }
        (Exchange::Kraken, MarketKind::Perp) => {
            "https://futures.kraken.com/derivatives/api/v3/instruments"
        }
        (Exchange::Pacifica, _) => "https://api.pacifica.fi/api/v1/info",
        _ => unreachable!(),
    };
    let value = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    parse_catalog(exchange, kind, &value)
}

fn parse_catalog(
    exchange: Exchange,
    kind: MarketKind,
    value: &Value,
) -> Result<Vec<SymbolInfo>, Error> {
    let mut result = Vec::new();
    if exchange == Exchange::Kraken && kind == MarketKind::Spot {
        for (symbol, row) in value["result"]
            .as_object()
            .ok_or("invalid Kraken catalog")?
        {
            if row["status"] != "online" || !symbol.contains('/') {
                continue;
            }
            let Some((base, quote)) = symbol.split_once('/') else {
                continue;
            };
            result.push(SymbolInfo {
                symbol: symbol.clone(),
                base: base.into(),
                quote: quote.into(),
                base_token_id: None,
                market_id: None,
                size_multiplier: None,
                price_step: text(&row["tick_size"]),
            });
        }
    } else {
        let rows = &value[if exchange == Exchange::Kraken {
            "instruments"
        } else {
            "data"
        }];
        for row in rows.as_array().ok_or("invalid exchange catalog")? {
            let Some(symbol) = row["symbol"].as_str() else {
                continue;
            };
            let (enabled, base, quote, multiplier, step) = match (exchange, kind) {
                (Exchange::Kucoin, MarketKind::Spot) => (
                    row["enableTrading"] == true,
                    row["baseCurrency"].as_str(),
                    row["quoteCurrency"].as_str(),
                    None,
                    text(&row["priceIncrement"]),
                ),
                (Exchange::Kucoin, MarketKind::Perp) => (
                    row["status"] == "Open"
                        && row["isInverse"] == false
                        && row["expireDate"].is_null(),
                    row["baseCurrency"].as_str(),
                    row["quoteCurrency"].as_str(),
                    text(&row["multiplier"]),
                    text(&row["tickSize"]),
                ),
                (Exchange::Kraken, MarketKind::Perp) => (
                    symbol.starts_with("PF_")
                        && row["tradeable"] == true
                        && row["isExpired"] != true,
                    row["base"].as_str(),
                    row["quote"].as_str(),
                    None,
                    text(&row["tickSize"]),
                ),
                (Exchange::Pacifica, _) => (
                    if kind == MarketKind::Spot {
                        row["instrument_type"] == "spot"
                    } else {
                        row["instrument_type"] != "spot"
                    },
                    row["base_asset"].as_str().or(Some(symbol)),
                    Some("USDC"),
                    None,
                    text(&row["tick_size"]),
                ),
                _ => unreachable!(),
            };
            if !enabled {
                continue;
            }
            let (Some(base), Some(quote)) = (base, quote) else {
                continue;
            };
            result.push(SymbolInfo {
                symbol: symbol.into(),
                base: if base == "XBT" {
                    "BTC".into()
                } else {
                    base.into()
                },
                quote: quote.into(),
                base_token_id: None,
                market_id: None,
                size_multiplier: multiplier,
                price_step: step,
            });
        }
    }
    result.sort_unstable_by(|a, b| a.symbol.cmp(&b.symbol));
    result.dedup_by(|a, b| a.symbol == b.symbol);
    if result.is_empty() {
        return Err("no active markets returned by exchange".into());
    }
    Ok(result)
}

async fn multiplier(market: &Market, state: &AppState) -> Result<Decimal, Error> {
    if market.exchange != Exchange::Kucoin || market.kind != MarketKind::Perp {
        return Ok(Decimal::ONE);
    }
    state
        .symbols(market.exchange, market.kind)
        .await
        .map_err(std::io::Error::other)?
        .into_iter()
        .find(|row| row.symbol == market.symbol)
        .and_then(|row| row.size_multiplier)
        .and_then(|value| Decimal::from_str(&value).ok())
        .filter(|value| *value > Decimal::ZERO)
        .ok_or_else(|| "missing KuCoin contract multiplier".into())
}

pub(crate) async fn candles(
    market: &Market,
    state: &AppState,
    count: usize,
) -> Result<Vec<Candle>, Error> {
    let end = now_ms();
    let start = end - count as i64 * 60_000;
    let symbol = market.symbol.as_str();
    let client = &state.http;
    let request = match (market.exchange, market.kind) {
        (Exchange::Kucoin, MarketKind::Spot) => client
            .get("https://api.kucoin.com/api/v1/market/candles")
            .query(&[
                ("symbol", symbol.to_owned()),
                ("type", "1min".into()),
                ("startAt", (start / 1000).to_string()),
                ("endAt", (end / 1000).to_string()),
            ]),
        (Exchange::Kucoin, MarketKind::Perp) => client
            .get("https://api-futures.kucoin.com/api/v1/kline/query")
            .query(&[
                ("symbol", symbol.to_owned()),
                ("granularity", "1".into()),
                ("from", start.to_string()),
                ("to", end.to_string()),
            ]),
        (Exchange::Kraken, MarketKind::Spot) => {
            client.get("https://api.kraken.com/0/public/OHLC").query(&[
                ("pair", symbol.to_owned()),
                ("interval", "1".into()),
                ("since", (start / 1000).to_string()),
            ])
        }
        (Exchange::Kraken, MarketKind::Perp) => client
            .get(format!(
                "https://futures.kraken.com/api/charts/v1/trade/{symbol}/1m"
            ))
            .query(&[
                ("from", (start / 1000).to_string()),
                ("to", (end / 1000).to_string()),
            ]),
        (Exchange::Pacifica, _) => client.get("https://api.pacifica.fi/api/v1/kline").query(&[
            ("symbol", symbol.to_owned()),
            ("interval", "1m".into()),
            ("start_time", start.to_string()),
            ("end_time", end.to_string()),
            ("limit", count.to_string()),
        ]),
        _ => unreachable!(),
    };
    let value: Value = request.send().await?.error_for_status()?.json().await?;
    let rows = match (market.exchange, market.kind) {
        (Exchange::Kraken, MarketKind::Spot) => value["result"].as_object().and_then(|map| {
            map.iter()
                .find(|(key, _)| key.as_str() != "last")
                .map(|(_, rows)| rows)
        }),
        (Exchange::Kraken, MarketKind::Perp) => Some(&value["candles"]),
        _ => Some(&value["data"]),
    }
    .and_then(Value::as_array)
    .ok_or("invalid candle response")?;
    let scale = multiplier(market, state).await?.to_f64().unwrap_or(1.0);
    let mut result: Vec<_> = rows
        .iter()
        .filter_map(|row| parse_candle(market.exchange, market.kind, row))
        .collect();
    for candle in &mut result {
        candle.volume *= scale;
    }
    result.sort_unstable_by_key(|c| c.time);
    result.dedup_by_key(|c| c.time);
    Ok(result)
}

fn parse_candle(exchange: Exchange, kind: MarketKind, row: &Value) -> Option<Candle> {
    let (time, open, high, low, close, volume) = match (exchange, kind) {
        (Exchange::Kucoin, MarketKind::Spot) => (
            integer(&row[0])?,
            number(&row[1])?,
            number(&row[3])?,
            number(&row[4])?,
            number(&row[2])?,
            number(&row[5])?,
        ),
        (Exchange::Kucoin, MarketKind::Perp) => (
            integer(&row[0])? / 1000,
            number(&row[1])?,
            number(&row[2])?,
            number(&row[3])?,
            number(&row[4])?,
            number(&row[5])?,
        ),
        (Exchange::Kraken, MarketKind::Spot) => (
            integer(&row[0])?,
            number(&row[1])?,
            number(&row[2])?,
            number(&row[3])?,
            number(&row[4])?,
            number(&row[6])?,
        ),
        (Exchange::Kraken, MarketKind::Perp) => (
            integer(&row["time"])? / 1000,
            number(&row["open"])?,
            number(&row["high"])?,
            number(&row["low"])?,
            number(&row["close"])?,
            number(&row["volume"])?,
        ),
        (Exchange::Pacifica, _) => (
            integer(&row["t"])? / 1000,
            number(&row["o"])?,
            number(&row["h"])?,
            number(&row["l"])?,
            number(&row["c"])?,
            number(&row["v"])?,
        ),
        _ => return None,
    };
    Some(Candle {
        time,
        open,
        high,
        low,
        close,
        volume,
    })
}

fn trade(
    row: &Value,
    price: &str,
    size: &str,
    time_ms: i64,
    side: TradeSide,
    scale: Decimal,
) -> Option<Trade> {
    let price = decimal(&row[price])?;
    let size = decimal(&row[size])? * scale;
    if price <= Decimal::ZERO || size <= Decimal::ZERO {
        return None;
    }
    Some(Trade {
        price: price.normalize().to_string(),
        size: size.normalize().to_string(),
        time_ms,
        side,
    })
}

fn side(value: &Value) -> TradeSide {
    match value.as_str() {
        Some("buy" | "open_long" | "close_short") => TradeSide::Buy,
        Some("sell" | "open_short" | "close_long") => TradeSide::Sell,
        _ => TradeSide::Unknown,
    }
}

fn parse_trades(market: &Market, value: &Value, scale: Decimal) -> Vec<Trade> {
    match (market.exchange, market.kind) {
        (Exchange::Kucoin, _)
            if value["T"]
                .as_str()
                .is_some_and(|topic| topic.starts_with("trade.")) =>
        {
            let row = &value["d"];
            trade(
                row,
                "p",
                "q",
                integer(&row["M"]).unwrap_or_else(|| now_ms() * 1_000_000) / 1_000_000,
                side(&row["S"]),
                scale,
            )
            .into_iter()
            .collect()
        }
        (Exchange::Kraken, MarketKind::Spot)
            if value["channel"] == "trade" && value["type"] == "update" =>
        {
            value["data"].as_array().map_or_else(Vec::new, |rows| {
                rows.iter()
                    .filter_map(|row| {
                        trade(
                            row,
                            "price",
                            "qty",
                            timestamp_ms(&row["timestamp"]),
                            side(&row["side"]),
                            scale,
                        )
                    })
                    .collect()
            })
        }
        (Exchange::Kraken, MarketKind::Perp) if value["feed"] == "trade" => trade(
            value,
            "price",
            "qty",
            integer(&value["time"]).unwrap_or_else(now_ms),
            side(&value["side"]),
            scale,
        )
        .into_iter()
        .collect(),
        (Exchange::Pacifica, _) if value["channel"] == "trades" => {
            value["data"].as_array().map_or_else(Vec::new, |rows| {
                rows.iter()
                    .filter_map(|row| {
                        trade(
                            row,
                            "p",
                            "a",
                            integer(&row["t"]).unwrap_or_else(now_ms),
                            side(&row["d"]),
                            scale,
                        )
                    })
                    .collect()
            })
        }
        _ => Vec::new(),
    }
}

// Kraken timestamps are UTC RFC3339. No date/time dependency is needed for this fixed format.
fn timestamp_ms(value: &Value) -> i64 {
    fn parse(text: &str) -> Option<i64> {
        let year: i64 = text.get(0..4)?.parse().ok()?;
        let month: i64 = text.get(5..7)?.parse().ok()?;
        let day: i64 = text.get(8..10)?.parse().ok()?;
        let hour: i64 = text.get(11..13)?.parse().ok()?;
        let minute: i64 = text.get(14..16)?.parse().ok()?;
        let second: i64 = text.get(17..19)?.parse().ok()?;
        let adjusted_year = year - i64::from(month <= 2);
        let era = adjusted_year.div_euclid(400);
        let y = adjusted_year - era * 400;
        let m = month + if month > 2 { -3 } else { 9 };
        let days = era * 146097 + y * 365 + y / 4 - y / 100 + (153 * m + 2) / 5 + day - 1 - 719468;
        let fraction = text.get(19..)?.strip_prefix('.').unwrap_or("");
        let millis: String = fraction
            .chars()
            .take_while(char::is_ascii_digit)
            .take(3)
            .chain(std::iter::repeat('0'))
            .take(3)
            .collect();
        Some(
            ((days * 24 + hour) * 60 + minute) * 60_000
                + second * 1000
                + millis.parse::<i64>().ok()?,
        )
    }
    value.as_str().and_then(parse).unwrap_or_else(now_ms)
}

// Normalize venue object rows to decimal pairs.
fn levels(rows: &Value, size_key: &str) -> Value {
    Value::Array(rows.as_array().map_or_else(Vec::new, |rows| {
        rows.iter()
            .filter_map(|row| {
                Some(json!([
                    text(&row[if size_key == "a" { "p" } else { "price" }])?,
                    text(&row[size_key])?
                ]))
            })
            .collect()
    }))
}

fn kraken_checksum(book: &BookAccumulator) -> u32 {
    let mut crc = !0_u32;
    for (price, size) in book
        .asks
        .iter()
        .take(10)
        .chain(book.bids.iter().rev().take(10))
    {
        for decimal in [price, size] {
            let digits = decimal.to_string().replace('.', "");
            for byte in digits.trim_start_matches('0').bytes() {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ if crc & 1 == 1 { 0xedb88320 } else { 0 };
                }
            }
        }
    }
    !crc
}

// Preserve Kraken's numeric spelling only at the exchange boundary. Global
// arbitrary_precision breaks f64 fields buffered by internally tagged Serde enums.
#[derive(serde::Deserialize)]
struct KrakenBookFrame {
    data: Vec<KrakenBookRows>,
}

#[derive(serde::Deserialize)]
struct KrakenBookRows {
    #[serde(default)]
    bids: Vec<KrakenRawLevel>,
    #[serde(default)]
    asks: Vec<KrakenRawLevel>,
}

#[derive(serde::Deserialize)]
struct KrakenRawLevel {
    price: Box<serde_json::value::RawValue>,
    qty: Box<serde_json::value::RawValue>,
}

fn kraken_levels(rows: &[KrakenRawLevel]) -> Value {
    Value::Array(
        rows.iter()
            .map(|row| json!([row.price.get(), row.qty.get()]))
            .collect(),
    )
}

async fn publish(
    market: &Market,
    state: &AppState,
    book: &BookAccumulator,
    previous: Option<BookAccumulator>,
    stamp: i64,
    scale: Decimal,
) -> Result<(), Error> {
    if !book.ready || book.is_crossed() {
        return Err("invalid or crossed order book; resubscribing".into());
    }
    let current = book.book_scaled(stamp, scale);
    if let Some(previous) = previous {
        let bids = changed_levels(&previous.bids, &book.bids, scale);
        let asks = changed_levels(&previous.asks, &book.asks, scale);
        if !bids.is_empty() || !asks.is_empty() {
            state.publish_book_delta(market, current, bids, asks).await;
        }
    } else {
        state.publish_book(market, current).await;
    }
    Ok(())
}

pub(crate) async fn stream(market: &Market, state: &AppState) -> Result<(), Error> {
    if market.exchange == Exchange::Kucoin {
        return kucoin(market, state).await;
    }
    let scale = Decimal::ONE;
    let url = match (market.exchange, market.kind) {
        (Exchange::Kraken, MarketKind::Spot) => "wss://ws.kraken.com/v2",
        (Exchange::Kraken, MarketKind::Perp) => "wss://futures.kraken.com/ws/v1",
        (Exchange::Pacifica, _) => "wss://ws.pacifica.fi/ws",
        _ => unreachable!(),
    };
    let (mut socket, _) = connect_public(url).await?;
    for channel in ["book", "trade"] {
        let subscription = match (market.exchange, market.kind) {
            (Exchange::Kraken, MarketKind::Spot) => {
                if channel == "book" {
                    json!({"method":"subscribe","params":{"channel":"book","symbol":[market.symbol],"depth":1000,"snapshot":true}})
                } else {
                    json!({"method":"subscribe","params":{"channel":"trade","symbol":[market.symbol],"snapshot":false}})
                }
            }
            (Exchange::Kraken, MarketKind::Perp) => {
                json!({"event":"subscribe","feed":channel,"product_ids":[market.symbol]})
            }
            (Exchange::Pacifica, _) => {
                if channel == "book" {
                    json!({"method":"subscribe","params":{"source":"book","symbol":market.symbol,"agg_level":1}})
                } else {
                    json!({"method":"subscribe","params":{"source":"trades","symbol":market.symbol}})
                }
            }
            _ => unreachable!(),
        };
        socket
            .send(Message::Text(subscription.to_string().into()))
            .await?;
    }
    if market.exchange == Exchange::Pacifica {
        socket
            .send(Message::Text(
                json!({"method":"subscribe","params":{"source":"bbo","symbol":market.symbol}})
                    .to_string()
                    .into(),
            ))
            .await?;
    }
    let mut book = BookAccumulator::default();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    let mut last_received = tokio::time::Instant::now();
    heartbeat.tick().await;
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if last_received.elapsed() > Duration::from_secs(45) { return Err("market feed timed out".into()); }
                if market.exchange == Exchange::Pacifica { socket.send(Message::Text(json!({"method":"ping"}).to_string().into())).await?; }
                else { socket.send(Message::Ping(Vec::new().into())).await?; }
            }
            message = socket.next() => {
                let Some(message) = message else { return Err("WebSocket closed".into()); };
                last_received = tokio::time::Instant::now();
                let raw = match message? {
                    Message::Text(text) => text,
                    Message::Ping(data) => { socket.send(Message::Pong(data)).await?; continue; }
                    Message::Close(_) => return Err("WebSocket closed".into()),
                    _ => continue,
                };
                let value: Value = serde_json::from_str(&raw)?;
                if value["success"] == false || value["event"] == "error" || value["event"] == "subscribed_failed" { return Err(format!("subscription rejected: {value}").into()); }
                if value["event"].is_string() { continue; }
                let trades = parse_trades(market,&value,scale);
                if !trades.is_empty() { state.publish_trades(market,trades).await; }
                if market.exchange == Exchange::Pacifica && value["channel"] == "bbo" {
                    let row = &value["data"];
                    if let (Some(bid),Some(ask)) = (decimal(&row["b"]),decimal(&row["a"])) {
                        if bid > Decimal::ZERO && ask > bid { state.publish_best_bid_ask(market,BestBidAsk { bid: bid.normalize().to_string(), ask: ask.normalize().to_string(),time_ms:integer(&row["t"]).unwrap_or_else(now_ms) }).await; }
                    }
                }
                let is_book = value["channel"] == "book" || value["feed"] == "book" || value["feed"] == "book_snapshot";
                if !is_book { continue; }
                let previous = book.ready.then(|| book.clone());
                let stamp;
                match (market.exchange,market.kind) {
                    (Exchange::Kraken,MarketKind::Spot) if value["channel"] == "book" => {
                        let row = &value["data"][0];
                        let precise: KrakenBookFrame = serde_json::from_str(&raw)?;
                        let precise = precise.data.first().ok_or("missing Kraken book data")?;
                        let bids = kraken_levels(&precise.bids); let asks = kraken_levels(&precise.asks);
                        if value["type"] == "snapshot" { book.replace(&bids,&asks,None); }
                        else if value["type"] == "update" { if !book.update(&bids,&asks) { return Err("Kraken update before snapshot".into()); } }
                        else { continue; }
                        book.retain_top(1000);
                        if row["checksum"].as_u64().is_some_and(|crc| crc != u64::from(kraken_checksum(&book))) { return Err("Kraken book checksum mismatch".into()); }
                        stamp = timestamp_ms(&row["timestamp"]);
                    }
                    (Exchange::Kraken,MarketKind::Perp) if value["feed"] == "book_snapshot" || value["feed"] == "book" => {
                        let seq = integer(&value["seq"]).ok_or("missing Kraken sequence")?;
                        if value["feed"] == "book_snapshot" { book.replace(&levels(&value["bids"],"qty"),&levels(&value["asks"],"qty"),Some(seq)); }
                        else {
                            if book.sequence.is_some_and(|last| seq <= last) { continue; }
                            if book.sequence != Some(seq-1) { return Err("Kraken book sequence gap".into()); }
                            let row = json!([[text(&value["price"]).ok_or("missing book price")?,text(&value["qty"]).ok_or("missing book size")?]]);
                            let empty = json!([]);
                            match value["side"].as_str() { Some("buy") => { book.update(&row,&empty); },Some("sell") => { book.update(&empty,&row); },_ => return Err("invalid Kraken book side".into()) }
                            book.sequence = Some(seq);
                        }
                        stamp = integer(&value["timestamp"]).unwrap_or_else(now_ms);
                    }
                    (Exchange::Pacifica,_) if value["channel"] == "book" => {
                        let row = &value["data"];
                        if !row["l"][0].is_array() || !row["l"][1].is_array() { return Err("invalid Pacifica snapshot".into()); }
                        book.replace(&levels(&row["l"][0],"a"),&levels(&row["l"][1],"a"),None);
                        stamp = integer(&row["t"]).unwrap_or_else(now_ms);
                    }
                    _ => continue,
                }
                publish(market,state,&book,previous,stamp,scale).await?;
            }
        }
    }
}

async fn kucoin(market: &Market, state: &AppState) -> Result<(), Error> {
    let url = if market.kind == MarketKind::Spot {
        "wss://x-push-spot.kucoin.com"
    } else {
        "wss://x-push-futures.kucoin.com"
    };
    let ((mut socket, _), scale) =
        tokio::try_join!(connect_public(url), multiplier(market, state))?;
    let welcome = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(message) = socket.next().await {
            let value: Value = match message? {
                Message::Text(text) => serde_json::from_str(&text)?,
                Message::Binary(bytes) => serde_json::from_slice(&bytes)?,
                Message::Ping(data) => {
                    socket.send(Message::Pong(data)).await?;
                    continue;
                }
                Message::Close(_) => return Err("KuCoin closed before welcome".into()),
                _ => continue,
            };
            if value["message"] == "welcome" {
                return Ok::<Value, Error>(value);
            }
        }
        Err("KuCoin closed before welcome".into())
    })
    .await??;
    let kind = if market.kind == MarketKind::Spot {
        "SPOT"
    } else {
        "FUTURES"
    };
    socket.send(Message::Text(json!({"id":"book","action":"subscribe","channel":"obu","tradeType":kind,"symbol":market.symbol,"depth":"increment@10ms","rpiFilter":0}).to_string().into())).await?;
    socket.send(Message::Text(json!({"id":"trade","action":"subscribe","channel":"trade","tradeType":kind,"symbol":market.symbol}).to_string().into())).await?;
    let interval = Duration::from_millis(
        integer(&welcome["pingInterval"]).unwrap_or(18000).max(1000) as u64 / 2,
    );
    let mut heartbeat = tokio::time::interval(interval);
    let mut last_pong = tokio::time::Instant::now();
    let mut book = BookAccumulator::default();
    heartbeat.tick().await;
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if last_pong.elapsed() > interval*3 + Duration::from_secs(10) { return Err("KuCoin heartbeat timed out".into()); }
                socket.send(Message::Text(json!({"id":now_ms().to_string(),"op":"ping"}).to_string().into())).await?;
            }
            message = socket.next() => {
                let Some(message) = message else { return Err("KuCoin WebSocket closed".into()); };
                let value: Value = match message? {
                    Message::Text(text) => serde_json::from_str(&text)?,
                    Message::Binary(bytes) => serde_json::from_slice(&bytes)?,
                    Message::Ping(data) => { socket.send(Message::Pong(data)).await?; continue; }
                    Message::Close(_) => return Err("KuCoin WebSocket closed".into()),
                    _ => continue,
                };
                if value["result"] == false || value["error"].is_object() { return Err(format!("KuCoin subscription rejected: {value}").into()); }
                if value["op"] == "pong" { last_pong = tokio::time::Instant::now(); }
                let trades = parse_trades(market,&value,scale);
                if !trades.is_empty() { state.publish_trades(market,trades).await; }
                if !value["T"].as_str().is_some_and(|topic| topic.starts_with("obu.")) { continue; }
                let previous = book.ready.then(|| book.clone());
                if apply_kucoin(&mut book,&value)? {
                    let stamp = integer(&value["d"]["M"]).map(|ts| ts/1_000_000).unwrap_or_else(now_ms);
                    publish(market,state,&book,previous,stamp,scale).await?;
                }
            }
        }
    }
}

fn apply_kucoin(book: &mut BookAccumulator, value: &Value) -> Result<bool, Error> {
    let row = &value["d"];
    let end = integer(&row["C"]).ok_or("missing KuCoin update sequence")?;
    if value["t"] == "snapshot" {
        if !book.replace(&row["b"], &row["a"], Some(end)) {
            return Err("invalid KuCoin snapshot".into());
        }
    } else if value["t"] == "delta" {
        let last = book.sequence.ok_or("KuCoin delta before snapshot")?;
        if end <= last {
            return Ok(false);
        }
        let start = integer(&row["O"]).ok_or("missing KuCoin sequence start")?;
        if start > last + 1 {
            return Err("KuCoin book sequence gap".into());
        }
        if !book.update(&row["b"], &row["a"]) {
            return Err("invalid KuCoin delta".into());
        }
        book.sequence = Some(end);
    } else {
        return Ok(false);
    }
    // The feed explicitly deletes levels that leave its 500-level window.
    book.retain_top(500);
    if book.is_crossed() {
        return Err("crossed KuCoin book".into());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kucoin_book_deletions_stale_batches_and_gaps() {
        let mut book = BookAccumulator::default();
        book.replace(&json!([["100", "4"]]), &json!([["102", "2"]]), Some(10));
        let update =
            json!({"t":"delta","d":{"O":11,"C":12,"b":[["99","3"]],"a":[["102","0"],["103","5"]]}});
        assert!(apply_kucoin(&mut book, &update).unwrap());
        let current = book.book(0);
        assert_eq!(current.bids[0].size, "4");
        assert_eq!(current.bids[1].price, "99");
        assert_eq!(current.asks[0].price, "103");
        assert!(!apply_kucoin(&mut book, &update).unwrap());
        let gap = json!({"t":"delta","d":{"O":14,"C":14,"b":[],"a":[]}});
        assert!(apply_kucoin(&mut book, &gap).is_err());
        assert_eq!(book.sequence, Some(12));
    }

    #[test]
    fn kucoin_contracts_are_base_sizes_and_trades_use_nanoseconds() {
        let market = Market::for_exchange_kind(Exchange::Kucoin, MarketKind::Perp);
        let scale = Decimal::from_str("0.001").unwrap();
        let mut book = BookAccumulator::default();
        book.replace(&json!([["80000", "3"]]), &json!([["80001", "5"]]), Some(20));
        apply_kucoin(
            &mut book,
            &json!({"t":"delta","d":{"O":21,"C":21,"b":[["80000","12"]],"a":[]}}),
        )
        .unwrap();
        assert_eq!(book.book_scaled(0, scale).bids[0].size, "0.012");
        assert_eq!(book.book_scaled(0, scale).bids[0].quote_size, "960");
        let trades = parse_trades(
            &market,
            &json!({"T":"trade.FUTURES","d":{"p":"80000","q":"12","S":"sell","M":1770000000123456789_i64}}),
            scale,
        );
        assert_eq!(trades[0].size, "0.012");
        assert_eq!(trades[0].time_ms, 1770000000123);
        assert_eq!(trades[0].side, TradeSide::Sell);
    }

    #[test]
    fn kraken_numeric_checksum_preserves_trailing_zeroes() {
        // Kraken's published v2 CRC fixture, using numeric JSON exactly as on the wire.
        let value: KrakenBookRows = serde_json::from_str(
            r#"{"bids":[
            {"price":45283.5,"qty":0.10000000},{"price":45283.4,"qty":1.54582015},
            {"price":45282.1,"qty":0.10000000},{"price":45281.0,"qty":0.10000000},
            {"price":45280.3,"qty":1.54592586},{"price":45279.0,"qty":0.07990000},
            {"price":45277.6,"qty":0.03310103},{"price":45277.5,"qty":0.30000000},
            {"price":45277.3,"qty":1.54602737},{"price":45276.6,"qty":0.15445238}],
            "asks":[{"price":45285.2,"qty":0.00100000},{"price":45286.4,"qty":1.54571953},
            {"price":45286.6,"qty":1.54571109},{"price":45289.6,"qty":1.54560911},
            {"price":45290.2,"qty":0.15890660},{"price":45291.8,"qty":1.54553491},
            {"price":45294.7,"qty":0.04454749},{"price":45296.1,"qty":0.35380000},
            {"price":45297.5,"qty":0.09945542},{"price":45299.5,"qty":0.18772827}]}"#,
        )
        .unwrap();
        let mut book = BookAccumulator::default();
        book.replace(
            &kraken_levels(&value.bids),
            &kraken_levels(&value.asks),
            None,
        );
        assert_eq!(kraken_checksum(&book), 3310070434);
        book.update(&json!([["45283.5", "0.20000000"]]), &json!([]));
        assert_ne!(kraken_checksum(&book), 3310070434);
    }

    #[test]
    fn kraken_utc_timestamp_and_batch_trades_are_exact() {
        assert_eq!(timestamp_ms(&json!("1970-01-01T00:00:00.123456Z")), 123);
        assert_eq!(
            timestamp_ms(&json!("2024-02-29T12:34:56.789Z")),
            1709210096789
        );
        let market = Market::for_exchange_kind(Exchange::Kraken, MarketKind::Spot);
        let trades = parse_trades(
            &market,
            &json!({"channel":"trade","type":"update","data":[
            {"price":100,"qty":2,"side":"buy","timestamp":"2024-02-29T12:34:56.789Z"},
            {"price":99,"qty":3,"side":"sell","timestamp":"2024-02-29T12:34:56.790Z"}]}),
            Decimal::ONE,
        );
        assert_eq!(trades.len(), 2);
        assert_eq!(trades[0].side, TradeSide::Buy);
        assert_eq!(trades[1].side, TradeSide::Sell);
        assert_eq!(trades[1].time_ms - trades[0].time_ms, 1);
        assert!(
            parse_trades(
                &market,
                &json!({"channel":"trade","type":"snapshot","data":[]}),
                Decimal::ONE
            )
            .is_empty()
        );
    }

    #[test]
    fn catalogs_separate_spot_and_linear_perpetuals() {
        let data = json!({"data":[{"symbol":"SOL-USDC","instrument_type":"spot","base_asset":"SOL","tick_size":"0.01"},{"symbol":"kPEPE","instrument_type":"perpetual","tick_size":"0.000001"}]});
        let spot = parse_catalog(Exchange::Pacifica, MarketKind::Spot, &data).unwrap();
        let perp = parse_catalog(Exchange::Pacifica, MarketKind::Perp, &data).unwrap();
        assert_eq!(spot[0].symbol, "SOL-USDC");
        assert_eq!(perp[0].symbol, "kPEPE");
        let data = json!({"instruments":[{"symbol":"PF_XBTUSD","base":"BTC","quote":"USD","tradeable":true,"isExpired":false},{"symbol":"PI_XBTUSD","tradeable":true},{"symbol":"PF_ETHUSD","tradeable":false}]});
        assert_eq!(
            parse_catalog(Exchange::Kraken, MarketKind::Perp, &data)
                .unwrap()
                .len(),
            1
        );
        let data = json!({"data":[{"symbol":"XBTUSDTM","baseCurrency":"XBT","quoteCurrency":"USDT","status":"Open","isInverse":false,"expireDate":null,"multiplier":0.001},{"symbol":"XBTUSD","status":"Open","isInverse":true}]});
        let symbols = parse_catalog(Exchange::Kucoin, MarketKind::Perp, &data).unwrap();
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].base, "BTC");
        assert_eq!(symbols[0].size_multiplier.as_deref(), Some("0.001"));
    }

    #[test]
    fn pacifica_trade_directions_and_candle_units() {
        let market = Market::for_exchange_kind(Exchange::Pacifica, MarketKind::Spot);
        let trades = parse_trades(
            &market,
            &json!({"channel":"trades","data":[{"p":"90.5","a":"2","d":"open_long","t":1770000000000_i64,"it":0},{"p":"90.4","a":"3","d":"close_long","t":1770000000001_i64}]}),
            Decimal::ONE,
        );
        assert_eq!(trades[0].side, TradeSide::Buy);
        assert_eq!(trades[1].side, TradeSide::Sell);
        let candle = parse_candle(
            Exchange::Kucoin,
            MarketKind::Spot,
            &json!([1770000000, "10", "12", "13", "9", "20"]),
        )
        .unwrap();
        assert_eq!(
            (
                candle.open,
                candle.high,
                candle.low,
                candle.close,
                candle.volume
            ),
            (10., 13., 9., 12., 20.)
        );
        let candle = parse_candle(
            Exchange::Pacifica,
            MarketKind::Spot,
            &json!({"t":1770000000000_i64,"o":"90","h":"92","l":"89","c":"91","v":"6"}),
        )
        .unwrap();
        assert_eq!(candle.time, 1770000000);
        assert_eq!(candle.volume, 6.);
    }
}
