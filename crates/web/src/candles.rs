use std::collections::VecDeque;

use serde::{Deserialize, Serialize};
use terminal_core::Candle;

use crate::{MAX_PRICE_HISTORY_MS, TradePoint};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Timeframe {
    S1,
    S5,
    S30,
    #[default]
    M1,
    M5,
    M15,
    M30,
    H1,
}

impl Timeframe {
    pub const ALL: [Self; 8] = [
        Self::S1,
        Self::S5,
        Self::S30,
        Self::M1,
        Self::M5,
        Self::M15,
        Self::M30,
        Self::H1,
    ];

    pub fn seconds(self) -> i64 {
        match self {
            Self::S1 => 1,
            Self::S5 => 5,
            Self::S30 => 30,
            Self::M1 => 60,
            Self::M5 => 300,
            Self::M15 => 900,
            Self::M30 => 1800,
            Self::H1 => 3600,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::S1 => "1s",
            Self::S5 => "5s",
            Self::S30 => "30s",
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::M30 => "30m",
            Self::H1 => "1h",
        }
    }

    pub fn uses_trades(self) -> bool {
        self.seconds() < 60
    }
}

struct Bucket {
    candle: Candle,
    first_ms: i64,
    last_ms: i64,
}

// Aggregate on receipt, before the raw trade cap, so busy markets keep full volume.
#[derive(Default)]
pub struct History {
    buckets: VecDeque<Bucket>,
}

impl History {
    pub fn push(&mut self, trade: TradePoint) {
        if trade.time_ms < 0
            || !trade.price.is_finite()
            || trade.price <= 0.0
            || !trade.size.is_finite()
            || trade.size <= 0.0
        {
            return;
        }
        let second = trade.time_ms / 1000;
        let latest = self
            .buckets
            .back()
            .map_or(second, |b| b.candle.time.max(second));
        let cutoff = latest - MAX_PRICE_HISTORY_MS / 1000;
        while self.buckets.front().is_some_and(|b| b.candle.time < cutoff) {
            self.buckets.pop_front();
        }
        if second < cutoff {
            return;
        }
        let index = self.buckets.partition_point(|b| b.candle.time < second);
        if let Some(bucket) = self
            .buckets
            .get_mut(index)
            .filter(|b| b.candle.time == second)
        {
            let candle = &mut bucket.candle;
            candle.high = candle.high.max(trade.price);
            candle.low = candle.low.min(trade.price);
            candle.volume += trade.size;
            if trade.time_ms < bucket.first_ms {
                candle.open = trade.price;
                bucket.first_ms = trade.time_ms;
            }
            if trade.time_ms >= bucket.last_ms {
                candle.close = trade.price;
                bucket.last_ms = trade.time_ms;
            }
        } else {
            self.buckets.insert(
                index,
                Bucket {
                    candle: Candle {
                        time: second,
                        open: trade.price,
                        high: trade.price,
                        low: trade.price,
                        close: trade.price,
                        volume: trade.size,
                    },
                    first_ms: trade.time_ms,
                    last_ms: trade.time_ms,
                },
            );
        }
    }

    pub fn candles(&self, timeframe: Timeframe) -> Vec<Candle> {
        aggregate(self.buckets.iter().map(|b| &b.candle), timeframe)
    }
}

