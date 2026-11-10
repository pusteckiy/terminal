//! Coinbase Exchange public Spot market data: full L2, individual matches and 1m candles.
use super::*;
use terminal_core::SymbolInfo;

const REST: &str = "https://api.exchange.coinbase.com";
const FEED: &str = "wss://ws-feed.exchange.coinbase.com";
static NEXT_CONNECTION: tokio::sync::Mutex<Option<tokio::time::Instant>> =
    tokio::sync::Mutex::const_new(None);

pub(crate) async fn catalog(
    kind: MarketKind,
    client: &crate::http::Client,
) -> Result<Vec<SymbolInfo>, Error> {
    if kind != MarketKind::Spot {
        return Err("Coinbase integration supports Spot markets only".into());
    }
    let value = client
        .get(format!("{REST}/products"))
        .header("User-Agent", "Terminal/0.1")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    parse_catalog(&value)
}

fn parse_catalog(value: &Value) -> Result<Vec<SymbolInfo>, Error> {
    let mut symbols = Vec::new();
    for row in value.as_array().ok_or("invalid Coinbase product catalog")? {
        if row["status"] != "online"
            || row["trading_disabled"] == true
            || row["cancel_only"] == true
            || row["auction_mode"] == true
        {
            continue;
        }
        let (Some(symbol), Some(base), Some(quote), Some(step)) = (
            row["id"].as_str(),
            row["base_currency"].as_str(),
            row["quote_currency"].as_str(),
            row["quote_increment"].as_str(),
        ) else {
            continue;
        };
        if base.is_empty()
            || quote.is_empty()
            || decimal(&row["quote_increment"]).is_none_or(|n| n <= Decimal::ZERO)
        {
            continue;
        }
        symbols.push(SymbolInfo {
            symbol: symbol.into(),
            base: base.into(),
            quote: quote.into(),
            price_step: Some(step.into()),
            base_token_id: None,
            market_id: None,
            size_multiplier: None,
        });
    }
    symbols.sort_unstable_by(|a, b| a.symbol.cmp(&b.symbol));
    symbols.dedup_by(|a, b| a.symbol == b.symbol);
    if symbols.is_empty() {
        return Err("Coinbase returned no active Spot products".into());
    }
    Ok(symbols)
}

fn timestamp(value: &Value) -> Option<i64> {
    let text = value.as_str()?;
    let seconds = parse_utc_seconds(text)?;
    let fraction = text
        .split_once('.')
        .map_or("", |(_, s)| s.trim_end_matches('Z'));
    let millis: String = fraction
        .chars()
        .take(3)
        .chain(std::iter::repeat('0'))
        .take(3)
        .collect();
    seconds.checked_mul(1000)?.checked_add(millis.parse().ok()?)
}

fn parse_trade(value: &Value) -> Option<Trade> {
    if value["type"] != "match" {
        return None;
    }
    let price = decimal(&value["price"])?;
    let size = decimal(&value["size"])?;
    if price <= Decimal::ZERO || size <= Decimal::ZERO {
        return None;
    }
    // Exchange match.side is the maker side; our trade colors represent the aggressor.
    let side = match value["side"].as_str()? {
        "sell" => TradeSide::Buy,
        "buy" => TradeSide::Sell,
        _ => return None,
    };
    Some(Trade {
        price: price.normalize().to_string(),
        size: size.normalize().to_string(),
        time_ms: timestamp(&value["time"])?,
        side,
    })
}

fn update_book(
    book: &mut BookAccumulator,
    value: &Value,
) -> Result<(Vec<BookChange>, Vec<BookChange>), Error> {
    let valid = |rows: &Value| {
        rows.as_array().is_some_and(|rows| {
            rows.iter().all(|row| {
                decimal(&row[0]).is_some_and(|n| n > Decimal::ZERO)
                    && decimal(&row[1]).is_some_and(|n| n >= Decimal::ZERO)
            })
        })
    };
    if value["type"] == "snapshot" {
        if !valid(&value["bids"])
            || !valid(&value["asks"])
            || !book.replace(&value["bids"], &value["asks"], None)
        {
            return Err("invalid Coinbase book snapshot".into());
        }
        if book.is_crossed() {
            return Err("crossed Coinbase snapshot; resubscribing".into());
        }
        return Ok((Vec::new(), Vec::new()));
    }
    let mut bids = Vec::new();
    let mut asks = Vec::new();
    for change in value["changes"]
        .as_array()
        .ok_or("invalid Coinbase book changes")?
    {
        let pair = json!([change[1], change[2]]);
        match change[0].as_str() {
            Some("buy") => bids.push(pair),
            Some("sell") => asks.push(pair),
            _ => return Err("invalid Coinbase book side".into()),
        }
    }
    let (bids, asks) = (Value::Array(bids), Value::Array(asks));
    if !valid(&bids) || !valid(&asks) || !book.update(&bids, &asks) {
        return Err("invalid Coinbase delta or delta before snapshot".into());
    }
    if book.is_crossed() {
        return Err("crossed Coinbase book; resubscribing".into());
    }
    Ok((
        book_changes(&bids, Decimal::ONE),
        book_changes(&asks, Decimal::ONE),
    ))
}

