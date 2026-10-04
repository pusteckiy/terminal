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
            info.base.eq_ignore_ascii_case(&first.base)
                && (units == Units::Base || info.quote.eq_ignore_ascii_case(&first.quote))
        })
    })
}

pub fn collect(
    markets: &[Market],
    source: Option<&Market>,
    data: &HashMap<Market, MarketData>,
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
            for bucket in &data.orderflow.buckets {
                if (start..=end).contains(&bucket.second) {
                    result[(bucket.second - start) as usize].add(bucket);
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(time_ms: i64, price: f64, size: f64, side: TradeSide) -> TradePoint {
        TradePoint {
            time_ms,
            price,
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
        let buckets = collect(&markets, None, &data, 1_000, 3_500);
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].buy.quote, 20.0);
        assert_eq!(buckets[0].sell.quote, 20.0);
        assert_eq!(buckets[1].trades, 0);
        let selected = collect(&markets, Some(&markets[0]), &data, 1_000, 3_500);
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
