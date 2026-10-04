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
    Kucoin,
    Kraken,
    Pacifica,
}

impl Exchange {
    pub const ALL: [Self; 12] = [
        Self::Binance,
        Self::Okx,
        Self::Bybit,
        Self::Hyperliquid,
        Self::Gate,
        Self::Lighter,
        Self::Bitget,
        Self::Aster,
        Self::Bitunix,
        Self::Kucoin,
        Self::Kraken,
        Self::Pacifica,
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
            Self::Kucoin => "KuCoin",
            Self::Kraken => "Kraken",
            Self::Pacifica => "Pacifica",
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
            (Self::Kucoin, MarketKind::Spot) => "BTC-USDT",
            (Self::Kucoin, MarketKind::Perp) => "XBTUSDTM",
            (Self::Kraken, MarketKind::Spot) => "BTC/USD",
            (Self::Kraken, MarketKind::Perp) => "PF_XBTUSD",
            (Self::Pacifica, MarketKind::Spot) => "SOL-USDC",
            (Self::Pacifica, MarketKind::Perp) => "BTC",
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
            Exchange::Hyperliquid | Exchange::Lighter | Exchange::Bitunix | Exchange::Pacifica => {
                MarketKind::Perp
            }
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
                (Exchange::Kucoin, true) if kind == MarketKind::Perp => "ETHUSDTM",
                (Exchange::Kucoin, true) => "ETH-USDT",
                (Exchange::Kraken, true) if kind == MarketKind::Perp => "PF_ETHUSD",
                (Exchange::Kraken, true) => "ETH/USD",
                (Exchange::Pacifica, true) if kind == MarketKind::Spot => "SOL-USDC",
                (Exchange::Pacifica, true) => "ETH",
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

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Account {
    pub exchange: Exchange,
    pub address: String,
}

impl Account {
    pub fn valid(&self) -> bool {
        self.exchange == Exchange::Hyperliquid
            && self.address.len() == 42
            && self.address.starts_with("0x")
            && self.address[2..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnOrder {
    pub coin: String,
    pub order_id: u64,
    pub side: TradeSide,
    pub price: String,
    pub size: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub coin: String,
    pub size: String,
    pub entry_price: String,
    pub unrealized_pnl: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OwnFill {
    pub market: Market,
    pub time_ms: i64,
    pub trade_id: u64,
    pub order_id: u64,
    pub side: TradeSide,
    pub price: String,
    pub size: String,
    pub taker: bool,
    pub fee: String,
    pub fee_token: String,
}

impl OwnFill {
    pub fn key(&self) -> (i64, &str, &str, u64, u64) {
        (
            self.time_ms,
            self.market.kind.label(),
            &self.market.symbol,
            self.trade_id,
            self.order_id,
        )
    }
}

pub const MAX_ACCOUNT_FILLS: usize = 1_000;

// Snapshots, live batches, and reconnect replays use the same execution identity.
// Keep individual fills (including partial fills), newest first, with bounded memory.
pub fn merge_account_fills(history: &mut Vec<OwnFill>, fills: Vec<OwnFill>) -> Vec<OwnFill> {
    let mut added = Vec::new();
    for fill in fills {
        let index = history.partition_point(|stored| stored.key() > fill.key());
        if index >= MAX_ACCOUNT_FILLS
            || history
                .get(index)
                .is_some_and(|stored| stored.key() == fill.key())
        {
            continue;
        }
        added.push(fill.clone());
        history.insert(index, fill);
        history.truncate(MAX_ACCOUNT_FILLS);
    }
    added
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountState {
    pub orders: Vec<OwnOrder>,
    pub positions: Vec<Position>,
    pub connected: bool,
    pub updated_at_ms: i64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    SubscribeAccount {
        account: Account,
    },
    UnsubscribeAccount {
        account: Account,
    },
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
    AccountFills {
        account: Account,
        fills: Vec<OwnFill>,
        snapshot: bool,
    },
    AccountState {
        account: Account,
        state: AccountState,
    },
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
            Self::AccountState { .. } | Self::AccountFills { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_fill_replay_is_bounded_sorted_and_keeps_partial_executions() {
        let fill = OwnFill {
            market: Market::for_exchange(Exchange::Hyperliquid),
            time_ms: 1000,
            trade_id: 1,
            order_id: 2,
            side: TradeSide::Buy,
            price: "100".into(),
            size: "0.2".into(),
            taker: false,
            fee: "0".into(),
            fee_token: "USDC".into(),
        };
        let mut history = Vec::new();
        assert_eq!(
            merge_account_fills(&mut history, vec![fill.clone()]).len(),
            1
        );
        assert!(merge_account_fills(&mut history, vec![fill.clone()]).is_empty());
        let mut partial = fill.clone();
        partial.trade_id = 3;
        merge_account_fills(&mut history, vec![partial]);
        assert_eq!(history.len(), 2);
        for time in 1001..2200 {
            let mut item = fill.clone();
            item.time_ms = time;
            merge_account_fills(&mut history, vec![item]);
        }
        assert_eq!(history.len(), MAX_ACCOUNT_FILLS);
        assert_eq!(history[0].time_ms, 2199);
        assert!(history.windows(2).all(|rows| rows[0].key() > rows[1].key()));
        assert!(merge_account_fills(&mut history, vec![fill]).is_empty());
    }

    #[test]
    fn chart_messages_decode_decimal_candles() {
        let snapshot = r#"{"type":"snapshot","market":{"exchange":"okx","kind":"spot","symbol":"SOL-USDT"},"candles":[{"time":1791105720,"open":121.01,"high":121.05,"low":121.0,"close":121.04,"volume":146.030352}],"book":null,"best_bid_ask":null,"last_price":null,"connected":true}"#;
        let message = serde_json::from_str::<ServerMessage>(snapshot).unwrap();
        let ServerMessage::Snapshot { candles, .. } = message else {
            panic!("expected candle history");
        };
        assert_eq!(candles.len(), 1);
        assert_eq!(candles[0].close, 121.04);
        let update = r#"{"type":"candle","market":{"exchange":"okx","kind":"spot","symbol":"SOL-USDT"},"candle":{"time":1791123660,"open":121.3,"high":121.31,"low":121.25,"close":121.25,"volume":241.663733}}"#;
        let message = serde_json::from_str::<ServerMessage>(update).unwrap();
        let ServerMessage::Candle { candle, .. } = message else {
            panic!("expected live candle");
        };
        assert_eq!(candle.close, 121.25);
    }

    #[test]
    fn hyperliquid_account_requires_wallet_address() {
        let account = Account {
            exchange: Exchange::Hyperliquid,
            address: "0x0000000000000000000000000000000000000000".into(),
        };
        assert!(account.valid());
        assert!(
            !Account {
                address: "0x1234".into(),
                ..account.clone()
            }
            .valid()
        );
        assert!(
            !Account {
                exchange: Exchange::Binance,
                ..account
            }
            .valid()
        );
    }

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
