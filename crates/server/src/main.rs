mod accounts;
mod catalog;
mod exchange;

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::IntoResponse,
    routing::{any, get},
};
use terminal_core::{
    Account, AccountState, BestBidAsk, Book, BookChange, Candle, ClientMessage, Exchange,
    LastPrice, Market, MarketKind, OwnFill, ServerMessage, SymbolInfo, Trade,
};
use tokio::{
    sync::{Mutex, RwLock, broadcast, mpsc, watch},
    task::JoinHandle,
};
use tower_http::services::ServeDir;

#[derive(Clone, Default)]
struct MarketState {
    candles: Vec<Candle>,
    latest_candle_trade_ms: Option<i64>,
    book: Option<Book>,
    best_bid_ask: Option<BestBidAsk>,
    last_price: Option<LastPrice>,
    connected: bool,
}

impl MarketState {
    fn apply_trade_to_candle(&mut self, trade: &Trade) -> Option<Candle> {
        let price = trade.price.parse::<f64>().ok()?;
        let size = trade.size.parse::<f64>().ok()?;
        if !price.is_finite() || price <= 0.0 || !size.is_finite() || size <= 0.0 {
            return None;
        }
        let minute = trade.time_ms.div_euclid(60_000) * 60;
        match self.candles.last_mut() {
            Some(candle) if minute < candle.time => return None,
            Some(candle) if minute == candle.time => {
                candle.high = candle.high.max(price);
                candle.low = candle.low.min(price);
                candle.volume += size;
                if self
                    .latest_candle_trade_ms
                    .is_none_or(|previous| trade.time_ms >= previous)
                {
                    candle.close = price;
                }
            }
            _ => {
                self.candles.push(Candle {
                    time: minute,
                    open: price,
                    high: price,
                    low: price,
                    close: price,
                    volume: size,
                });
                if self.candles.len() > 300 {
                    self.candles.remove(0);
                }
            }
        }
        self.latest_candle_trade_ms = Some(
            self.latest_candle_trade_ms
                .map_or(trade.time_ms, |previous| previous.max(trade.time_ms)),
        );
        self.candles.last().cloned()
    }
}

struct Worker {
    users: usize,
    books: JoinHandle<()>,
    candles: JoinHandle<()>,
}

#[derive(Clone)]
struct CatalogEntry {
    symbols: Vec<SymbolInfo>,
    fetched_at: Instant,
}

#[derive(Clone)]
struct AppState {
    markets: Arc<RwLock<HashMap<Market, MarketState>>>,
    workers: Arc<Mutex<HashMap<Market, Worker>>>,
    catalogs: Arc<RwLock<HashMap<(Exchange, MarketKind), CatalogEntry>>>,
    http: reqwest::Client,
    updates: broadcast::Sender<ServerMessage>,
    accounts: Arc<RwLock<HashMap<Account, AccountState>>>,
    account_fills: Arc<RwLock<HashMap<Account, Vec<OwnFill>>>>,
    account_users: Arc<Mutex<HashMap<Account, usize>>>,
    account_control: watch::Sender<Vec<Account>>,
}

impl AppState {
    async fn acquire_account(&self, account: Account) {
        let mut users = self.account_users.lock().await;
        *users.entry(account).or_default() += 1;
        let mut accounts: Vec<_> = users.keys().cloned().collect();
        accounts.sort_by(|left, right| left.address.cmp(&right.address));
        self.account_control.send_replace(accounts);
    }

    async fn release_account(&self, account: &Account) {
        let mut users = self.account_users.lock().await;
        if let Some(count) = users.get_mut(account) {
            *count -= 1;
            if *count == 0 {
                users.remove(account);
                self.accounts.write().await.remove(account);
                self.account_fills.write().await.remove(account);
            }
        }
        let mut accounts: Vec<_> = users.keys().cloned().collect();
        accounts.sort_by(|left, right| left.address.cmp(&right.address));
        self.account_control.send_replace(accounts);
    }

    async fn account_snapshot(&self, account: &Account) -> ServerMessage {
        ServerMessage::AccountState {
            account: account.clone(),
            state: self
                .accounts
                .read()
                .await
                .get(account)
                .cloned()
                .unwrap_or_default(),
        }
    }

    async fn publish_account(&self, account: &Account, state: AccountState) {
        self.accounts
            .write()
            .await
            .insert(account.clone(), state.clone());
        let _ = self.updates.send(ServerMessage::AccountState {
            account: account.clone(),
            state,
        });
    }

