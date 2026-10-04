# Rust Terminal

A real-time crypto market-data terminal built in Rust. The interface runs in the browser through WebAssembly; a Rust server connects to exchange feeds and keeps the order books in sync. No JavaScript framework.

Connect a Hyperliquid wallet address from the account button beside **Add widget** to see live open orders and perpetual positions. CHART marks orders and position entries; BOOK highlights own orders at levels present in Hyperliquid's depth feed. PRICES marks open orders for its selected sources; its config can hide them per widget. The account menu lists all open orders, including those beyond the book feed's 20 levels. Use the eye beside an account to hide its widget overlays without disconnecting it. This is read-only: no signing or API key is needed. Multiple wallet addresses can be saved locally in the browser. Hyperliquid limits live user subscriptions to 10 distinct addresses.

![Terminal with charts, price comparison, order books, and trades tape](docs/screenshots/overview.jpg)

## What it does

- **Twelve exchanges:** Binance, OKX, Bybit, Hyperliquid, Gate, Lighter, Bitget, Aster, Bitunix, KuCoin, Kraken, and Pacifica. Choose Spot or Perp and a symbol separately for each widget.
- **Four widgets:** live candlestick chart, scrollable order book, multi-venue price comparison, and trades tape.
- **Flexible workspace:** drag widgets, resize tiles, and switch between saved terminal layouts. Settings are stored locally in the browser.
- **Live data:** exchange WebSockets for books and trades; REST snapshots for initial book depth and candle history.

Drag CHART or PRICES vertically to move the price range. Scroll over the right price scale to zoom it, or use Shift + scroll over CHART; PRICES also supports scrolling over the plot. Double-click the price scale to restore automatic scaling.

<img src="docs/screenshots/market-selection.jpg" alt="Per-widget exchange, market type, and symbol selection" width="520">

## Run locally

Requires Rust and [Trunk](https://github.com/trunk-rs/trunk).

```sh
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
cd crates/web && trunk build --release && cd ../..
cargo run -p terminal-server --release
```

Open **http://127.0.0.1:3000**. Run `cargo test --workspace` to check the project. The same interface can also run as a native app with `cargo run -p terminal-web` while the server is running.

## Scope

Perp support covers linear contracts: Binance USDⓈ-M, OKX linear swaps, Bybit linear, Gate USDT, Hyperliquid, Lighter, Bitget USDT, Aster, Bitunix USDT, KuCoin linear, Kraken `PF_*`, and Pacifica. Inverse contracts are not included. Binance and Aster Perp publish aggregate trade events, so their tapes show each published aggregate rather than every underlying execution. Bitunix Spot currently provides a polled public order book and candle history; its public API does not expose a trade stream, so its trade tape and Last Trades comparison are unavailable. Order books contain the price levels available from each exchange's public feed; they do not expose individual orders. The terminal displays public market data and does not place trades.

KuCoin uses its public UTA feed with 500 levels per side and up to 10 ms book updates. Kraken Spot provides 1,000 levels with checksum validation; its linear Perp feed supplies a snapshot and sequenced updates. Pacifica supports both Spot and Perp, with a dedicated event-driven BBO feed for PRICES. See [API notes](docs/exchanges/kucoin-kraken-pacifica.md) for source documentation and depth limits.

## Project

- `crates/core` — shared market and WebSocket protocol types
- `crates/server` — exchange adapters, order books, and data fanout
- `crates/web` — `egui` interface for WebAssembly and native desktop

MIT licensed. See [LICENSE](LICENSE).
