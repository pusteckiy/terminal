//! The relay can only forward these signed, read-only exchange requests.
use crate::Exchange;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadRoute {
    BinanceKey,
    BinanceSpotAccount,
    BinanceSpotOrders,
    BinanceSpotFills,
    BinancePerpAccount,
    BinancePerpOrders,
    BinancePerpFills,
    BybitKey,
    BybitOrders,
    BybitPositions,
    BybitWallet,
    BybitFills,
    OkxConfig,
    OkxOrders,
    OkxPositions,
    OkxWallet,
    OkxFills,
    OkxInstruments,
}
impl ReadRoute {
    pub fn exchange(self) -> Exchange {
        use ReadRoute::*;
        match self {
            BinanceKey | BinanceSpotAccount | BinanceSpotOrders | BinanceSpotFills
            | BinancePerpAccount | BinancePerpOrders | BinancePerpFills => Exchange::Binance,
            BybitKey | BybitOrders | BybitPositions | BybitWallet | BybitFills => Exchange::Bybit,
            _ => Exchange::Okx,
        }
    }
    pub fn origin(self) -> &'static str {
        use ReadRoute::*;
        match self {
            BinanceKey | BinanceSpotAccount | BinanceSpotOrders | BinanceSpotFills => {
                "https://api.binance.com"
            }
            BinancePerpAccount | BinancePerpOrders | BinancePerpFills => "https://fapi.binance.com",
            BybitKey | BybitOrders | BybitPositions | BybitWallet | BybitFills => {
                "https://api.bybit.com"
            }
            _ => "https://openapi.okx.com",
        }
    }
    pub fn path(self) -> &'static str {
        use ReadRoute::*;
        match self {
            BinanceKey => "/sapi/v1/account/apiRestrictions",
            BinanceSpotAccount => "/api/v3/account",
            BinanceSpotOrders => "/api/v3/openOrders",
            BinanceSpotFills => "/api/v3/myTrades",
            BinancePerpAccount => "/fapi/v3/positionRisk",
            BinancePerpOrders => "/fapi/v1/openOrders",
            BinancePerpFills => "/fapi/v1/userTrades",
            BybitKey => "/v5/user/query-api",
            BybitOrders => "/v5/order/realtime",
            BybitPositions => "/v5/position/list",
            BybitWallet => "/v5/account/wallet-balance",
            BybitFills => "/v5/execution/list",
            OkxConfig => "/api/v5/account/config",
            OkxOrders => "/api/v5/trade/orders-pending",
            OkxPositions => "/api/v5/account/positions",
            OkxWallet => "/api/v5/account/balance",
            OkxFills => "/api/v5/trade/fills",
            OkxInstruments => "/api/v5/public/instruments",
        }
    }
}
// Deliberately no API secret, password, arbitrary URL, method, or request body.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRead {
    pub route: ReadRoute,
    pub query: String,
    pub headers: Vec<(String, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SocketFeed {
    BinanceSpot,
    BinanceApi,
    BinancePerp,
    Bybit,
    Okx,
}
impl SocketFeed {
    pub fn name(self) -> &'static str {
        match self {
            Self::BinanceSpot => "binance_spot",
            Self::BinanceApi => "binance_api",
            Self::BinancePerp => "binance_perp",
            Self::Bybit => "bybit",
            Self::Okx => "okx",
        }
    }
    pub fn url(self) -> &'static str {
        match self {
            Self::BinanceSpot => "wss://ws-api.binance.com:443/ws-api/v3",
            Self::BinanceApi => "wss://ws-fapi.binance.com/ws-fapi/v1",
            Self::BinancePerp => "wss://fstream.binance.com/private/ws/",
            Self::Bybit => "wss://stream.bybit.com/v5/private",
            Self::Okx => "wss://ws.okx.com/ws/v5/private",
        }
    }
}
