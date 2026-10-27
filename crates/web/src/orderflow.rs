use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use terminal_core::{Exchange, Market, MarketKind, SymbolInfo, TradeSide};

use crate::{MAX_PRICE_HISTORY_MS, MarketData, TradePoint};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Units {
    Base,
    #[default]
    Quote,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub source: Option<Market>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Volume {
    pub base: f64,
    pub quote: f64,
}

impl Volume {
    pub fn value(self, units: Units) -> f64 {
        match units {
            Units::Base => self.base,
            Units::Quote => self.quote,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Bucket {
    pub second: i64,
    pub buy: Volume,
    pub sell: Volume,
    pub unknown: Volume,
    pub trades: u64,
}

impl Bucket {
    fn add(&mut self, other: &Self) {
        for (total, volume) in [
            (&mut self.buy, other.buy),
            (&mut self.sell, other.sell),
            (&mut self.unknown, other.unknown),
        ] {
            total.base += volume.base;
            total.quote += volume.quote;
        }
        self.trades += other.trades;
    }

    pub fn delta(self, units: Units) -> f64 {
        self.buy.value(units) - self.sell.value(units)
    }
}

// Aggregate before the price plot's point cap: volume never depends on rendered dots.
#[derive(Default)]
pub struct History {
    pub buckets: VecDeque<Bucket>,
}

impl History {
    pub fn push(&mut self, trade: TradePoint) {
        let quote = trade.price * trade.size;
        if !trade.price.is_finite()
            || trade.price <= 0.0
            || !trade.size.is_finite()
            || trade.size <= 0.0
            || !quote.is_finite()
        {
            return;
        }
        let second = trade.time_ms.div_euclid(1_000);
        let latest = self
            .buckets
            .back()
            .map_or(second, |bucket| bucket.second)
            .max(second);
        let cutoff = latest - MAX_PRICE_HISTORY_MS / 1_000;
        while self
            .buckets
            .front()
            .is_some_and(|bucket| bucket.second < cutoff)
        {
            self.buckets.pop_front();
        }
        if second < cutoff {
            return;
        }
        // Late trades update their original second; gaps need no stored empty buckets.
        let index = self
            .buckets
            .partition_point(|bucket| bucket.second < second);
        if self
            .buckets
            .get(index)
            .is_none_or(|bucket| bucket.second != second)
        {
            self.buckets.insert(
                index,
                Bucket {
                    second,
                    ..Default::default()
                },
            );
        }
        let bucket = &mut self.buckets[index];
        let volume = match trade.side {
            TradeSide::Buy => &mut bucket.buy,
            TradeSide::Sell => &mut bucket.sell,
            TradeSide::Unknown => &mut bucket.unknown,
        };
        volume.base += trade.size;
        volume.quote += quote;
        bucket.trades += 1;
    }
}

pub fn symbol_info<'a>(
    market: &Market,
    catalogs: &'a HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
) -> Option<&'a SymbolInfo> {
    catalogs
        .get(&(market.exchange, market.kind))?
        .iter()
        .find(|info| info.symbol == market.symbol)
}

pub fn base_units(base: &str, kind: MarketKind) -> (&str, u64) {
    if base.eq_ignore_ascii_case("XBT") {
        return ("BTC", 1);
    }
    if kind == MarketKind::Perp {
        if let Some(coin) = base.strip_prefix('k')
            && coin.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
        {
            return (coin, 1_000);
        }
        let digits = base.bytes().take_while(u8::is_ascii_digit).count();
        let (prefix, coin) = base.split_at(digits);
        // Only explicit powers of ten are bundle prefixes; 1INCH and KAS are assets.
        if !coin.is_empty()
            && prefix.starts_with('1')
            && prefix.len() >= 4
            && prefix[1..].bytes().all(|byte| byte == b'0')
            && let Ok(multiplier) = prefix.parse()
        {
            return (coin, multiplier);
        }
    }
    (base, 1)
}

// Different quote currencies are not interchangeable. In Base mode only the asset
// must match; adapters already normalize contract quantities to base units.
pub fn compatible(
    markets: &[Market],
    catalogs: &HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
    units: Units,
) -> bool {
    let Some(first) = markets
        .first()
        .and_then(|market| symbol_info(market, catalogs))
    else {
        return false;
    };
    markets.iter().all(|market| {
        symbol_info(market, catalogs).is_some_and(|info| {
            base_units(&info.base, market.kind)
                .0
                .eq_ignore_ascii_case(base_units(&first.base, markets[0].kind).0)
                && (units == Units::Base || info.quote.eq_ignore_ascii_case(&first.quote))
        })
    })
}

pub fn collect(
    markets: &[Market],
    source: Option<&Market>,
    data: &HashMap<Market, MarketData>,
    catalogs: &HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
    start_ms: i64,
    end_ms: i64,
) -> Vec<Bucket> {
    let start = start_ms.div_euclid(1_000);
    let end = end_ms.div_euclid(1_000);
    let mut result: Vec<_> = (start..=end)
        .map(|second| Bucket {
            second,
            ..Default::default()
        })
        .collect();
    for market in markets
        .iter()
        .filter(|market| source.is_none_or(|selected| selected == *market))
    {
        if let Some(data) = data.get(market) {
            let multiplier = symbol_info(market, catalogs)
                .map_or(1, |info| base_units(&info.base, market.kind).1)
                as f64;
            for bucket in &data.orderflow.buckets {
                if (start..=end).contains(&bucket.second) {
                    let mut normalized = *bucket;
                    for volume in [
                        &mut normalized.buy,
                        &mut normalized.sell,
                        &mut normalized.unknown,
                    ] {
                        volume.base *= multiplier;
                    }
                    result[(bucket.second - start) as usize].add(&normalized);
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundles_normalize_base_volume_without_changing_quote_notional() {
        assert_eq!(base_units("XBT", MarketKind::Perp), ("BTC", 1));
        assert_eq!(base_units("XBT", MarketKind::Spot), ("BTC", 1));
        for (base, expected, multiplier) in [
            ("1000PEPE", "PEPE", 1_000),
            ("kPEPE", "PEPE", 1_000),
            ("1000000MOG", "MOG", 1_000_000),
            ("10000SATS", "SATS", 10_000),
            ("1INCH", "1INCH", 1),
            ("KAS", "KAS", 1),
            ("KAVA", "KAVA", 1),
            ("kaito", "kaito", 1),
            ("123TOKEN", "123TOKEN", 1),
            ("PEPE", "PEPE", 1),
        ] {
            assert_eq!(base_units(base, MarketKind::Perp), (expected, multiplier));
            assert_eq!(base_units(base, MarketKind::Spot), (base, 1));
        }
        let mut markets = Vec::new();
        let mut catalogs = HashMap::new();
        let mut data = HashMap::new();
        for (exchange, base, price, size) in [
            (Exchange::Binance, "1000PEPE", 0.004, 2.0),
            (Exchange::Hyperliquid, "kPEPE", 0.004, 2.0),
            (Exchange::Okx, "PEPE", 0.000004, 2000.0),
        ] {
            let market = Market {
                exchange,
                kind: MarketKind::Perp,
                symbol: base.into(),
            };
            catalogs.insert(
                (exchange, market.kind),
                vec![SymbolInfo {
                    symbol: base.into(),
                    base: base.into(),
                    quote: "USDT".into(),
                    base_token_id: None,
                    market_id: None,
                    size_multiplier: None,
                    price_step: None,
                }],
            );
            let mut feed = MarketData::default();
            for side in [TradeSide::Buy, TradeSide::Sell, TradeSide::Unknown] {
                feed.orderflow.push(trade(1_000, price, size, side));
            }
            data.insert(market.clone(), feed);
            markets.push(market);
        }
        assert!(compatible(&markets, &catalogs, Units::Quote));
        for source in [None, Some(&markets[0])] {
            let buckets = collect(&markets, source, &data, &catalogs, 1_000, 1_000);
            let count = if source.is_some() { 1.0 } else { 3.0 };
            for volume in [buckets[0].buy, buckets[0].sell, buckets[0].unknown] {
                assert_eq!(volume.base, 2000.0 * count);
                assert!((volume.quote - 0.008 * count).abs() < 1e-12);
            }
        }
    }

    fn trade(time_ms: i64, price: f64, size: f64, side: TradeSide) -> TradePoint {
        TradePoint {
            time_ms,
            price,
            exact_price: price.to_string().parse().ok(),
            size,
            side,
        }
    }

    #[test]
    fn live_second_accumulates_each_fill_and_keeps_unknown_separate() {
        let mut history = History::default();
        history.push(trade(1_000, 100.0, 2.0, TradeSide::Buy));
        assert_eq!(history.buckets[0].buy.quote, 200.0);
        history.push(trade(1_999, 110.0, 3.0, TradeSide::Buy));
        history.push(trade(1_500, 120.0, 1.0, TradeSide::Sell));
        history.push(trade(1_800, 130.0, 4.0, TradeSide::Unknown));
        history.push(trade(2_000, 140.0, 1.0, TradeSide::Sell));
        assert_eq!(history.buckets.len(), 2);
        let bucket = history.buckets[0];
        assert_eq!(bucket.buy.base, 5.0);
        assert_eq!(bucket.buy.quote, 530.0);
        assert_eq!(bucket.delta(Units::Quote), 410.0);
        assert_eq!(bucket.unknown.quote, 520.0);
        assert_eq!(bucket.trades, 4);
    }

    #[test]
    fn history_is_bounded_handles_late_trades_and_rejects_invalid_sizes() {
        let mut history = History::default();
        for second in 0..=602 {
            history.push(trade(second * 1_000, 10.0, 1.0, TradeSide::Buy));
        }
        assert_eq!(history.buckets.len(), 601);
        assert_eq!(history.buckets.front().unwrap().second, 2);
        history.push(trade(3_500, 10.0, 2.0, TradeSide::Sell));
        assert_eq!(history.buckets[1].sell.base, 2.0);
        history.push(trade(1_000, 10.0, 5.0, TradeSide::Buy));
        for size in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            history.push(trade(602_001, 10.0, size, TradeSide::Buy));
        }
        assert_eq!(history.buckets.len(), 601);
        assert_eq!(history.buckets.back().unwrap().trades, 1);
    }

    #[test]
    fn all_sources_sum_by_utc_second_and_fill_empty_seconds() {
        let markets = [
            Market::for_exchange(Exchange::Binance),
            Market::for_exchange(Exchange::Bybit),
        ];
        let mut data = HashMap::new();
        for (market, side) in markets.iter().zip([TradeSide::Buy, TradeSide::Sell]) {
            let mut feed = MarketData::default();
            feed.orderflow.push(trade(1_700, 10.0, 2.0, side));
            data.insert(market.clone(), feed);
        }
        let buckets = collect(&markets, None, &data, &HashMap::new(), 1_000, 3_500);
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].buy.quote, 20.0);
        assert_eq!(buckets[0].sell.quote, 20.0);
        assert_eq!(buckets[1].trades, 0);
        let selected = collect(
            &markets,
            Some(&markets[0]),
            &data,
            &HashMap::new(),
            1_000,
            3_500,
        );
        assert_eq!(selected[0].delta(Units::Quote), 20.0);
        assert_eq!(selected[0].trades, 1);
    }

    #[test]
    fn aggregation_requires_matching_assets_and_volume_units() {
        let markets = [
            Market::for_exchange(Exchange::Binance),
            Market::for_exchange(Exchange::Hyperliquid),
        ];
        let mut catalogs = HashMap::new();
        for (market, quote) in markets.iter().zip(["USDT", "USDC"]) {
            catalogs.insert(
                (market.exchange, market.kind),
                vec![SymbolInfo {
                    symbol: market.symbol.clone(),
                    base: "BTC".into(),
                    quote: quote.into(),
                    base_token_id: None,
                    market_id: None,
                    size_multiplier: None,
                    price_step: None,
                }],
            );
        }
        assert!(!compatible(&markets, &catalogs, Units::Quote));
        assert!(compatible(&markets, &catalogs, Units::Base));
        catalogs
            .get_mut(&(markets[1].exchange, markets[1].kind))
            .unwrap()[0]
            .base = "ETH".into();
        assert!(!compatible(&markets, &catalogs, Units::Base));
        assert!(!compatible(&markets, &HashMap::new(), Units::Base));
    }
}
