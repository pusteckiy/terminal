use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Exchange {
    #[default]
    Binance,
    Okx,
    Bybit,
    Hyperliquid,
    Gate,
    Lighter,
    Bitget,
    Aster,
    Bitunix,
}

impl Exchange {
    pub const ALL: [Self; 9] = [
        Self::Binance,
        Self::Okx,
        Self::Bybit,
        Self::Hyperliquid,
        Self::Gate,
        Self::Lighter,
        Self::Bitget,
        Self::Aster,
        Self::Bitunix,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Binance => "Binance",
            Self::Okx => "OKX",
            Self::Bybit => "Bybit",
            Self::Hyperliquid => "Hyperliquid",
            Self::Gate => "Gate",
            Self::Lighter => "Lighter",
            Self::Bitget => "Bitget",
            Self::Aster => "Aster",
            Self::Bitunix => "Bitunix",
        }
    }

    pub const fn default_symbol(self, kind: MarketKind) -> &'static str {
        match (self, kind) {
            (Self::Binance | Self::Bybit | Self::Bitget | Self::Aster | Self::Bitunix, _) => {
                "BTCUSDT"
            }
            (Self::Okx, MarketKind::Spot) => "BTC-USDT",
            (Self::Okx, MarketKind::Perp) => "BTC-USDT-SWAP",
            (Self::Hyperliquid, MarketKind::Spot) => "PURR/USDC",
            (Self::Hyperliquid, MarketKind::Perp) => "BTC",
            (Self::Gate, _) => "BTC_USDT",
            (Self::Lighter, MarketKind::Spot) => "ETH/USDC",
            (Self::Lighter, MarketKind::Perp) => "BTC",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarketKind {
    #[default]
    Spot,
    Perp,
}

impl MarketKind {
    pub const ALL: [Self; 2] = [Self::Spot, Self::Perp];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Spot => "Spot",
            Self::Perp => "Perp",
        }
    }
}

// Used only to load layouts saved by the first BTC/ETH-only version.
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LegacyAsset {
    Btc,
    Eth,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
pub struct Market {
    pub exchange: Exchange,
    pub kind: MarketKind,
    pub symbol: String,
}

impl Default for Market {
    fn default() -> Self {
        Self::for_exchange_kind(Exchange::Binance, MarketKind::Spot)
    }
}

impl Market {
    pub fn for_exchange(exchange: Exchange) -> Self {
        let kind = match exchange {
            Exchange::Hyperliquid | Exchange::Lighter | Exchange::Bitunix => MarketKind::Perp,
            _ => MarketKind::Spot,
        };
        Self::for_exchange_kind(exchange, kind)
    }

    pub fn for_exchange_kind(exchange: Exchange, kind: MarketKind) -> Self {
        Self {
            exchange,
            kind,
            symbol: exchange.default_symbol(kind).to_owned(),
        }
    }

    pub fn market_type(&self) -> &'static str {
        self.kind.label()
    }
}