pub(crate) async fn stream(market: &Market, state: &AppState) -> Result<(), Error> {
    if market.kind != MarketKind::Spot {
        return Err("Coinbase integration supports Spot markets only".into());
    }
    // Share the per-IP connection budget across products; Coinbase allows 8 requests/s.
    let mut next = NEXT_CONNECTION.lock().await;
    if let Some(when) = *next {
        tokio::time::sleep_until(when).await;
    }
    *next = Some(tokio::time::Instant::now() + Duration::from_millis(200));
    drop(next);
    let (mut socket, _) = connect_public(FEED).await?;
    socket.send(Message::Text(json!({"type":"subscribe","product_ids":[market.symbol],"channels":["level2_batch","matches","heartbeat"]}).to_string().into())).await?;
    let mut book = BookAccumulator::default();
    let mut last_trade_id = None;
    let mut last_book_time = 0;
    let mut last_received = tokio::time::Instant::now();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    heartbeat.tick().await;
    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if last_received.elapsed() > Duration::from_secs(30) { return Err("Coinbase market feed timed out".into()); }
                socket.send(Message::Ping(Vec::new().into())).await?;
            }
            message = socket.next() => {
                let Some(message) = message else { return Err("Coinbase WebSocket closed".into()); };
                let raw = match message? {
                    Message::Text(raw) => raw,
                    Message::Ping(payload) => { socket.send(Message::Pong(payload)).await?; continue; }
                    Message::Close(_) => return Err("Coinbase WebSocket closed".into()),
                    _ => continue,
                };
                let value: Value = serde_json::from_str(&raw)?;
                if value["type"] == "error" { return Err(format!("Coinbase subscription rejected: {}",value["message"]).into()); }
                if value["product_id"].as_str() != Some(&market.symbol) { continue; }
                last_received = tokio::time::Instant::now();
                match value["type"].as_str() {
                    Some("match") => {
                        let id = value["trade_id"].as_u64().ok_or("missing Coinbase trade ID")?;
                        if last_trade_id.is_some_and(|last| id <= last) { continue; }
                        let trade = parse_trade(&value).ok_or("invalid Coinbase match")?;
                        last_trade_id = Some(id);
                        state.publish_trades(market,vec![trade]).await;
                    }
                    Some("snapshot" | "l2update") => {
                        let snapshot = value["type"] == "snapshot";
                        let stamp = if snapshot {
                            // Snapshots have no event time: receipt time could discard the first buffered deltas.
                            0
                        } else { timestamp(&value["time"]).ok_or("invalid Coinbase book timestamp")? };
                        if !snapshot && stamp < last_book_time { return Err("out-of-order Coinbase book; resubscribing".into()); }
                        let (bids,asks) = update_book(&mut book,&value)?;
                        last_book_time = stamp;
                        if !snapshot && bids.is_empty() && asks.is_empty() { continue; }
                        let current = book.book(stamp);
                        if snapshot { state.publish_book(market,current).await; }
                        else if !bids.is_empty() || !asks.is_empty() { state.publish_book_delta(market,current,bids,asks).await; }
                    }
                    // last_match is a historical seed, not a new execution.
                    _ => {}
                }
            }
        }
    }
}