    async fn fills_snapshot(&self, account: &Account) -> ServerMessage {
        ServerMessage::AccountFills {
            account: account.clone(),
            fills: self
                .account_fills
                .read()
                .await
                .get(account)
                .cloned()
                .unwrap_or_default(),
            snapshot: true,
        }
    }

    async fn publish_fills(&self, account: &Account, fills: Vec<OwnFill>, snapshot: bool) {
        let mut histories = self.account_fills.write().await;
        let history = histories.entry(account.clone()).or_default();
        let added = terminal_core::merge_account_fills(history, fills);
        let fills = if snapshot { history.clone() } else { added };
        drop(histories);
        if snapshot || !fills.is_empty() {
            let _ = self.updates.send(ServerMessage::AccountFills {
                account: account.clone(),
                fills,
                snapshot,
            });
        }
    }

    async fn acquire(&self, market: Market) {
        let mut workers = self.workers.lock().await;
        if let Some(worker) = workers.get_mut(&market) {
            worker.users += 1;
            return;
        }
        let books = tokio::spawn(exchange::run_books(market.clone(), self.clone()));
        let candles = tokio::spawn(exchange::run_candles(
            market.clone(),
            self.clone(),
            self.http.clone(),
        ));
        workers.insert(
            market,
            Worker {
                users: 1,
                books,
                candles,
            },
        );
    }

    async fn release(&self, market: &Market) {
        let mut workers = self.workers.lock().await;
        let Some(worker) = workers.get_mut(market) else {
            return;
        };
        if worker.users > 1 {
            worker.users -= 1;
            return;
        }
        if let Some(worker) = workers.remove(market) {
            worker.books.abort();
            worker.candles.abort();
            self.markets.write().await.remove(market);
        }
    }

    async fn symbols(
        &self,
        exchange: Exchange,
        kind: MarketKind,
    ) -> Result<Vec<SymbolInfo>, String> {
        let key = (exchange, kind);
        let previous = self.catalogs.read().await.get(&key).cloned();
        if let Some(entry) = &previous
            && entry.fetched_at.elapsed() < Duration::from_secs(15 * 60)
        {
            return Ok(entry.symbols.clone());
        }
        match catalog::fetch(exchange, kind, &self.http).await {
            Ok(symbols) => {
                self.catalogs.write().await.insert(
                    key,
                    CatalogEntry {
                        symbols: symbols.clone(),
                        fetched_at: Instant::now(),
                    },
                );
                Ok(symbols)
            }
            Err(error) => {
                previous.map_or_else(|| Err(error.to_string()), |entry| Ok(entry.symbols))
            }
        }
    }

    async fn snapshot(&self, market: &Market) -> ServerMessage {
        let state = self
            .markets
            .read()
            .await
            .get(market)
            .cloned()
            .unwrap_or_default();
        ServerMessage::Snapshot {
            market: market.clone(),
            candles: state.candles,
            book: state.book,
            best_bid_ask: state.best_bid_ask,
            last_price: state.last_price,
            connected: state.connected,
        }
    }

    async fn publish_book(&self, market: &Market, book: Book) {
        let mut markets = self.markets.write().await;
        let state = markets.entry(market.clone()).or_default();
        state.book = Some(book.clone());
        let became_connected = !state.connected;
        state.connected = true;
        drop(markets);
        if became_connected {
            let _ = self.updates.send(ServerMessage::Status {
                market: market.clone(),
                connected: true,
            });
        }
        let _ = self.updates.send(ServerMessage::Book {
            market: market.clone(),
            book,
        });
    }

    async fn publish_book_delta(
        &self,
        market: &Market,
        book: Book,
        bids: Vec<BookChange>,
        asks: Vec<BookChange>,
    ) {
        let updated_at_ms = book.updated_at_ms;
        self.markets
            .write()
            .await
            .entry(market.clone())
            .or_default()
            .book = Some(book);
        let _ = self.updates.send(ServerMessage::BookDelta {
            market: market.clone(),
            bids,
            asks,
            updated_at_ms,
        });
    }