impl<'de> Deserialize<'de> for Market {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct SavedMarket {
            exchange: Exchange,
            kind: Option<MarketKind>,
            symbol: Option<String>,
            asset: Option<LegacyAsset>,
        }
        let saved = SavedMarket::deserialize(deserializer)?;
        let kind = saved.kind.unwrap_or_else(|| match saved.exchange {
            Exchange::Hyperliquid => MarketKind::Perp,
            Exchange::Lighter if !saved.symbol.as_deref().is_some_and(|s| s.contains('/')) => {
                MarketKind::Perp
            }
            _ => MarketKind::Spot,
        });
        let symbol = saved.symbol.unwrap_or_else(|| {
            let eth = matches!(saved.asset, Some(LegacyAsset::Eth));
            match (saved.exchange, eth) {
                (
                    Exchange::Binance
                    | Exchange::Bybit
                    | Exchange::Bitget
                    | Exchange::Aster
                    | Exchange::Bitunix,
                    true,
                ) => "ETHUSDT",
                (Exchange::Okx, true) => "ETH-USDT",
                (Exchange::Hyperliquid, true) => "ETH",
                (Exchange::Gate, true) => "ETH_USDT",
                (Exchange::Lighter, true) => "ETH",
                (exchange, false) => exchange.default_symbol(kind),
            }
            .to_owned()
        });
        if symbol.is_empty()
            || symbol.len() > 80
            || !symbol.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || matches!(character, '-' | '_' | '@' | ':' | '/')
            })
        {
            return Err(serde::de::Error::custom("invalid market symbol"));
        }
        Ok(Self {
            exchange: saved.exchange,
            kind,
            symbol,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SymbolInfo {
    pub symbol: String,
    pub base: String,
    pub quote: String,
    #[serde(default)]
    pub market_id: Option<u32>,
    #[serde(default)]
    pub size_multiplier: Option<String>,
    #[serde(default)]
    pub price_step: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Candle {
    pub time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Level {
    pub price: String,
    pub size: String,
    #[serde(default)]
    pub quote_size: String,
    #[serde(default)]
    pub depth_base: String,
    #[serde(default)]
    pub depth_quote: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Book {
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BookChange {
    pub price: String,
    pub size: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BestBidAsk {
    pub bid: String,
    pub ask: String,
    pub time_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LastPrice {
    pub price: String,
    pub time_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trade {
    pub price: String,
    pub size: String,
    pub time_ms: i64,
    #[serde(default)]
    pub side: TradeSide,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TradeSide {
    Buy,
    Sell,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Subscribe {
        market: Market,
    },
    Unsubscribe {
        market: Market,
    },
    Symbols {
        exchange: Exchange,
        kind: MarketKind,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Snapshot {
        market: Market,
        candles: Vec<Candle>,
        book: Option<Book>,
        #[serde(default)]
        best_bid_ask: Option<BestBidAsk>,
        last_price: Option<LastPrice>,
        connected: bool,
    },
    Book {
        market: Market,
        book: Book,
    },
    BookDelta {
        market: Market,
        bids: Vec<BookChange>,
        asks: Vec<BookChange>,
        updated_at_ms: i64,
    },
    BestBidAsk {
        market: Market,
        quote: BestBidAsk,
    },
    Candle {
        market: Market,
        candle: Candle,
    },
    LastPrice {
        market: Market,
        last_price: LastPrice,
    },
    Trades {
        market: Market,
        trades: Vec<Trade>,
    },
    Status {
        market: Market,
        connected: bool,
    },
    Symbols {
        exchange: Exchange,
        kind: MarketKind,
        symbols: Vec<SymbolInfo>,
        error: Option<String>,
    },
}

impl ServerMessage {
    pub fn market(&self) -> Option<&Market> {
        match self {
            Self::Snapshot { market, .. }
            | Self::Book { market, .. }
            | Self::BookDelta { market, .. }
            | Self::BestBidAsk { market, .. }
            | Self::Candle { market, .. }
            | Self::LastPrice { market, .. }
            | Self::Trades { market, .. }
            | Self::Status { market, .. } => Some(market),
            Self::Symbols { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_market_selection_restores_its_exchange_symbol() {
        let market: Market = serde_json::from_str(r#"{"exchange":"okx","asset":"eth"}"#).unwrap();
        assert_eq!(market.exchange, Exchange::Okx);
        assert_eq!(market.kind, MarketKind::Spot);
        assert_eq!(market.symbol, "ETH-USDT");
    }

    #[test]
    fn old_perpetual_layouts_and_new_market_kinds_round_trip() {
        let hyperliquid: Market =
            serde_json::from_str(r#"{"exchange":"hyperliquid","symbol":"BTC"}"#).unwrap();
        let lighter: Market =
            serde_json::from_str(r#"{"exchange":"lighter","symbol":"BTC"}"#).unwrap();
        let lighter_spot: Market =
            serde_json::from_str(r#"{"exchange":"lighter","symbol":"ETH/USDC"}"#).unwrap();
        assert_eq!(hyperliquid.kind, MarketKind::Perp);
        assert_eq!(lighter.kind, MarketKind::Perp);
        assert_eq!(lighter_spot.kind, MarketKind::Spot);
        let market = Market::for_exchange_kind(Exchange::Okx, MarketKind::Perp);
        assert_eq!(market.symbol, "BTC-USDT-SWAP");
        let restored: Market =
            serde_json::from_str(&serde_json::to_string(&market).unwrap()).unwrap();
        assert_eq!(restored, market);
    }
}
