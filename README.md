# Rust Terminal

A real-time crypto market-data terminal built in Rust. The interface runs in the browser through WebAssembly; a Rust server connects to exchange feeds and keeps the order books in sync. No JavaScript framework.

![Terminal with charts, price comparison, order books, and trades tape](docs/screenshots/overview.jpg)

## What it does

- **Six exchanges:** Binance, OKX, Bybit, Hyperliquid, Gate, and Lighter. Choose Spot or Perp and a symbol separately for each widget.
- **Four widgets:** live candlestick chart, scrollable order book, multi-venue price comparison, and trades tape.
- **Flexible workspace:** drag widgets, resize tiles, and switch between saved terminal layouts. Settings are stored locally in the browser.
- **Live data:** exchange WebSockets for books and trades; REST snapshots for initial book depth and candle history.

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

Perp support covers linear contracts: Binance USDⓈ-M, OKX linear swaps, Bybit linear, Gate USDT, Hyperliquid, and Lighter. Inverse contracts are not included. Binance Perp publishes aggregate trade events, so its tape shows each published aggregate rather than every underlying execution. Order books contain the price levels available from each exchange's public feed; they do not expose individual orders. The terminal displays public market data and does not place trades.

## Project

- `crates/core` — shared market and WebSocket protocol types
- `crates/server` — exchange adapters, order books, and data fanout
- `crates/web` — `egui` interface for WebAssembly and native desktop

MIT licensed. See [LICENSE](LICENSE).