    async fn publish_best_bid_ask(&self, market: &Market, quote: BestBidAsk) {
        let mut markets = self.markets.write().await;
        let state = markets.entry(market.clone()).or_default();
        if state
            .best_bid_ask
            .as_ref()
            .is_some_and(|previous| quote.time_ms < previous.time_ms)
        {
            return;
        }
        state.best_bid_ask = Some(quote.clone());
        let became_connected = !state.connected;
        state.connected = true;
        drop(markets);
        if became_connected {
            let _ = self.updates.send(ServerMessage::Status {
                market: market.clone(),
                connected: true,
            });
        }
        let _ = self.updates.send(ServerMessage::BestBidAsk {
            market: market.clone(),
            quote,
        });
    }

    async fn publish_trades(&self, market: &Market, trades: Vec<Trade>) {
        let Some(latest) = trades.iter().max_by_key(|trade| trade.time_ms) else {
            return;
        };
        let mut markets = self.markets.write().await;
        let state = markets.entry(market.clone()).or_default();
        if state
            .last_price
            .as_ref()
            .is_none_or(|previous| latest.time_ms >= previous.time_ms)
        {
            state.last_price = Some(LastPrice {
                price: latest.price.clone(),
                time_ms: latest.time_ms,
            });
        }
        let mut ordered = trades.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|trade| trade.time_ms);
        let mut live_candle = None;
        for trade in ordered {
            if let Some(candle) = state.apply_trade_to_candle(trade) {
                live_candle = Some(candle);
            }
        }
        drop(markets);
        if let Some(candle) = live_candle {
            let _ = self.updates.send(ServerMessage::Candle {
                market: market.clone(),
                candle,
            });
        }
        let _ = self.updates.send(ServerMessage::Trades {
            market: market.clone(),
            trades,
        });
    }

    async fn disconnected(&self, market: &Market) {
        let mut markets = self.markets.write().await;
        let state = markets.entry(market.clone()).or_default();
        state.connected = false;
        state.book = None;
        state.best_bid_ask = None;
        drop(markets);
        let _ = self.updates.send(ServerMessage::Status {
            market: market.clone(),
            connected: false,
        });
    }

    async fn publish_candles(
        &self,
        market: &Market,
        mut candles: Vec<Candle>,
        requested_at_ms: i64,
    ) {
        if candles.is_empty() {
            return;
        }
        let mut markets = self.markets.write().await;
        let state = markets.entry(market.clone()).or_default();
        if let (Some(live_time_ms), Some(live)) =
            (state.latest_candle_trade_ms, state.candles.last().cloned())
            && live.time == live_time_ms.div_euclid(60_000) * 60
        {
            match candles.last_mut() {
                Some(rest)
                    if rest.time == live.time
                        && live.time >= requested_at_ms.div_euclid(60_000) * 60 =>
                {
                    rest.high = rest.high.max(live.high);
                    rest.low = rest.low.min(live.low);
                    rest.close = live.close;
                    rest.volume = rest.volume.max(live.volume);
                }
                Some(rest) if rest.time < live.time => candles.push(live),
                _ => {}
            }
        }
        if candles.len() > 300 {
            candles.drain(..candles.len() - 300);
        }
        let old = &state.candles;
        let history_changed = old.len() != candles.len()
            || old
                .iter()
                .take(old.len().saturating_sub(1))
                .ne(candles.iter().take(candles.len().saturating_sub(1)));
        let changed = old.last() != candles.last();
        let latest = candles.last().expect("non-empty candles").clone();
        state.candles = candles;
        let snapshot = history_changed.then(|| ServerMessage::Snapshot {
            market: market.clone(),
            candles: state.candles.clone(),
            book: state.book.clone(),
            best_bid_ask: state.best_bid_ask.clone(),
            last_price: state.last_price.clone(),
            connected: state.connected,
        });
        drop(markets);
        if let Some(snapshot) = snapshot {
            let _ = self.updates.send(snapshot);
        } else if changed {
            let _ = self.updates.send(ServerMessage::Candle {
                market: market.clone(),
                candle: latest,
            });
        }
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn send(socket: &mut WebSocket, message: &ServerMessage) -> Result<(), axum::Error> {
    let json = serde_json::to_string(message).expect("server message serializes");
    socket.send(Message::Text(json.into())).await
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut selected = HashSet::<Market>::new();
    let mut selected_accounts = HashSet::<Account>::new();
    let mut updates = state.updates.subscribe();
    let (catalog_tx, mut catalog_rx) = mpsc::unbounded_channel::<ServerMessage>();

    'connection: loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(message) = serde_json::from_str::<ClientMessage>(&text) {
                        match message {
                            ClientMessage::SubscribeAccount { account } => {
                                if !account.valid() { continue; }
                                if selected_accounts.insert(account.clone()) {
                                    state.acquire_account(account.clone()).await;
                                }
                                if send(&mut socket, &state.account_snapshot(&account).await).await.is_err() {
                                    break 'connection;
                                }
                                if send(&mut socket, &state.fills_snapshot(&account).await).await.is_err() {
                                    break 'connection;
                                }
                            }
                            ClientMessage::UnsubscribeAccount { account } => {
                                if selected_accounts.remove(&account) {
                                    state.release_account(&account).await;
                                }
                            }
                            ClientMessage::Subscribe { market } => {
                                if selected.insert(market.clone()) {
                                    state.acquire(market.clone()).await;
                                }
                                if send(&mut socket, &state.snapshot(&market).await).await.is_err() {
                                    break 'connection;
                                }
                            }
                            ClientMessage::Unsubscribe { market } => {
                                if selected.remove(&market) {
                                    state.release(&market).await;
                                }
                            }
                            ClientMessage::Symbols { exchange, kind } => {
                                let state = state.clone();
                                let tx = catalog_tx.clone();
                                tokio::spawn(async move {
                                    let result = state.symbols(exchange, kind).await;
                                    let (symbols, error) = match result {
                                        Ok(symbols) => (symbols, None),
                                        Err(error) => (Vec::new(), Some(error)),
                                    };
                                    let _ = tx.send(ServerMessage::Symbols { exchange, kind, symbols, error });
                                });
                            }
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break 'connection,
                _ => {}
            },
            Some(catalog) = catalog_rx.recv() => {
                if send(&mut socket, &catalog).await.is_err() {
                    break 'connection;
                }
            },
            event = updates.recv(), if !selected.is_empty() || !selected_accounts.is_empty() => match event {
                Ok(event) if event.market().is_some_and(|market| selected.contains(market))
                    || matches!(&event, ServerMessage::AccountState { account, .. } | ServerMessage::AccountFills { account, .. } if selected_accounts.contains(account)) => {
                    if send(&mut socket, &event).await.is_err() {
                        break 'connection;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    for market in &selected {
                        if send(&mut socket, &state.snapshot(market).await).await.is_err() {
                            break 'connection;
                        }
                    }
                    for account in &selected_accounts {
                        if send(&mut socket, &state.account_snapshot(account).await).await.is_err() {
                            break 'connection;
                        }
                        if send(&mut socket, &state.fills_snapshot(account).await).await.is_err() {
                            break 'connection;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break 'connection,
                _ => {}
            },
        }
    }
    for market in selected {
        state.release(&market).await;
    }
    for account in selected_accounts {
        state.release_account(&account).await;
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (updates, _) = broadcast::channel(512);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let (account_control, account_rx) = watch::channel(Vec::new());
    let state = AppState {
        markets: Arc::default(),
        workers: Arc::default(),
        catalogs: Arc::default(),
        http,
        updates,
        accounts: Arc::default(),
        account_fills: Arc::default(),
        account_users: Arc::default(),
        account_control,
    };
    tokio::spawn(accounts::run(state.clone(), account_rx));

    let dist = std::env::var("TERMINAL_WEB_DIST").unwrap_or_else(|_| "crates/web/dist".to_owned());
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/ws", any(ws_handler))
        .fallback_service(ServeDir::new(dist))
        .with_state(state);
    let address: SocketAddr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3000".to_owned())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!("Rust Terminal listening on http://{address}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_core::TradeSide;

    fn test_state() -> AppState {
        let (updates, _) = broadcast::channel(16);
        let (account_control, _) = watch::channel(Vec::new());
        AppState {
            markets: Arc::default(),
            workers: Arc::default(),
            catalogs: Arc::default(),
            http: reqwest::Client::new(),
            updates,
            accounts: Arc::default(),
            account_fills: Arc::default(),
            account_users: Arc::default(),
            account_control,
        }
    }

    #[tokio::test]
    async fn fill_fanout_sends_deltas_and_reconnect_snapshot_without_duplicates() {
        let state = test_state();
        let account = Account {
            exchange: Exchange::Hyperliquid,
            address: "0x1234".into(),
        };
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
        let mut events = state.updates.subscribe();
        state
            .publish_fills(&account, vec![fill.clone()], false)
            .await;
        assert!(
            matches!(events.recv().await.unwrap(), ServerMessage::AccountFills { fills, snapshot: false, .. } if fills.len() == 1)
        );
        state
            .publish_fills(&account, vec![fill.clone()], false)
            .await;
        assert!(events.try_recv().is_err());
        assert!(
            matches!(state.fills_snapshot(&account).await, ServerMessage::AccountFills { fills, snapshot: true, .. } if fills == vec![fill])
        );
    }

    #[tokio::test]
    async fn trade_updates_current_chart_candle_before_next_rest_fetch() {
        let state = test_state();
        let market = Market::default();
        state
            .publish_candles(
                &market,
                vec![Candle {
                    time: 120,
                    open: 100.0,
                    high: 101.0,
                    low: 99.0,
                    close: 100.0,
                    volume: 10.0,
                }],
                120_000,
            )
            .await;
        let mut events = state.updates.subscribe();
        state
            .publish_trades(
                &market,
                vec![Trade {
                    price: "102".to_owned(),
                    size: "2".to_owned(),
                    time_ms: 123_000,
                    side: TradeSide::Buy,
                }],
            )
            .await;
        let candle = state.snapshot(&market).await;
        let ServerMessage::Snapshot { candles, .. } = candle else {
            panic!("expected market snapshot");
        };
        assert_eq!(candles.last().unwrap().close, 102.0);
        assert_eq!(candles.last().unwrap().high, 102.0);
        assert_eq!(candles.last().unwrap().volume, 12.0);
        assert!(matches!(
            events.try_recv(),
            Ok(ServerMessage::Candle { .. })
        ));
    }

    #[tokio::test]
    async fn rest_history_keeps_live_candle_and_finalizes_previous_minute() {
        let state = test_state();
        let market = Market::default();
        state
            .publish_trades(
                &market,
                vec![Trade {
                    price: "102".to_owned(),
                    size: "2".to_owned(),
                    time_ms: 123_000,
                    side: TradeSide::Buy,
                }],
            )
            .await;
        state
            .publish_candles(
                &market,
                vec![
                    Candle {
                        time: 60,
                        open: 98.0,
                        high: 99.0,
                        low: 97.0,
                        close: 98.0,
                        volume: 5.0,
                    },
                    Candle {
                        time: 120,
                        open: 100.0,
                        high: 101.0,
                        low: 99.0,
                        close: 100.0,
                        volume: 10.0,
                    },
                ],
                122_000,
            )
            .await;
        let ServerMessage::Snapshot { candles, .. } = state.snapshot(&market).await else {
            panic!("expected market snapshot");
        };
        assert_eq!(candles.len(), 2);
        assert_eq!(candles[1].open, 100.0);
        assert_eq!(candles[1].close, 102.0);
        assert_eq!(candles[1].high, 102.0);
        assert_eq!(candles[1].volume, 10.0);

        state
            .publish_trades(
                &market,
                vec![Trade {
                    price: "103".to_owned(),
                    size: "1".to_owned(),
                    time_ms: 181_000,
                    side: TradeSide::Buy,
                }],
            )
            .await;
        state
            .publish_candles(
                &market,
                vec![
                    Candle {
                        time: 120,
                        open: 100.0,
                        high: 102.0,
                        low: 99.0,
                        close: 102.0,
                        volume: 12.0,
                    },
                    Candle {
                        time: 180,
                        open: 101.0,
                        high: 102.0,
                        low: 101.0,
                        close: 102.0,
                        volume: 4.0,
                    },
                ],
                180_000,
            )
            .await;
        let ServerMessage::Snapshot { candles, .. } = state.snapshot(&market).await else {
            panic!("expected market snapshot");
        };
        assert_eq!(candles.last().unwrap().time, 180);
        assert_eq!(candles.last().unwrap().close, 103.0);
        assert_eq!(candles.last().unwrap().high, 103.0);
        assert_eq!(candles[0].volume, 12.0);

        state
            .publish_candles(
                &market,
                vec![Candle {
                    time: 180,
                    open: 101.0,
                    high: 104.0,
                    low: 101.0,
                    close: 104.0,
                    volume: 8.0,
                }],
                240_000,
            )
            .await;
        let ServerMessage::Snapshot { candles, .. } = state.snapshot(&market).await else {
            panic!("expected market snapshot");
        };
        assert_eq!(candles.last().unwrap().close, 104.0);
        assert_eq!(candles.last().unwrap().volume, 8.0);
    }
}