pub fn aggregate<'a>(
    candles: impl Iterator<Item = &'a Candle>,
    timeframe: Timeframe,
) -> Vec<Candle> {
    let seconds = timeframe.seconds();
    let mut result: Vec<Candle> = Vec::new();
    for candle in candles {
        let time = candle.time.div_euclid(seconds) * seconds;
        if let Some(last) = result.last_mut().filter(|last| last.time == time) {
            last.high = last.high.max(candle.high);
            last.low = last.low.min(candle.low);
            last.close = candle.close;
            last.volume += candle.volume;
        } else {
            result.push(Candle {
                time,
                ..candle.clone()
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_core::TradeSide;

    fn trade(time_ms: i64, price: f64, size: f64) -> TradePoint {
        TradePoint {
            time_ms,
            price,
            size,
            side: TradeSide::Buy,
        }
    }

    #[test]
    fn second_candles_update_live_and_use_exchange_time_for_late_trades() {
        let mut history = History::default();
        for trade in [
            trade(1_500, 100.0, 1.0),
            trade(1_900, 105.0, 2.0),
            trade(1_100, 99.0, 3.0),
            trade(5_000, 103.0, 4.0),
            trade(29_999, 107.0, 5.0),
            trade(30_000, 102.0, 6.0),
        ] {
            history.push(trade);
        }
        let seconds = history.candles(Timeframe::S1);
        assert_eq!(seconds.len(), 4);
        assert_eq!(
            seconds[0],
            Candle {
                time: 1,
                open: 99.0,
                high: 105.0,
                low: 99.0,
                close: 105.0,
                volume: 6.0
            }
        );
        let five = history.candles(Timeframe::S5);
        assert_eq!(
            five.iter().map(|c| c.time).collect::<Vec<_>>(),
            [0, 5, 25, 30]
        );
        let thirty = history.candles(Timeframe::S30);
        assert_eq!(
            thirty[0],
            Candle {
                time: 0,
                open: 99.0,
                high: 107.0,
                low: 99.0,
                close: 107.0,
                volume: 15.0
            }
        );
        assert_eq!(thirty[1].volume, 6.0);
        history.push(trade(30_100, 108.0, 1.0));
        assert_eq!(history.candles(Timeframe::S30)[1].close, 108.0);
        assert_eq!(history.candles(Timeframe::S30)[1].volume, 7.0);
    }

    #[test]
    fn history_is_bounded_rejects_invalid_trades_and_does_not_invent_gap_candles() {
        let mut history = History::default();
        for second in 0..=1200 {
            history.push(trade(second * 1000, 100.0, 1.0));
        }
        assert_eq!(history.buckets.len(), 601);
        for invalid in [
            trade(0, 100.0, 1.0),
            trade(-1, 100.0, 1.0),
            trade(1_200_000, f64::NAN, 1.0),
            trade(1_200_000, 0.0, 1.0),
            trade(1_200_000, 100.0, -1.0),
            trade(1_200_000, 100.0, f64::INFINITY),
        ] {
            history.push(invalid);
        }
        assert_eq!(history.candles(Timeframe::S1).len(), 601);
        assert_eq!(history.candles(Timeframe::S1).last().unwrap().volume, 1.0);
        history.push(trade(1_900_000, 101.0, 2.0));
        let candles = history.candles(Timeframe::S30);
        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].time, 1890);
        assert_eq!(candles[0].volume, 2.0);
    }

    #[test]
    fn larger_timeframes_aggregate_minute_history_in_utc_buckets() {
        let candles = [
            Candle {
                time: 240,
                open: 100.0,
                high: 110.0,
                low: 99.0,
                close: 105.0,
                volume: 2.0,
            },
            Candle {
                time: 300,
                open: 105.0,
                high: 120.0,
                low: 101.0,
                close: 110.0,
                volume: 3.0,
            },
            Candle {
                time: 360,
                open: 110.0,
                high: 115.0,
                low: 90.0,
                close: 98.0,
                volume: 4.0,
            },
        ];
        let five = aggregate(candles.iter(), Timeframe::M5);
        assert_eq!(five.len(), 2);
        assert_eq!(
            five[1],
            Candle {
                time: 300,
                open: 105.0,
                high: 120.0,
                low: 90.0,
                close: 98.0,
                volume: 7.0
            }
        );
        assert_eq!(aggregate(candles.iter(), Timeframe::H1)[0].volume, 9.0);
        assert_eq!(aggregate(candles.iter(), Timeframe::M1), candles);
    }
}
