use reqwest::Client;
use serde_json::{Value, json};
use terminal_core::{Exchange, MarketKind, SymbolInfo};

type Error = Box<dyn std::error::Error + Send + Sync>;

pub async fn fetch(
    exchange: Exchange,
    kind: MarketKind,
    client: &Client,
) -> Result<Vec<SymbolInfo>, Error> {
    if crate::exchange::additional::handles(exchange) {
        return crate::exchange::additional::catalog(exchange, kind, client).await;
    }
    let response: Value = match exchange {
        Exchange::Binance => {
            client
                .get(match kind {
                    MarketKind::Spot => "https://api.binance.com/api/v3/exchangeInfo",
                    MarketKind::Perp => "https://fapi.binance.com/fapi/v1/exchangeInfo",
                })
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        Exchange::Okx => {
            client
                .get("https://www.okx.com/api/v5/public/instruments")
                .query(&[(
                    "instType",
                    if kind == MarketKind::Spot {
                        "SPOT"
                    } else {
                        "SWAP"
                    },
                )])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        Exchange::Bybit => {
            client
                .get("https://api.bybit.com/v5/market/instruments-info")
                .query(&[
                    (
                        "category",
                        if kind == MarketKind::Spot {
                            "spot"
                        } else {
                            "linear"
                        },
                    ),
                    ("limit", "1000"),
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
                .json(&json!({"type":if kind == MarketKind::Spot { "spotMeta" } else { "meta" }}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        Exchange::Gate => {
            client
                .get(match kind {
                    MarketKind::Spot => "https://api.gateio.ws/api/v4/spot/currency_pairs",
                    MarketKind::Perp => "https://api.gateio.ws/api/v4/futures/usdt/contracts",
                })
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        Exchange::Lighter => {
            client
                .get("https://mainnet.zklighter.elliot.ai/api/v1/orderBooks")
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        Exchange::Bitget => {
            client
                .get(if kind == MarketKind::Spot {
                    "https://api.bitget.com/api/v2/spot/public/symbols"
                } else {
                    "https://api.bitget.com/api/v2/mix/market/contracts?productType=usdt-futures"
                })
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        Exchange::Aster => {
            client
                .get(if kind == MarketKind::Spot {
                    "https://sapi.asterdex.com/api/v3/exchangeInfo"
                } else {
                    "https://fapi.asterdex.com/fapi/v3/exchangeInfo"
                })
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        Exchange::Bitunix => {
            client
                .get(if kind == MarketKind::Spot {
                    "https://openapi.bitunix.com/api/spot/v1/common/coin_pair/list"
                } else {
                    "https://fapi.bitunix.com/api/v1/futures/market/trading_pairs"
                })
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?
        }
        Exchange::Kucoin | Exchange::Kraken | Exchange::Pacifica => unreachable!(),
    };
    let mut symbols = parse(exchange, kind, &response)?;
    if exchange == Exchange::Bybit && kind == MarketKind::Perp {
        let mut cursor = response["result"]["nextPageCursor"]
            .as_str()
            .unwrap_or("")
            .to_owned();
        let mut seen = std::collections::HashSet::new();
        while !cursor.is_empty() && seen.insert(cursor.to_owned()) {
            let page: Value = client
                .get("https://api.bybit.com/v5/market/instruments-info")
                .query(&[
                    ("category", "linear"),
                    ("limit", "1000"),
                    ("cursor", cursor.as_str()),
                ])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            symbols.extend(parse(exchange, kind, &page)?);
            cursor = page["result"]["nextPageCursor"]
                .as_str()
                .unwrap_or("")
                .to_owned();
        }
        symbols.sort_unstable_by(|a, b| a.symbol.cmp(&b.symbol));
        symbols.dedup_by(|a, b| a.symbol == b.symbol);
    }
    Ok(symbols)
}

fn parse(exchange: Exchange, kind: MarketKind, response: &Value) -> Result<Vec<SymbolInfo>, Error> {
    let rows = match exchange {
        Exchange::Binance => &response["symbols"],
        Exchange::Okx => &response["data"],
        Exchange::Bybit => &response["result"]["list"],
        Exchange::Hyperliquid => &response["universe"],
        Exchange::Gate => response,
        Exchange::Lighter => &response["order_books"],
        Exchange::Bitget | Exchange::Bitunix => &response["data"],
        Exchange::Aster => &response["symbols"],
        Exchange::Kucoin | Exchange::Kraken | Exchange::Pacifica => unreachable!(),
    }
    .as_array()
    .ok_or_else(|| std::io::Error::other("invalid symbol catalog response"))?;
    let mut symbols = rows
        .iter()
        .filter_map(|row| {
            let (enabled, symbol, base, quote, market_id, size_multiplier) = match exchange {
                Exchange::Binance => (
                    row["status"] == "TRADING"
                        && (kind == MarketKind::Spot || row["contractType"] == "PERPETUAL"),
                    row["symbol"].as_str()?,
                    row["baseAsset"].as_str()?,
                    row["quoteAsset"].as_str()?,
                    None,
                    None,
                ),
                Exchange::Okx => (
                    row["state"] == "live"
                        && (kind == MarketKind::Spot || row["ctType"] == "linear"),
                    row["instId"].as_str()?,
                    if kind == MarketKind::Spot {
                        row["baseCcy"].as_str()?
                    } else {
                        row["ctValCcy"].as_str()?
                    },
                    if kind == MarketKind::Spot {
                        row["quoteCcy"].as_str()?
                    } else {
                        row["settleCcy"].as_str()?
                    },
                    None,
                    if kind == MarketKind::Perp {
                        row["ctVal"].as_str().map(str::to_owned)
                    } else {
                        None
                    },
                ),
                Exchange::Bybit => (
                    row["status"] == "Trading"
                        && (kind == MarketKind::Spot || row["contractType"] == "LinearPerpetual"),
                    row["symbol"].as_str()?,
                    row["baseCoin"].as_str()?,
                    row["quoteCoin"].as_str()?,
                    None,
                    None,
                ),
                Exchange::Hyperliquid if kind == MarketKind::Perp => (
                    row["isDelisted"] != true,
                    row["name"].as_str()?,
                    row["name"].as_str()?,
                    "USD",
                    None,
                    None,
                ),
                Exchange::Hyperliquid => {
                    let tokens = response["tokens"].as_array()?;
                    let base_index = row["tokens"][0].as_u64()?;
                    let quote_index = row["tokens"][1].as_u64()?;
                    let base = tokens
                        .iter()
                        .find(|token| token["index"].as_u64() == Some(base_index))?["name"]
                        .as_str()?;
                    let quote = tokens
                        .iter()
                        .find(|token| token["index"].as_u64() == Some(quote_index))?["name"]
                        .as_str()?;
                    (true, row["name"].as_str()?, base, quote, None, None)
                }
                Exchange::Gate if kind == MarketKind::Spot => (
                    row["trade_status"] == "tradable",
                    row["id"].as_str()?,
                    row["base"].as_str()?,
                    row["quote"].as_str()?,
                    None,
                    None,
                ),
                Exchange::Gate => {
                    let symbol = row["name"].as_str()?;
                    let (base, quote) = symbol.split_once('_')?;
                    (
                        row["in_delisting"] != true
                            && row["type"] == "direct"
                            && (row["status"].is_null()
                                || row["status"] == "trading"
                                || row["status"] == "online"),
                        symbol,
                        base,
                        quote,
                        None,
                        row["quanto_multiplier"].as_str().map(str::to_owned),
                    )
                }
                Exchange::Lighter => {
                    let symbol = row["symbol"].as_str()?;
                    let (base, quote) = symbol.split_once('/').unwrap_or((symbol, "USD"));
                    (
                        row["status"] == "active"
                            && row["is_frozen"] != true
                            && if kind == MarketKind::Spot {
                                row["market_type"] == "spot"
                            } else {
                                row["market_type"] == "perp"
                            },
                        symbol,
                        base,
                        quote,
                        Some(u32::try_from(row["market_id"].as_u64()?).ok()?),
                        None,
                    )
                }
                Exchange::Bitget => (
                    if kind == MarketKind::Spot {
                        row["status"] == "online"
                    } else {
                        row["symbolStatus"] == "normal" && row["symbolType"] == "perpetual"
                    },
                    row["symbol"].as_str()?,
                    row["baseCoin"].as_str()?,
                    row["quoteCoin"].as_str()?,
                    None,
                    None,
                ),
                Exchange::Aster => (
                    row["status"] == "TRADING"
                        && (kind == MarketKind::Spot || row["contractType"] == "PERPETUAL"),
                    row["symbol"].as_str()?,
                    row["baseAsset"].as_str()?,
                    row["quoteAsset"].as_str()?,
                    None,
                    None,
                ),
                Exchange::Bitunix if kind == MarketKind::Spot => (
                    row["isOpen"] == 1,
                    row["symbol"].as_str()?,
                    row["base"].as_str()?,
                    row["quote"].as_str()?,
                    None,
                    None,
                ),
                Exchange::Bitunix => (
                    row["symbolStatus"] == "OPEN" && row["isApiSupported"] == true,
                    row["symbol"].as_str()?,
                    row["base"].as_str()?,
                    row["quote"].as_str()?,
                    None,
                    None,
                ),
                Exchange::Kucoin | Exchange::Kraken | Exchange::Pacifica => unreachable!(),
            };
            enabled.then(|| SymbolInfo {
                symbol: if exchange == Exchange::Bitunix && kind == MarketKind::Spot {
                    symbol.to_ascii_uppercase()
                } else {
                    symbol.to_owned()
                },
                base: base.to_owned(),
                quote: quote.to_owned(),
                base_token_id: if exchange == Exchange::Hyperliquid && kind == MarketKind::Spot {
                    row["tokens"][0]
                        .as_u64()
                        .and_then(|id| u32::try_from(id).ok())
                } else {
                    None
                },
                market_id,
                size_multiplier,
                price_step: if exchange == Exchange::Bitunix && kind == MarketKind::Spot {
                    row["precisions"][0].as_str().map(str::to_owned)
                } else {
                    None
                },
            })
        })
        .collect::<Vec<_>>();
    if symbols.is_empty() {
        return Err(std::io::Error::other("empty symbol catalog").into());
    }
    symbols.sort_unstable_by(|a, b| a.symbol.cmp(&b.symbol));
    Ok(symbols)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_venue_catalogs_select_active_spot_and_perpetual_symbols() {
        let bitget_spot = json!({"data":[{"symbol":"BTCUSDT","baseCoin":"BTC","quoteCoin":"USDT","status":"online"},{"symbol":"OLDUSDT","baseCoin":"OLD","quoteCoin":"USDT","status":"offline"}]});
        assert_eq!(
            parse(Exchange::Bitget, MarketKind::Spot, &bitget_spot)
                .unwrap()
                .len(),
            1
        );
        let bitget_perp = json!({"data":[{"symbol":"BTCUSDT","baseCoin":"BTC","quoteCoin":"USDT","symbolStatus":"normal","symbolType":"perpetual"},{"symbol":"ETHUSDT","baseCoin":"ETH","quoteCoin":"USDT","symbolStatus":"offline","symbolType":"perpetual"}]});
        assert_eq!(
            parse(Exchange::Bitget, MarketKind::Perp, &bitget_perp)
                .unwrap()
                .len(),
            1
        );
        let aster = json!({"symbols":[{"symbol":"BTCUSDT","baseAsset":"BTC","quoteAsset":"USDT","status":"TRADING","contractType":"PERPETUAL"},{"symbol":"ETHUSDT","baseAsset":"ETH","quoteAsset":"USDT","status":"TRADING","contractType":"CURRENT_QUARTER"}]});
        assert_eq!(
            parse(Exchange::Aster, MarketKind::Perp, &aster)
                .unwrap()
                .len(),
            1
        );
        let bitunix_spot = json!({"data":[{"symbol":"btcusdt","base":"BTC","quote":"USDT","isOpen":1,"precisions":["0.01","0.1"]}]});
        let symbols = parse(Exchange::Bitunix, MarketKind::Spot, &bitunix_spot).unwrap();
        assert_eq!(symbols[0].symbol, "BTCUSDT");
        assert_eq!(symbols[0].price_step.as_deref(), Some("0.01"));
        let bitunix_perp = json!({"data":[{"symbol":"BTCUSDT","base":"BTC","quote":"USDT","symbolStatus":"OPEN","isApiSupported":true}]});
        assert_eq!(
            parse(Exchange::Bitunix, MarketKind::Perp, &bitunix_perp)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn catalogs_keep_only_active_symbols() {
        let value = json!({"symbols": [
            {"symbol":"BTCUSDT","status":"TRADING","baseAsset":"BTC","quoteAsset":"USDT"},
            {"symbol":"OLDUSDT","status":"BREAK","baseAsset":"OLD","quoteAsset":"USDT"}
        ]});
        let symbols = parse(Exchange::Binance, MarketKind::Spot, &value).unwrap();
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].symbol, "BTCUSDT");
        let value = json!({"universe": [
            {"name":"BTC"},
            {"name":"OLD","isDelisted":true}
        ]});
        assert_eq!(
            parse(Exchange::Hyperliquid, MarketKind::Perp, &value)
                .unwrap()
                .len(),
            1
        );
        let gate = json!([
            {"id":"BTC_USDT","base":"BTC","quote":"USDT","trade_status":"tradable"},
            {"id":"OLD_USDT","base":"OLD","quote":"USDT","trade_status":"untradable"}
        ]);
        assert_eq!(
            parse(Exchange::Gate, MarketKind::Spot, &gate).unwrap()[0].symbol,
            "BTC_USDT"
        );
        assert_eq!(
            parse(Exchange::Gate, MarketKind::Spot, &gate)
                .unwrap()
                .len(),
            1
        );
        let lighter = json!({"order_books":[
            {"symbol":"BTC","market_id":1,"market_type":"perp","status":"active"},
            {"symbol":"ETH/USDC","market_id":2048,"market_type":"spot","status":"active"},
            {"symbol":"OLD","market_id":9,"status":"inactive"}
        ]});
        let perps = parse(Exchange::Lighter, MarketKind::Perp, &lighter).unwrap();
        let spot = parse(Exchange::Lighter, MarketKind::Spot, &lighter).unwrap();
        assert_eq!(perps[0].market_id, Some(1));
        assert_eq!(spot[0].quote, "USDC");
    }

    #[test]
    fn perpetual_catalogs_keep_contract_metadata_and_spot_is_separate() {
        let binance = json!({"symbols":[
            {"symbol":"BTCUSDT","status":"TRADING","contractType":"PERPETUAL","baseAsset":"BTC","quoteAsset":"USDT"},
            {"symbol":"BTCUSDT_261225","status":"TRADING","contractType":"CURRENT_QUARTER","baseAsset":"BTC","quoteAsset":"USDT"}
        ]});
        assert_eq!(
            parse(Exchange::Binance, MarketKind::Perp, &binance)
                .unwrap()
                .len(),
            1
        );

        let okx = json!({"data":[
            {"instId":"BTC-USDT-SWAP","state":"live","ctType":"linear","ctValCcy":"BTC","settleCcy":"USDT","ctVal":"0.01"},
            {"instId":"BTC-USD-SWAP","state":"live","ctType":"inverse","ctValCcy":"USD","settleCcy":"BTC","ctVal":"100"}
        ]});
        assert_eq!(
            parse(Exchange::Okx, MarketKind::Perp, &okx).unwrap().len(),
            1
        );
        let symbol = &parse(Exchange::Okx, MarketKind::Perp, &okx).unwrap()[0];
        assert_eq!(symbol.base, "BTC");
        assert_eq!(symbol.size_multiplier.as_deref(), Some("0.01"));

        let bybit = json!({"result":{"list":[
            {"symbol":"BTCUSDT","status":"Trading","contractType":"LinearPerpetual","baseCoin":"BTC","quoteCoin":"USDT"},
            {"symbol":"BTCUSDT-26DEC26","status":"Trading","contractType":"LinearFutures","baseCoin":"BTC","quoteCoin":"USDT"}
        ]}});
        assert_eq!(
            parse(Exchange::Bybit, MarketKind::Perp, &bybit)
                .unwrap()
                .len(),
            1
        );

        let hyperliquid = json!({"tokens":[{"index":0,"name":"USDC"},{"index":1,"name":"PURR"}],"universe":[{"name":"PURR/USDC","tokens":[1,0]}]});
        let symbol = &parse(Exchange::Hyperliquid, MarketKind::Spot, &hyperliquid).unwrap()[0];
        assert_eq!((&*symbol.base, &*symbol.quote), ("PURR", "USDC"));
        assert_eq!(symbol.base_token_id, Some(1));

        let gate = json!([{"name":"BTC_USDT","status":"trading","type":"direct","quanto_multiplier":"0.0001"}]);
        assert_eq!(
            parse(Exchange::Gate, MarketKind::Perp, &gate).unwrap()[0]
                .size_multiplier
                .as_deref(),
            Some("0.0001")
        );
    }
}