pub(crate) async fn candles(
    market: &Market,
    client: &crate::http::Client,
    count: usize,
) -> Result<Vec<Candle>, Error> {
    if market.kind != MarketKind::Spot {
        return Err("Coinbase integration supports Spot markets only".into());
    }
    let count = count.clamp(1, 300);
    let end = (now_ms() / 60_000 + 1) * 60;
    let start = end - count as i64 * 60;
    let value: Value = client
        .get(format!("{REST}/products/{}/candles", market.symbol))
        .header("User-Agent", "Terminal/0.1")
        .query(&[
            ("granularity", "60".to_owned()),
            ("start", start.to_string()),
            ("end", end.to_string()),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    parse_candles(&value, start, end, count)
}

fn parse_candles(value: &Value, start: i64, end: i64, count: usize) -> Result<Vec<Candle>, Error> {
    let mut candles: Vec<_> = value
        .as_array()
        .ok_or("invalid Coinbase candles")?
        .iter()
        .filter_map(|row| {
            let time = integer(&row[0])?;
            let candle = Candle {
                time,
                low: number(&row[1])?,
                high: number(&row[2])?,
                open: number(&row[3])?,
                close: number(&row[4])?,
                volume: number(&row[5])?,
            };
            (time >= start
                && time < end
                && [candle.open, candle.high, candle.low, candle.close]
                    .iter()
                    .all(|p| p.is_finite() && *p > 0.0)
                && candle.volume.is_finite()
                && candle.volume >= 0.0
                && candle.low <= candle.open.min(candle.close)
                && candle.high >= candle.open.max(candle.close))
            .then_some(candle)
        })
        .collect();
    candles.sort_unstable_by_key(|c| c.time);
    candles.dedup_by_key(|c| c.time);
    let skip = candles.len().saturating_sub(count);
    Ok(candles.into_iter().skip(skip).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_filters_inactive_products_and_keeps_quote_units_and_ticks() {
        let product = |id: &str, base: &str, quote: &str| json!({"id":id,"base_currency":base,"quote_currency":quote,"quote_increment":"0.000001","status":"online","trading_disabled":false,"cancel_only":false,"auction_mode":false});
        let mut disabled = product("BAD-USD", "BAD", "USD");
        disabled["trading_disabled"] = json!(true);
        let mut cancelled = product("OLD-USD", "OLD", "USD");
        cancelled["cancel_only"] = json!(true);
        let mut offline = product("OFF-USD", "OFF", "USD");
        offline["status"] = json!("offline");
        let mut auction = product("NEW-USD", "NEW", "USD");
        auction["auction_mode"] = json!(true);
        let symbols = parse_catalog(&json!([
            product("BTC-USD", "BTC", "USD"),
            product("ETH-EUR", "ETH", "EUR"),
            product("BTC-USDC", "BTC", "USDC"),
            disabled,
            cancelled,
            offline,
            auction
        ]))
        .unwrap();
        assert_eq!(symbols.len(), 3);
        assert_eq!(
            (symbols[0].symbol.as_str(), symbols[0].quote.as_str()),
            ("BTC-USD", "USD")
        );
        assert_eq!(symbols[1].quote, "USDC");
        assert_eq!(symbols[2].quote, "EUR");
        assert_eq!(symbols[0].price_step.as_deref(), Some("0.000001"));
        assert!(parse_catalog(&json!({"message":"rate limited"})).is_err());
    }

    #[test]
    fn matches_invert_the_maker_side_and_do_not_replay_last_match() {
        let mut value = json!({"type":"match","product_id":"BTC-USD","price":"74230.12345678","size":"0.00000019","side":"sell","time":"2024-02-29T12:34:56.789123Z"});
        let buy = parse_trade(&value).unwrap();
        assert_eq!(buy.side, TradeSide::Buy);
        assert_eq!(buy.time_ms, 1709210096789);
        assert_eq!(buy.size, "0.00000019");
        assert_eq!(buy.price, "74230.12345678");
        value["side"] = json!("buy");
        assert_eq!(parse_trade(&value).unwrap().side, TradeSide::Sell);
        value["type"] = json!("last_match");
        assert!(parse_trade(&value).is_none());
        value["type"] = json!("match");
        value["size"] = json!("-1");
        assert!(parse_trade(&value).is_none());
        assert_eq!(timestamp(&json!("1970-01-01T00:00:00Z")), Some(0));
        assert_eq!(timestamp(&json!("1970-01-01T00:00:00.1Z")), Some(100));
        assert!(timestamp(&json!("invalid")).is_none());
    }

    #[test]
    fn l2_updates_replace_sizes_remove_zero_levels_and_reject_corruption() {
        let mut book = BookAccumulator::default();
        let delta = json!({"type":"l2update","changes":[["buy","100","3.5"],["sell","102","0"],["sell","103","2"]]});
        assert!(update_book(&mut book, &delta).is_err());
        update_book(&mut book,&json!({"type":"snapshot","bids":[["100","2"],["99","4"]],"asks":[["101","1"],["102","5"]]})).unwrap();
        let (bids, asks) = update_book(&mut book, &delta).unwrap();
        assert_eq!(bids[0].size, "3.5");
        assert_eq!(asks[0].size, "0");
        assert_eq!(book.bids[&Decimal::from(100)], Decimal::new(35, 1));
        assert!(!book.asks.contains_key(&Decimal::from(102)));
        assert_eq!(book.asks[&Decimal::from(103)], Decimal::from(2));
        assert!(
            update_book(
                &mut book,
                &json!({"type":"l2update","changes":[["unknown","1","1"]]})
            )
            .is_err()
        );
        assert!(
            update_book(
                &mut book,
                &json!({"type":"l2update","changes":[["buy","100","-1"]]})
            )
            .is_err()
        );
        assert!(
            update_book(
                &mut book,
                &json!({"type":"snapshot","bids":[["102","1"]],"asks":[["101","1"]]})
            )
            .is_err()
        );
    }

    #[test]
    fn candle_history_sorts_filters_and_preserves_empty_intervals() {
        let value = json!([
            [180, 99, 105, 100, 104, 3.5],
            [60, 98, 102, 99, 101, 2],
            [180, 99, 105, 100, 104, 3.5],
            [0, 98, 102, 99, 101, 2],
            [240, 98, 102, 99, 101, 2],
            [120, 110, 105, 100, 104, 1]
        ]);
        let candles = parse_candles(&value, 60, 240, 300).unwrap();
        assert_eq!(
            candles.iter().map(|c| c.time).collect::<Vec<_>>(),
            vec![60, 180]
        );
        assert_eq!(
            (
                candles[1].low,
                candles[1].high,
                candles[1].open,
                candles[1].close,
                candles[1].volume
            ),
            (99.0, 105.0, 100.0, 104.0, 3.5)
        );
        assert_eq!(parse_candles(&value, 60, 240, 1).unwrap()[0].time, 180);
        assert!(parse_candles(&json!({"message":"error"}), 0, 300, 300).is_err());
    }
}
