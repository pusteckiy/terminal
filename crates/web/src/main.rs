mod view;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    str::FromStr,
    time::Duration,
};

use eframe::egui::{self, Color32, RichText, Sense};
use egui_tiles::{Behavior, Container, Linear, LinearDir, Tile, TileId, Tiles, Tree, UiResponse};
use ewebsock::{WsEvent, WsMessage, WsReceiver, WsSender};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};
use terminal_core::{
    Account, AccountState, BestBidAsk, Book, BookChange, Candle, ClientMessage, Exchange,
    LastPrice, Level, Market, MarketKind, ServerMessage, SymbolInfo, Trade, TradeSide,
};

// Dark neutral surfaces and control states follow the shadcn/ui color roles.
// Trading colors stay independent so buy/sell and chart series remain distinct.
const BG: Color32 = Color32::from_rgb(23, 23, 23);
const SURFACE: Color32 = Color32::from_rgb(32, 32, 32);
const RAISED: Color32 = Color32::from_rgb(41, 41, 41);
const HOVER: Color32 = Color32::from_rgb(53, 53, 53);
const BORDER: Color32 = Color32::from_rgb(57, 57, 57);
const TEXT: Color32 = Color32::from_rgb(245, 245, 245);
const MUTED: Color32 = Color32::from_rgb(163, 163, 163);
const GREEN: Color32 = Color32::from_rgb(93, 213, 172);
const RED: Color32 = Color32::from_rgb(239, 111, 123);
const MAX_PRICE_HISTORY_MS: i64 = 600_000;
const MAX_TRADE_POINTS: usize = 200_000;
const MAX_QUOTE_SAMPLES: usize = 12_100;
const MAX_TAPE_TRADES: usize = 500;
const TAB_DELETE_HOVER_SECS: f64 = 0.45;

const fn default_compare_window_secs() -> u32 {
    60
}

fn terminal_visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = SURFACE;
    visuals.window_stroke = egui::Stroke::new(1.0, BORDER);
    visuals.window_corner_radius = egui::CornerRadius::same(8);
    visuals.menu_corner_radius = egui::CornerRadius::same(8);
    visuals.override_text_color = Some(TEXT);
    visuals.weak_text_color = Some(MUTED);
    visuals.faint_bg_color = SURFACE;
    visuals.extreme_bg_color = BG;
    visuals.text_edit_bg_color = Some(BG);
    visuals.code_bg_color = RAISED;
    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = egui::CornerRadius::same(6);
        widget.bg_stroke = egui::Stroke::new(1.0, BORDER);
        widget.fg_stroke = egui::Stroke::new(1.0, TEXT);
    }
    visuals.widgets.noninteractive.bg_fill = BG;
    visuals.widgets.noninteractive.weak_bg_fill = BG;
    visuals.widgets.inactive.bg_fill = RAISED;
    visuals.widgets.inactive.weak_bg_fill = RAISED;
    visuals.widgets.hovered.bg_fill = HOVER;
    visuals.widgets.hovered.weak_bg_fill = HOVER;
    visuals.widgets.active.bg_fill = HOVER;
    visuals.widgets.active.weak_bg_fill = HOVER;
    visuals.widgets.open.bg_fill = RAISED;
    visuals.widgets.open.weak_bg_fill = RAISED;
    visuals.selection.bg_fill = HOVER;
    visuals.selection.stroke = egui::Stroke::new(1.0, TEXT);
    visuals
}

#[derive(Clone, Copy, Serialize, Deserialize)]
enum WidgetKind {
    Chart,
    Book,
    Compare,
    Tape,
}

impl WidgetKind {
    fn label(self) -> &'static str {
        match self {
            Self::Chart => "OHLCV chart",
            Self::Book => "Order book",
            Self::Compare => "Price comparison",
            Self::Tape => "Trades tape",
        }
    }

    fn short_label(self) -> &'static str {
        match self {
            Self::Chart => "CHART",
            Self::Book => "BOOK",
            Self::Compare => "PRICES",
            Self::Tape => "TRADES",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum CompareMode {
    #[default]
    Trades,
    BestBidAsk,
}

#[derive(Serialize)]
struct Pane {
    kind: WidgetKind,
    market: Market,
    series: Vec<Market>,
    compare_percent: bool,
    compare_mode: CompareMode,
    compare_window_secs: u32,
    #[serde(skip)]
    chart: view::ChartView,
    #[serde(skip)]
    book_view: view::BookView,
    #[serde(skip)]
    legacy: bool,
    #[serde(skip)]
    search: String,
    #[serde(skip)]
    editing_series: usize,
}

impl Pane {
    fn new(kind: WidgetKind, market: Market) -> Self {
        let series = if matches!(kind, WidgetKind::Compare | WidgetKind::Tape) {
            vec![
                Market::for_exchange(Exchange::Binance),
                Market::for_exchange(Exchange::Hyperliquid),
            ]
        } else {
            Vec::new()
        };
        Self {
            kind,
            market,
            series,
            compare_percent: false,
            compare_mode: CompareMode::default(),
            compare_window_secs: default_compare_window_secs(),
            chart: view::ChartView::default(),
            book_view: view::BookView::default(),
            legacy: false,
            search: String::new(),
            editing_series: 0,
        }
    }
}

// The previous release saved "Chart" or "Book" as each pane. Keep those layouts.
impl<'de> Deserialize<'de> for Pane {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum SavedPane {
            Current {
                kind: WidgetKind,
                market: Market,
                #[serde(default)]
                series: Vec<Market>,
                #[serde(default)]
                compare_percent: bool,
                #[serde(default)]
                compare_mode: CompareMode,
                #[serde(default = "default_compare_window_secs")]
                compare_window_secs: u32,
            },
            Legacy(WidgetKind),
        }
        Ok(match SavedPane::deserialize(deserializer)? {
            SavedPane::Current {
                kind,
                market,
                series,
                compare_percent,
                compare_mode,
                compare_window_secs,
            } => {
                let mut pane = Pane::new(kind, market);
                if matches!(kind, WidgetKind::Compare | WidgetKind::Tape) && !series.is_empty() {
                    pane.series = series;
                }
                pane.compare_percent = compare_percent;
                pane.compare_mode = compare_mode;
                pane.compare_window_secs = compare_window_secs.clamp(10, 600);
                pane
            }
            SavedPane::Legacy(kind) => {
                let mut pane = Pane::new(kind, Market::default());
                pane.legacy = true;
                pane
            }
        })
    }
}

#[derive(Default)]
struct MarketData {
    candles: Vec<Candle>,
    book: Option<Book>,
    best_bid_ask: Option<BestBidAsk>,
    last_price: Option<LastPrice>,
    price_trades: VecDeque<TradePoint>,
    trades: VecDeque<Trade>,
    quotes: VecDeque<QuoteTick>,
    connected: bool,
}

#[derive(Clone)]
struct QuoteTick {
    time_ms: i64,
    bid: f64,
    ask: f64,
}

#[derive(Clone, Copy)]
struct TradePoint {
    time_ms: i64,
    price: f64,
    size: f64,
    side: TradeSide,
}

impl MarketData {
    fn push_trade(&mut self, trade: Trade) {
        self.set_price(LastPrice {
            price: trade.price.clone(),
            time_ms: trade.time_ms,
        });
        if let Ok(price) = trade.price.parse::<f64>()
            && price.is_finite()
            && price > 0.0
        {
            self.price_trades.push_back(TradePoint {
                time_ms: trade.time_ms,
                price,
                size: trade.size.parse().unwrap_or(0.0),
                side: trade.side,
            });
            let latest_ms = self
                .last_price
                .as_ref()
                .map_or(trade.time_ms, |last| last.time_ms);
            while self.price_trades.len() > MAX_TRADE_POINTS
                || self
                    .price_trades
                    .front()
                    .is_some_and(|first| latest_ms - first.time_ms > MAX_PRICE_HISTORY_MS)
            {
                self.price_trades.pop_front();
            }
        }
        self.trades.push_back(trade);
        while self.trades.len() > MAX_TAPE_TRADES {
            self.trades.pop_front();
        }
    }

    fn set_price(&mut self, last_price: LastPrice) {
        if self
            .last_price
            .as_ref()
            .is_some_and(|previous| last_price.time_ms < previous.time_ms)
        {
            return;
        }
        self.last_price = Some(last_price);
    }

    fn set_book(&mut self, book: Book, exchange: Exchange) {
        if !matches!(
            exchange,
            Exchange::Gate | Exchange::Hyperliquid | Exchange::Lighter
        ) && let (Some(bid), Some(ask)) = (book.bids.first(), book.asks.first())
            && self
                .best_bid_ask
                .as_ref()
                .is_none_or(|quote| book.updated_at_ms > quote.time_ms)
        {
            self.set_best_bid_ask(BestBidAsk {
                bid: bid.price.clone(),
                ask: ask.price.clone(),
                time_ms: book.updated_at_ms,
            });
        }
        self.book = Some(book);
    }

    fn apply_book_delta(
        &mut self,
        bids: Vec<BookChange>,
        asks: Vec<BookChange>,
        updated_at_ms: i64,
        exchange: Exchange,
    ) {
        let Some(book) = self.book.as_mut() else {
            return;
        };
        apply_book_side(&mut book.bids, bids, true);
        apply_book_side(&mut book.asks, asks, false);
        book.updated_at_ms = updated_at_ms;
        let quote = match (book.bids.first(), book.asks.first()) {
            (Some(bid), Some(ask)) => Some(BestBidAsk {
                bid: bid.price.clone(),
                ask: ask.price.clone(),
                time_ms: updated_at_ms,
            }),
            _ => None,
        };
        if !matches!(
            exchange,
            Exchange::Gate | Exchange::Hyperliquid | Exchange::Lighter
        ) && let Some(quote) = quote
            && self
                .best_bid_ask
                .as_ref()
                .is_none_or(|previous| updated_at_ms > previous.time_ms)
        {
            self.set_best_bid_ask(quote);
        }
    }

    fn set_best_bid_ask(&mut self, quote: BestBidAsk) {
        if self
            .best_bid_ask
            .as_ref()
            .is_some_and(|previous| quote.time_ms < previous.time_ms)
        {
            return;
        }
        let (Ok(bid), Ok(ask)) = (quote.bid.parse::<f64>(), quote.ask.parse::<f64>()) else {
            return;
        };
        if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask <= bid {
            return;
        }
        let tick = QuoteTick {
            time_ms: quote.time_ms,
            bid,
            ask,
        };
        if let Some(previous) = self.quotes.back_mut()
            && tick.time_ms.div_euclid(50) == previous.time_ms.div_euclid(50)
        {
            *previous = tick;
        } else {
            self.quotes.push_back(tick);
        }
        while self.quotes.len() > MAX_QUOTE_SAMPLES
            || self
                .quotes
                .front()
                .is_some_and(|first| quote.time_ms - first.time_ms > MAX_PRICE_HISTORY_MS)
        {
            self.quotes.pop_front();
        }
        self.best_bid_ask = Some(quote);
    }
}

fn apply_book_side(levels: &mut Vec<Level>, changes: Vec<BookChange>, bids: bool) {
    if changes.is_empty() {
        return;
    }
    let mut sizes: HashMap<Decimal, Decimal> = levels
        .iter()
        .filter_map(|level| {
            Some((
                Decimal::from_str(&level.price).ok()?,
                Decimal::from_str(&level.size).ok()?,
            ))
        })
        .collect();
    for change in changes {
        let (Ok(price), Ok(size)) = (
            Decimal::from_str(&change.price),
            Decimal::from_str(&change.size),
        ) else {
            continue;
        };
        if size.is_zero() {
            sizes.remove(&price);
        } else {
            sizes.insert(price, size);
        }
    }
    let mut entries: Vec<_> = sizes.into_iter().collect();
    entries.sort_unstable_by(|a, b| if bids { b.0.cmp(&a.0) } else { a.0.cmp(&b.0) });
    let (mut depth_base, mut depth_quote) = (Decimal::ZERO, Decimal::ZERO);
    *levels = entries
        .into_iter()
        .map(|(price, size)| {
            let quote = price * size;
            depth_base += size;
            depth_quote += quote;
            Level {
                price: price.normalize().to_string(),
                size: size.normalize().to_string(),
                quote_size: quote.normalize().to_string(),
                depth_base: depth_base.normalize().to_string(),
                depth_quote: depth_quote.normalize().to_string(),
            }
        })
        .collect();
}

struct TerminalApp {
    workspaces: Vec<Tree<Pane>>,
    active_workspace: usize,
    tab_hover: Option<(usize, f64)>,
    data: HashMap<Market, MarketData>,
    catalogs: HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
    catalog_errors: HashMap<(Exchange, MarketKind), String>,
    catalog_requests: HashSet<(Exchange, MarketKind)>,
    subscribed: HashSet<Market>,
    accounts: Vec<Account>,
    account_data: HashMap<Account, AccountState>,
    subscribed_accounts: HashSet<Account>,
    account_exchange: Exchange,
    account_address: String,
    account_error: Option<String>,
    focused: Option<TileId>,
    socket_open: bool,
    sender: Option<WsSender>,
    receiver: Option<WsReceiver>,
    reconnect_at: f64,
}

impl TerminalApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(terminal_visuals());

        let old_market: Market = cc
            .storage
            .and_then(|storage| storage.get_string("market"))
            .and_then(|saved| serde_json::from_str(&saved).ok())
            .unwrap_or_default();
        let saved_workspaces = cc
            .storage
            .and_then(|storage| storage.get_string("terminal_workspaces"))
            .and_then(|saved| serde_json::from_str::<Vec<Tree<Pane>>>(&saved).ok())
            .filter(|workspaces| !workspaces.is_empty());
        let mut workspaces: Vec<_> = saved_workspaces
            .unwrap_or_else(|| {
                let first = cc
                    .storage
                    .and_then(|storage| storage.get_string("layout"))
                    .and_then(|saved| serde_json::from_str(&saved).ok())
                    .unwrap_or_else(|| Self::default_tree(0));
                vec![first, Self::default_tree(1), Self::default_tree(2)]
            })
            .into_iter()
            .enumerate()
            .map(|(index, tree)| Self::with_workspace_id(tree, index))
            .collect();
        for tree in &mut workspaces {
            for (_, tile) in tree.tiles.iter_mut() {
                if let Tile::Pane(pane) = tile
                    && pane.legacy
                {
                    pane.market = old_market.clone();
                    pane.legacy = false;
                }
            }
        }
        let active_workspace = cc
            .storage
            .and_then(|storage| storage.get_string("active_terminal"))
            .and_then(|saved| saved.parse::<usize>().ok())
            .filter(|index| *index < workspaces.len())
            .unwrap_or(0);
        let mut app = Self {
            workspaces,
            active_workspace,
            tab_hover: None,
            data: HashMap::new(),
            catalogs: HashMap::new(),
            catalog_errors: HashMap::new(),
            catalog_requests: HashSet::new(),
            subscribed: HashSet::new(),
            accounts: cc
                .storage
                .and_then(|storage| storage.get_string("accounts"))
                .and_then(|saved| serde_json::from_str::<Vec<Account>>(&saved).ok())
                .unwrap_or_default()
                .into_iter()
                .filter(Account::valid)
                .collect(),
            account_data: HashMap::new(),
            subscribed_accounts: HashSet::new(),
            account_exchange: Exchange::Hyperliquid,
            account_address: String::new(),
            account_error: None,
            focused: None,
            socket_open: false,
            sender: None,
            receiver: None,
            reconnect_at: 0.0,
        };
        app.connect(&cc.egui_ctx);
        app
    }

    fn default_tree(index: usize) -> Tree<Pane> {
        let mut tiles = Tiles::default();
        let chart = tiles.insert_pane(Pane::new(WidgetKind::Chart, Market::default()));
        let book = tiles.insert_pane(Pane::new(WidgetKind::Book, Market::default()));
        let root = tiles.insert_new(Tile::Container(Container::Linear(Linear::new_binary(
            LinearDir::Horizontal,
            [chart, book],
            0.70,
        ))));
        Tree::new(format!("terminal-{}", index + 1), root, tiles)
    }

    fn with_workspace_id(tree: Tree<Pane>, index: usize) -> Tree<Pane> {
        let mut unique = Tree::empty(format!("terminal-{}", index + 1));
        unique.root = tree.root;
        unique.tiles = tree.tiles;
        unique
    }

    fn connect(&mut self, ctx: &egui::Context) {
        let wake = ctx.clone();
        match ewebsock::connect_with_wakeup(ws_url(), ewebsock::Options::default(), move || {
            wake.request_repaint()
        }) {
            Ok((sender, receiver)) => {
                self.sender = Some(sender);
                self.receiver = Some(receiver);
                self.socket_open = false;
                self.subscribed.clear();
                self.subscribed_accounts.clear();
                self.catalog_requests.clear();
            }
            Err(_) => {
                self.sender = None;
                self.receiver = None;
                self.socket_open = false;
                self.subscribed.clear();
                self.subscribed_accounts.clear();
                self.catalog_requests.clear();
            }
        }
    }

    fn send(&mut self, message: ClientMessage) {
        if let Some(sender) = &mut self.sender
            && let Ok(text) = serde_json::to_string(&message)
        {
            sender.send(WsMessage::Text(text));
        }
    }

    fn sync_subscriptions(&mut self) {
        if !self.socket_open {
            return;
        }
        let mut wanted = HashSet::<Market>::new();
        for (_, tile) in self.workspaces[self.active_workspace].tiles.iter() {
            if let Tile::Pane(pane) = tile {
                if matches!(pane.kind, WidgetKind::Compare | WidgetKind::Tape) {
                    wanted.extend(pane.series.iter().cloned());
                } else {
                    wanted.insert(pane.market.clone());
                }
            }
        }
        let removed: Vec<_> = self.subscribed.difference(&wanted).cloned().collect();
        let added: Vec<_> = wanted.difference(&self.subscribed).cloned().collect();
        for market in removed {
            self.send(ClientMessage::Unsubscribe {
                market: market.clone(),
            });
            self.data.remove(&market);
        }
        for market in added {
            self.send(ClientMessage::Subscribe { market });
        }
        self.subscribed = wanted;
        let wanted_accounts: HashSet<_> = self.accounts.iter().cloned().collect();
        for account in self
            .subscribed_accounts
            .difference(&wanted_accounts)
            .cloned()
            .collect::<Vec<_>>()
        {
            self.send(ClientMessage::UnsubscribeAccount {
                account: account.clone(),
            });
            self.account_data.remove(&account);
        }
        for account in wanted_accounts
            .difference(&self.subscribed_accounts)
            .cloned()
            .collect::<Vec<_>>()
        {
            self.send(ClientMessage::SubscribeAccount { account });
        }
        self.subscribed_accounts = wanted_accounts;
    }

    fn request_catalogs(
        &mut self,
        needed: HashSet<(Exchange, MarketKind)>,
        retry: HashSet<(Exchange, MarketKind)>,
    ) {
        for key in retry {
            self.catalogs.remove(&key);
            self.catalog_errors.remove(&key);
            self.catalog_requests.remove(&key);
        }
        if self.socket_open {
            for (exchange, kind) in needed {
                if !self.catalogs.contains_key(&(exchange, kind))
                    && self.catalog_requests.insert((exchange, kind))
                {
                    self.send(ClientMessage::Symbols { exchange, kind });
                }
            }
        }
    }

    fn poll_socket(&mut self, ctx: &egui::Context) {
        while let Some(event) = self.receiver.as_ref().and_then(WsReceiver::try_recv) {
            match event {
                WsEvent::Opened => {
                    self.socket_open = true;
                    self.sync_subscriptions();
                }
                WsEvent::Message(WsMessage::Text(text)) => {
                    if let Ok(message) = serde_json::from_str::<ServerMessage>(&text) {
                        if let ServerMessage::Symbols {
                            exchange,
                            kind,
                            symbols,
                            error,
                        } = message
                        {
                            let key = (exchange, kind);
                            if let Some(error) = error {
                                self.catalog_errors.insert(key, error);
                            } else {
                                self.catalog_errors.remove(&key);
                                self.catalogs.insert(key, symbols);
                                self.catalog_requests.remove(&key);
                            }
                            continue;
                        }
                        if let ServerMessage::AccountState { account, state } = message {
                            if self.subscribed_accounts.contains(&account) {
                                self.account_data.insert(account, state);
                            }
                            continue;
                        }
                        let Some(market) = message.market().cloned() else {
                            continue;
                        };
                        if !self.subscribed.contains(&market) {
                            continue;
                        }
                        let exchange = market.exchange;
                        let data = self.data.entry(market).or_default();
                        match message {
                            ServerMessage::Snapshot {
                                candles,
                                book,
                                best_bid_ask,
                                last_price,
                                connected,
                                ..
                            } => {
                                data.candles = candles;
                                if let Some(book) = book {
                                    data.set_book(book, exchange);
                                } else {
                                    data.book = None;
                                }
                                if let Some(quote) = best_bid_ask {
                                    data.set_best_bid_ask(quote);
                                }
                                if let Some(last_price) = last_price {
                                    data.set_price(last_price);
                                }
                                data.connected = connected;
                            }
                            ServerMessage::Book { book, .. } => {
                                data.set_book(book, exchange);
                                data.connected = true;
                            }
                            ServerMessage::BookDelta {
                                bids,
                                asks,
                                updated_at_ms,
                                ..
                            } => {
                                data.apply_book_delta(bids, asks, updated_at_ms, exchange);
                                data.connected = true;
                            }
                            ServerMessage::BestBidAsk { quote, .. } => {
                                data.set_best_bid_ask(quote);
                                data.connected = true;
                            }
                            ServerMessage::Candle { candle, .. } => {
                                if data
                                    .candles
                                    .last()
                                    .is_some_and(|last| last.time == candle.time)
                                {
                                    *data.candles.last_mut().unwrap() = candle;
                                } else {
                                    data.candles.push(candle);
                                    if data.candles.len() > 300 {
                                        data.candles.remove(0);
                                    }
                                }
                            }
                            ServerMessage::LastPrice { last_price, .. } => {
                                data.set_price(last_price);
                            }
                            ServerMessage::Trades { trades, .. } => {
                                for trade in trades {
                                    data.push_trade(trade);
                                }
                            }
                            ServerMessage::Status { connected, .. } => {
                                data.connected = connected;
                                if !connected {
                                    data.book = None;
                                    data.best_bid_ask = None;
                                }
                            }
                            ServerMessage::Symbols { .. } => {}
                            ServerMessage::AccountState { .. } => {}
                        }
                    }
                }
                WsEvent::Closed | WsEvent::Error(_) => {
                    self.socket_open = false;
                    self.subscribed.clear();
                    self.subscribed_accounts.clear();
                    self.account_data.clear();
                    for data in self.data.values_mut() {
                        data.connected = false;
                        data.book = None;
                        data.best_bid_ask = None;
                    }
                    self.sender = None;
                    self.receiver = None;
                    self.reconnect_at = ctx.input(|input| input.time) + 3.0;
                    break;
                }
                _ => {}
            }
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) -> Option<WidgetKind> {
        let mut add = None;
        let mut switch_to = None;
        let mut remove_workspace = None;
        let mut create_workspace = false;
        let now = ui.input(|input| input.time);
        let mut hovered_tab = None;
        let header = ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 36.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add_space(10.0);
                let tabs_width = (ui.available_width() - 140.0).max(100.0);
                let mut tabs = |ui: &mut egui::Ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for index in 0..self.workspaces.len() {
                        let selected = index == self.active_workspace;
                        let button = egui::Button::new(
                            RichText::new((index + 1).to_string())
                                .size(12.0)
                                .color(if selected { BG } else { MUTED }),
                        )
                        .fill(if selected { TEXT } else { RAISED })
                        .stroke(egui::Stroke::new(1.0, if selected { TEXT } else { BORDER }))
                        .corner_radius(egui::CornerRadius::same(6));
                        let shortcut = match index {
                            0..=8 => format!(" · Cmd + Option + {}", index + 1),
                            9 => " · Cmd + Option + 0".to_owned(),
                            _ => String::new(),
                        };
                        let response = ui.add_sized([29.0, 25.0], button);
                        if response.clicked() {
                            switch_to = Some(index);
                        }
                        let badge_center =
                            egui::pos2(response.rect.right() - 5.0, response.rect.top() + 5.0);
                        let badge_rect =
                            egui::Rect::from_center_size(badge_center, egui::vec2(16.0, 16.0));
                        let pointer = ui.input(|input| input.pointer.hover_pos());
                        let hovering = pointer.is_some_and(|pointer| {
                            response.rect.contains(pointer)
                                || (self.tab_hover.map(|(hovered, _)| hovered) == Some(index)
                                    && badge_rect.contains(pointer))
                        });
                        if hovering {
                            hovered_tab = Some(index);
                            if self.tab_hover.map(|(hovered, _)| hovered) != Some(index) {
                                self.tab_hover = Some((index, now));
                            }
                            let started = self.tab_hover.unwrap().1;
                            if self.workspaces.len() > 1 && now - started >= TAB_DELETE_HOVER_SECS {
                                let close = ui.interact(
                                    badge_rect,
                                    response.id.with("delete-terminal"),
                                    Sense::click(),
                                );
                                let painter = ui.painter();
                                painter.circle_filled(
                                    badge_center,
                                    6.0,
                                    if close.hovered() { RED } else { HOVER },
                                );
                                painter.circle_stroke(
                                    badge_center,
                                    6.0,
                                    egui::Stroke::new(
                                        1.0,
                                        if close.hovered() { RED } else { BORDER },
                                    ),
                                );
                                let cross = egui::Stroke::new(1.3, TEXT);
                                painter.line_segment(
                                    [
                                        badge_center + egui::vec2(-2.0, -2.0),
                                        badge_center + egui::vec2(2.0, 2.0),
                                    ],
                                    cross,
                                );
                                painter.line_segment(
                                    [
                                        badge_center + egui::vec2(-2.0, 2.0),
                                        badge_center + egui::vec2(2.0, -2.0),
                                    ],
                                    cross,
                                );
                                if close.clicked() {
                                    remove_workspace = Some(index);
                                }
                                if pointer.is_some_and(|pointer| badge_rect.contains(pointer)) {
                                    close.on_hover_text("Delete terminal");
                                } else {
                                    response.on_hover_text(format!(
                                        "Terminal {}{}",
                                        index + 1,
                                        shortcut
                                    ));
                                }
                            } else if self.workspaces.len() > 1 {
                                ui.ctx().request_repaint_after(Duration::from_secs_f64(
                                    (TAB_DELETE_HOVER_SECS - (now - started)).max(0.0),
                                ));
                                response.on_hover_text(format!(
                                    "Terminal {}{}",
                                    index + 1,
                                    shortcut
                                ));
                            }
                        } else {
                            response.on_hover_text(format!("Terminal {}{}", index + 1, shortcut));
                        }
                    }
                    let button = egui::Button::new(RichText::new("+").size(16.0).color(TEXT))
                        .fill(RAISED)
                        .stroke(egui::Stroke::new(1.0, BORDER))
                        .corner_radius(egui::CornerRadius::same(6));
                    if ui
                        .add_sized([29.0, 25.0], button)
                        .on_hover_text("New terminal")
                        .clicked()
                    {
                        create_workspace = true;
                    }
                };
                let tabs_content_width = (self.workspaces.len() as f32 + 1.0) * 29.0
                    + self.workspaces.len() as f32 * 4.0;
                if tabs_content_width <= tabs_width {
                    tabs(ui);
                } else {
                    egui::ScrollArea::horizontal()
                        .id_salt("terminal-switcher")
                        .max_width(tabs_width)
                        .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                        .show(ui, |ui| {
                            ui.horizontal(&mut tabs);
                        });
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(12.0);
                    let button = egui::Button::new(
                        RichText::new("Add widget").strong().size(12.0).color(BG),
                    )
                    .fill(TEXT)
                    .stroke(egui::Stroke::new(1.0, TEXT))
                    .corner_radius(egui::CornerRadius::same(6))
                    .min_size(egui::vec2(104.0, 25.0));
                    let (response, _) = egui::menu::MenuButton::from_button(button)
                        .config(
                            egui::menu::MenuConfig::new()
                                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
                        )
                        .ui(ui, |ui| {
                            ui.set_min_width(200.0);
                            ui.label(
                                RichText::new("ADD WIDGET")
                                    .monospace()
                                    .size(10.0)
                                    .color(MUTED),
                            );
                            ui.separator();
                            for kind in [
                                WidgetKind::Chart,
                                WidgetKind::Book,
                                WidgetKind::Compare,
                                WidgetKind::Tape,
                            ] {
                                if ui.button(kind.label()).clicked() {
                                    add = Some(kind);
                                    ui.close();
                                }
                            }
                            ui.separator();
                            ui.label(
                                RichText::new("Splits the focused pane")
                                    .size(10.0)
                                    .color(MUTED),
                            );
                        });
                    response.on_hover_text("Add a widget beside the focused pane");
                    self.account_menu(ui);
                });
            },
        );
        ui.painter().hline(
            header.response.rect.x_range(),
            header.response.rect.bottom() - 0.5,
            egui::Stroke::new(1.0, BORDER),
        );
        if hovered_tab.is_none() {
            self.tab_hover = None;
        }
        if let Some(index) = remove_workspace {
            self.remove_workspace(index);
        } else if let Some(index) = switch_to {
            self.switch_workspace(index);
        }
        if create_workspace {
            self.workspaces
                .push(Self::default_tree(self.workspaces.len()));
            self.switch_workspace(self.workspaces.len() - 1);
        }
        add
    }

    fn account_menu(&mut self, ui: &mut egui::Ui) {
        let button = egui::Button::new("")
            .fill(RAISED)
            .stroke(egui::Stroke::new(1.0, BORDER))
            .corner_radius(egui::CornerRadius::same(6))
            .min_size(egui::vec2(29.0, 25.0));
        let (response, _) = egui::menu::MenuButton::from_button(button)
            .config(
                egui::menu::MenuConfig::new()
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
            )
            .ui(ui, |ui| {
                ui.set_min_width(290.0);
                ui.label(
                    RichText::new("ACCOUNTS")
                        .monospace()
                        .size(10.0)
                        .color(MUTED),
                );
                ui.label(
                    RichText::new("Read-only live orders and positions")
                        .size(11.0)
                        .color(MUTED),
                );
                ui.separator();
                let mut remove = None;
                if self.accounts.is_empty() {
                    ui.label(RichText::new("No accounts connected").color(MUTED));
                }
                for (index, account) in self.accounts.iter().enumerate() {
                    let state = self.account_data.get(account);
                    ui.horizontal(|ui| {
                        let status = state.is_some_and(|state| state.connected);
                        let (dot, _) =
                            ui.allocate_exact_size(egui::vec2(10.0, 14.0), Sense::hover());
                        ui.painter().circle_filled(
                            dot.center(),
                            3.0,
                            if status { GREEN } else { MUTED },
                        );
                        ui.label(format!(
                            "{}  {}…{}",
                            account.exchange.label(),
                            &account.address[..6],
                            &account.address[account.address.len() - 4..]
                        ))
                        .on_hover_text(&account.address);
                        if ui
                            .small_button("×")
                            .on_hover_text("Remove account")
                            .clicked()
                        {
                            remove = Some(index);
                        }
                    });
                    if let Some(state) = state {
                        let description =
                            state
                                .error
                                .as_deref()
                                .map(str::to_owned)
                                .unwrap_or_else(|| {
                                    if state.connected {
                                        format!(
                                            "{} orders · {} positions · live",
                                            state.orders.len(),
                                            state.positions.len()
                                        )
                                    } else {
                                        "Syncing current state…".to_owned()
                                    }
                                });
                        ui.label(RichText::new(description).size(10.0).color(MUTED));
                        if state.connected
                            && (!state.orders.is_empty() || !state.positions.is_empty())
                        {
                            ui.collapsing("View live state", |ui| {
                                egui::ScrollArea::vertical()
                                    .max_height(180.0)
                                    .show(ui, |ui| {
                                        for position in &state.positions {
                                            let long = !position.size.starts_with('-');
                                            ui.colored_label(
                                                if long { GREEN } else { RED },
                                                format!(
                                                    "{} {} {} @ {} · PnL {}",
                                                    position.coin,
                                                    if long { "LONG" } else { "SHORT" },
                                                    position.size,
                                                    position.entry_price,
                                                    position.unrealized_pnl
                                                ),
                                            );
                                        }
                                        for order in &state.orders {
                                            ui.colored_label(
                                                if order.side == TradeSide::Buy {
                                                    GREEN
                                                } else {
                                                    RED
                                                },
                                                format!(
                                                    "{} {} {} @ {}",
                                                    order.coin,
                                                    if order.side == TradeSide::Buy {
                                                        "BUY"
                                                    } else {
                                                        "SELL"
                                                    },
                                                    order.size,
                                                    order.price
                                                ),
                                            );
                                        }
                                    });
                            });
                        }
                    }
                }
                if let Some(index) = remove {
                    self.accounts.remove(index);
                }
                ui.separator();
                ui.label(
                    RichText::new("CONNECT ACCOUNT")
                        .monospace()
                        .size(10.0)
                        .color(MUTED),
                );
                ui.label(
                    RichText::new("Use the trading or sub-account address")
                        .size(10.0)
                        .color(MUTED),
                );
                egui::ComboBox::from_id_salt("account-exchange")
                    .selected_text(self.account_exchange.label())
                    .show_ui(ui, |ui| {
                        for exchange in Exchange::ALL {
                            ui.add_enabled_ui(exchange == Exchange::Hyperliquid, |ui| {
                                ui.selectable_value(
                                    &mut self.account_exchange,
                                    exchange,
                                    exchange.label(),
                                );
                            });
                        }
                    });
                ui.add(
                    egui::TextEdit::singleline(&mut self.account_address)
                        .hint_text("0x wallet address")
                        .desired_width(270.0),
                );
                if let Some(error) = &self.account_error {
                    ui.label(RichText::new(error).size(11.0).color(RED));
                }
                if ui.button("Connect account").clicked() {
                    let account = Account {
                        exchange: self.account_exchange,
                        address: self.account_address.trim().to_ascii_lowercase(),
                    };
                    if !account.valid() {
                        self.account_error = Some("Enter a valid 0x wallet address".into());
                    } else if self.accounts.contains(&account) {
                        self.account_error = Some("This account is already connected".into());
                    } else {
                        self.accounts.push(account);
                        self.account_address.clear();
                        self.account_error = None;
                    }
                }
            });
        let painter = ui.painter();
        let center = response.rect.center();
        painter.circle_stroke(
            center + egui::vec2(0.0, -3.0),
            2.6,
            egui::Stroke::new(1.35, TEXT),
        );
        painter.add(egui::epaint::PathShape::line(
            vec![
                center + egui::vec2(-5.3, 5.0),
                center + egui::vec2(-4.0, 2.0),
                center + egui::vec2(-1.7, 0.9),
                center + egui::vec2(1.7, 0.9),
                center + egui::vec2(4.0, 2.0),
                center + egui::vec2(5.3, 5.0),
            ],
            egui::Stroke::new(1.35, TEXT),
        ));
        response.on_hover_text("Accounts");
    }

    fn switch_workspace(&mut self, index: usize) {
        if index < self.workspaces.len() && index != self.active_workspace {
            self.active_workspace = index;
            self.focused = None;
            self.sync_subscriptions();
        }
    }

    fn remove_workspace(&mut self, index: usize) {
        if self.workspaces.len() <= 1 || index >= self.workspaces.len() {
            return;
        }
        self.workspaces.remove(index);
        if index < self.active_workspace {
            self.active_workspace -= 1;
        } else if index == self.active_workspace {
            self.active_workspace = self.active_workspace.min(self.workspaces.len() - 1);
        }
        self.workspaces = std::mem::take(&mut self.workspaces)
            .into_iter()
            .enumerate()
            .map(|(index, tree)| Self::with_workspace_id(tree, index))
            .collect();
        self.focused = None;
        self.tab_hover = None;
        self.sync_subscriptions();
    }

    fn handle_workspace_shortcuts(&mut self, ctx: &egui::Context) {
        let keys = [
            egui::Key::Num1,
            egui::Key::Num2,
            egui::Key::Num3,
            egui::Key::Num4,
            egui::Key::Num5,
            egui::Key::Num6,
            egui::Key::Num7,
            egui::Key::Num8,
            egui::Key::Num9,
            egui::Key::Num0,
        ];
        for (index, key) in keys.into_iter().enumerate().take(self.workspaces.len()) {
            if ctx.input_mut(|input| {
                input.consume_key(egui::Modifiers::MAC_CMD | egui::Modifiers::ALT, key)
            }) {
                self.switch_workspace(index);
                break;
            }
        }
    }

    fn add_widget(&mut self, kind: WidgetKind) {
        let tree = &mut self.workspaces[self.active_workspace];
        let active = tree.active_tiles();
        let focused = self
            .focused
            .filter(|id| active.contains(id) && tree.tiles.get_pane(id).is_some())
            .or_else(|| {
                active
                    .into_iter()
                    .find(|id| tree.tiles.get_pane(id).is_some())
            });
        let market = focused
            .and_then(|id| tree.tiles.get_pane(&id))
            .map(|pane| pane.market.clone())
            .unwrap_or_default();
        let parent = focused.and_then(|id| tree.tiles.parent_of(id));
        let dir = focused
            .and_then(|id| tree.tiles.rect(id))
            .map(|rect| {
                if rect.width() >= rect.height() {
                    LinearDir::Horizontal
                } else {
                    LinearDir::Vertical
                }
            })
            .unwrap_or(LinearDir::Horizontal);
        let new_id = tree.tiles.insert_pane(Pane::new(kind, market));
        if let Some(focused) = focused {
            let split =
                tree.tiles
                    .insert_new(Tile::Container(Container::Linear(Linear::new_binary(
                        dir,
                        [focused, new_id],
                        0.5,
                    ))));
            if let Some(parent) = parent
                && let Some(Tile::Container(container)) = tree.tiles.get_mut(parent)
            {
                let _ = container.replace_child(focused, split);
            } else {
                tree.root = Some(split);
            }
        } else {
            tree.root = Some(new_id);
        }
        self.focused = Some(new_id);
        self.sync_subscriptions();
    }

    fn close_widget(&mut self, id: TileId) {
        let tree = &mut self.workspaces[self.active_workspace];
        if tree.root == Some(id) {
            tree.root = None;
        }
        tree.remove_recursively(id);
        tree.simplify(&egui_tiles::SimplificationOptions::default());
        if self.focused == Some(id) {
            self.focused = None;
        }
        self.sync_subscriptions();
    }
}

fn compare_editor(
    ui: &mut egui::Ui,
    pane: &mut Pane,
    catalogs: &HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
    errors: &HashMap<(Exchange, MarketKind), String>,
    needed: &mut HashSet<(Exchange, MarketKind)>,
    retry: &mut HashSet<(Exchange, MarketKind)>,
) {
    sources_list(ui, pane);
    ui.separator();
    ui.label(RichText::new("PRICE SOURCE").small().color(MUTED));
    ui.horizontal(|ui| {
        ui.selectable_value(&mut pane.compare_mode, CompareMode::Trades, "Last trades");
        ui.selectable_value(
            &mut pane.compare_mode,
            CompareMode::BestBidAsk,
            "Best bid / ask",
        );
    });
    if pane.compare_mode == CompareMode::Trades {
        ui.checkbox(&mut pane.compare_percent, "Compare % change");
    }
    ui.label(
        RichText::new("ROLLING WINDOW · SECONDS")
            .small()
            .color(MUTED),
    );
    ui.horizontal(|ui| {
        for (seconds, label) in [(60, "60s"), (300, "5m"), (600, "10m")] {
            ui.selectable_value(&mut pane.compare_window_secs, seconds, label);
        }
        ui.add_space(10.0);
        ui.add(
            egui::DragValue::new(&mut pane.compare_window_secs)
                .range(10..=600)
                .suffix("s"),
        );
    });
    ui.separator();
    selected_source_editor(ui, pane, catalogs, errors, needed, retry);
}

fn sources_list(ui: &mut egui::Ui, pane: &mut Pane) {
    ui.label(RichText::new("SOURCES").small().color(MUTED));
    let mut remove = None;
    egui::ScrollArea::vertical()
        .id_salt("comparison-sources")
        .max_height(118.0)
        .show(ui, |ui| {
            for (index, market) in pane.series.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(
                            pane.editing_series == index,
                            format!(
                                "{} {}  {}",
                                market.exchange.label(),
                                market.kind.label(),
                                market.symbol
                            ),
                        )
                        .clicked()
                    {
                        pane.editing_series = index;
                        pane.search.clear();
                    }
                    if pane.series.len() > 1 && ui.small_button("×").clicked() {
                        remove = Some(index);
                    }
                });
            }
        });
    if let Some(index) = remove {
        pane.series.remove(index);
        pane.editing_series = pane.editing_series.min(pane.series.len() - 1);
        pane.search.clear();
    }
    if ui.button("+ Add source").clicked() {
        let exchange = Exchange::ALL
            .into_iter()
            .find(|exchange| {
                !pane.series.iter().any(|m| {
                    m.exchange == *exchange
                        && m.kind
                            == pane
                                .series
                                .get(pane.editing_series)
                                .map_or(MarketKind::Spot, |selected| selected.kind)
                })
            })
            .unwrap_or(Exchange::Binance);
        pane.series.push(Market::for_exchange_kind(
            exchange,
            pane.series
                .get(pane.editing_series)
                .map_or(MarketKind::Spot, |market| market.kind),
        ));
        pane.editing_series = pane.series.len() - 1;
        pane.search.clear();
    }
}

fn selected_source_editor(
    ui: &mut egui::Ui,
    pane: &mut Pane,
    catalogs: &HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
    errors: &HashMap<(Exchange, MarketKind), String>,
    needed: &mut HashSet<(Exchange, MarketKind)>,
    retry: &mut HashSet<(Exchange, MarketKind)>,
) {
    if let Some(market) = pane.series.get_mut(pane.editing_series) {
        market_editor(
            ui,
            market,
            &mut pane.search,
            catalogs,
            errors,
            needed,
            retry,
        );
    }
}

fn market_editor(
    ui: &mut egui::Ui,
    market: &mut Market,
    search: &mut String,
    catalogs: &HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
    errors: &HashMap<(Exchange, MarketKind), String>,
    needed: &mut HashSet<(Exchange, MarketKind)>,
    retry: &mut HashSet<(Exchange, MarketKind)>,
) {
    ui.label(RichText::new("EXCHANGE").small().color(MUTED));
    for exchange in Exchange::ALL {
        if ui
            .selectable_label(market.exchange == exchange, exchange.label())
            .clicked()
            && market.exchange != exchange
        {
            *market = Market::for_exchange_kind(exchange, market.kind);
            search.clear();
        }
    }
    ui.separator();
    ui.label(RichText::new("MARKET").small().color(MUTED));
    ui.horizontal(|ui| {
        for kind in MarketKind::ALL {
            if ui
                .selectable_label(market.kind == kind, kind.label())
                .clicked()
                && market.kind != kind
            {
                *market = Market::for_exchange_kind(market.exchange, kind);
                search.clear();
            }
        }
    });
    ui.separator();
    ui.label(
        RichText::new(format!("SYMBOL · {}", market.market_type()))
            .small()
            .color(MUTED),
    );
    let key = (market.exchange, market.kind);
    needed.insert(key);
    if let Some(symbols) = catalogs.get(&key) {
        ui.add(
            egui::TextEdit::singleline(search)
                .hint_text("Search all symbols")
                .desired_width(260.0),
        );
        let query = search.trim().to_ascii_uppercase();
        let matches: Vec<_> = symbols
            .iter()
            .filter(|symbol| {
                query.is_empty()
                    || symbol.symbol.to_ascii_uppercase().contains(&query)
                    || symbol.base.to_ascii_uppercase().contains(&query)
                    || symbol.quote.to_ascii_uppercase().contains(&query)
            })
            .collect();
        ui.label(
            RichText::new(format!(
                "{} matches · {} available",
                matches.len(),
                symbols.len()
            ))
            .small()
            .color(MUTED),
        );
        egui::ScrollArea::vertical()
            .id_salt(("symbols", key))
            .max_height(190.0)
            .show_rows(ui, 22.0, matches.len(), |ui, rows| {
                for index in rows {
                    let symbol = matches[index];
                    let label = format!("{} / {}   {}", symbol.base, symbol.quote, symbol.symbol);
                    if ui
                        .selectable_label(market.symbol == symbol.symbol, label)
                        .clicked()
                    {
                        market.symbol.clone_from(&symbol.symbol);
                    }
                }
            });
    } else if let Some(error) = errors.get(&key) {
        ui.label(RichText::new(format!("Could not load symbols: {error}")).color(RED));
        if ui.button("Retry").clicked() {
            retry.insert(key);
        }
    } else {
        ui.label(RichText::new("Loading symbols…").color(MUTED));
    }
}

struct PaneBehavior<'a> {
    data: &'a HashMap<Market, MarketData>,
    account_data: &'a HashMap<Account, AccountState>,
    catalogs: &'a HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
    catalog_errors: &'a HashMap<(Exchange, MarketKind), String>,
    needed_catalogs: &'a mut HashSet<(Exchange, MarketKind)>,
    retry_catalogs: &'a mut HashSet<(Exchange, MarketKind)>,
    focused: &'a mut Option<TileId>,
    close: &'a mut Option<TileId>,
}

impl Behavior<Pane> for PaneBehavior<'_> {
    fn pane_ui(&mut self, ui: &mut egui::Ui, tile_id: TileId, pane: &mut Pane) -> UiResponse {
        if ui.rect_contains_pointer(ui.max_rect()) && ui.input(|input| input.pointer.any_pressed())
        {
            *self.focused = Some(tile_id);
        }
        let old_market = pane.market.clone();
        let live = if matches!(pane.kind, WidgetKind::Compare | WidgetKind::Tape) {
            !pane.series.is_empty()
                && pane
                    .series
                    .iter()
                    .all(|market| self.data.get(market).is_some_and(|data| data.connected))
        } else {
            self.data
                .get(&pane.market)
                .is_some_and(|data| data.connected)
        };
        let dragging = ui
            .horizontal(|ui| {
                ui.add_space(10.0);
                let title = ui.add(
                    egui::Label::new(
                        RichText::new(pane.kind.short_label())
                            .strong()
                            .size(12.0)
                            .color(TEXT),
                    )
                    .sense(Sense::drag()),
                );
                let subtitle = if matches!(pane.kind, WidgetKind::Compare | WidgetKind::Tape) {
                    format!("{} sources", pane.series.len())
                } else {
                    format!(
                        "{} {}  {}",
                        pane.market.exchange.label(),
                        pane.market.kind.label(),
                        pane.market.symbol
                    )
                };
                let label_width = (ui.available_width() - 72.0).max(42.0);
                ui.add_sized(
                    [label_width, 18.0],
                    egui::Label::new(RichText::new(subtitle).size(11.0).color(MUTED)).truncate(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(6.0);
                    if ui.small_button("×").on_hover_text("Close widget").clicked() {
                        *self.close = Some(tile_id);
                    }
                    let (config_button, _) = egui::menu::MenuButton::new("⚙")
                        .config(
                            egui::menu::MenuConfig::new()
                                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
                        )
                        .ui(ui, |ui| {
                            ui.set_min_width(280.0);
                            ui.label(RichText::new("WIDGET CONFIG").small().color(MUTED));
                            ui.separator();
                            match pane.kind {
                                WidgetKind::Compare => compare_editor(
                                    ui,
                                    pane,
                                    self.catalogs,
                                    self.catalog_errors,
                                    self.needed_catalogs,
                                    self.retry_catalogs,
                                ),
                                WidgetKind::Tape => {
                                    sources_list(ui, pane);
                                    ui.separator();
                                    selected_source_editor(
                                        ui,
                                        pane,
                                        self.catalogs,
                                        self.catalog_errors,
                                        self.needed_catalogs,
                                        self.retry_catalogs,
                                    );
                                }
                                WidgetKind::Chart | WidgetKind::Book => market_editor(
                                    ui,
                                    &mut pane.market,
                                    &mut pane.search,
                                    self.catalogs,
                                    self.catalog_errors,
                                    self.needed_catalogs,
                                    self.retry_catalogs,
                                ),
                            }
                        });
                    config_button.on_hover_text("Configure widget");
                    let (rect, response) =
                        ui.allocate_exact_size(egui::vec2(10.0, 10.0), Sense::hover());
                    ui.painter()
                        .circle_filled(rect.center(), 3.0, if live { GREEN } else { RED });
                    response.on_hover_text(if live { "Live" } else { "Connecting" });
                });
                title.drag_started()
            })
            .inner;
        if old_market != pane.market {
            pane.chart = view::ChartView::default();
            pane.book_view = view::BookView::default();
        }
        ui.separator();
        let data = self.data.get(&pane.market);
        let market = &pane.market;
        let own_orders: Vec<_> = self
            .account_data
            .iter()
            .filter(|(account, state)| account.exchange == market.exchange && state.connected)
            .flat_map(|(account, state)| {
                state.orders.iter().filter_map(move |order| {
                    (order.coin == market.symbol).then_some((account, order))
                })
            })
            .collect();
        let positions: Vec<_> = self
            .account_data
            .iter()
            .filter(|(account, state)| account.exchange == market.exchange && state.connected)
            .flat_map(|(account, state)| {
                state.positions.iter().filter_map(move |position| {
                    (market.kind == MarketKind::Perp && position.coin == market.symbol)
                        .then_some((account, position))
                })
            })
            .collect();
        match pane.kind {
            WidgetKind::Chart => {
                let candles = data.map_or(&[][..], |data| data.candles.as_slice());
                view::chart_ui(ui, candles, &mut pane.chart, &own_orders, &positions);
            }
            WidgetKind::Book => view::book_ui(
                ui,
                data.and_then(|data| data.book.as_ref()),
                &mut pane.book_view,
                &own_orders,
            ),
            WidgetKind::Compare => view::compare_ui(
                ui,
                &pane.series,
                self.data,
                pane.compare_mode,
                pane.compare_percent,
                pane.compare_window_secs,
            ),
            WidgetKind::Tape => view::tape_ui(ui, &pane.series, self.data),
        }
        if dragging {
            UiResponse::DragStarted
        } else {
            UiResponse::None
        }
    }

    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
        if matches!(pane.kind, WidgetKind::Compare | WidgetKind::Tape) {
            format!(
                "{} · {} sources",
                pane.kind.short_label(),
                pane.series.len()
            )
            .into()
        } else {
            format!(
                "{} · {} {} {}",
                pane.kind.short_label(),
                pane.market.exchange.label(),
                pane.market.kind.label(),
                pane.market.symbol
            )
            .into()
        }
    }

    fn min_size(&self) -> f32 {
        220.0
    }
    fn gap_width(&self, _style: &egui::Style) -> f32 {
        4.0
    }
}

impl eframe::App for TerminalApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if ui.visuals().window_fill != terminal_visuals().window_fill {
            ui.ctx().set_visuals(terminal_visuals());
        }
        self.poll_socket(ui.ctx());
        let time = ui.ctx().input(|input| input.time);
        if self.receiver.is_none() && time >= self.reconnect_at {
            self.reconnect_at = time + 3.0;
            self.connect(ui.ctx());
        }
        if self.receiver.is_none() {
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
        self.handle_workspace_shortcuts(ui.ctx());

        egui::Frame::new().fill(BG).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            let add = self.header(ui);
            let mut close = None;
            let mut needed_catalogs = HashSet::new();
            let mut retry_catalogs = HashSet::new();
            {
                let mut behavior = PaneBehavior {
                    data: &self.data,
                    account_data: &self.account_data,
                    catalogs: &self.catalogs,
                    catalog_errors: &self.catalog_errors,
                    needed_catalogs: &mut needed_catalogs,
                    retry_catalogs: &mut retry_catalogs,
                    focused: &mut self.focused,
                    close: &mut close,
                };
                self.workspaces[self.active_workspace].ui(&mut behavior, ui);
            }
            if let Some(id) = close {
                self.close_widget(id);
            }
            if let Some(kind) = add {
                self.add_widget(kind);
            }
            self.sync_subscriptions();
            self.request_catalogs(needed_catalogs, retry_catalogs);
        });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Ok(workspaces) = serde_json::to_string(&self.workspaces) {
            storage.set_string("terminal_workspaces", workspaces);
            storage.set_string("active_terminal", self.active_workspace.to_string());
        }
        if let Ok(accounts) = serde_json::to_string(&self.accounts) {
            storage.set_string("accounts", accounts);
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn ws_url() -> String {
    let location = web_sys::window().expect("window").location();
    let scheme = if location.protocol().ok().as_deref() == Some("https:") {
        "wss"
    } else {
        "ws"
    };
    format!("{scheme}://{}/ws", location.host().expect("host"))
}

#[cfg(not(target_arch = "wasm32"))]
fn ws_url() -> String {
    "ws://127.0.0.1:3000/ws".to_owned()
}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result {
    eframe::run_native(
        "Rust Terminal",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([1440.0, 900.0]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(TerminalApp::new(cc)))),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use eframe::wasm_bindgen::JsCast as _;
    wasm_bindgen_futures::spawn_local(async {
        let document = web_sys::window().unwrap().document().unwrap();
        let canvas = document
            .get_element_by_id("terminal")
            .unwrap()
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .unwrap();
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| Ok(Box::new(TerminalApp::new(cc)))),
            )
            .await;
        if let Some(loading) = document.get_element_by_id("loading") {
            if result.is_ok() {
                loading.remove();
            } else {
                loading.set_inner_html("Failed to load terminal");
            }
        }
        result.expect("failed to start web terminal");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_ids(tree: &Tree<Pane>) -> Vec<TileId> {
        tree.active_tiles()
            .into_iter()
            .filter(|id| tree.tiles.get_pane(id).is_some())
            .collect()
    }

    fn test_app() -> TerminalApp {
        TerminalApp {
            workspaces: (0..3).map(TerminalApp::default_tree).collect(),
            active_workspace: 0,
            tab_hover: None,
            data: HashMap::new(),
            catalogs: HashMap::new(),
            catalog_errors: HashMap::new(),
            catalog_requests: HashSet::new(),
            subscribed: HashSet::new(),
            accounts: Vec::new(),
            account_data: HashMap::new(),
            subscribed_accounts: HashSet::new(),
            account_exchange: Exchange::Hyperliquid,
            account_address: String::new(),
            account_error: None,
            focused: None,
            socket_open: false,
            sender: None,
            receiver: None,
            reconnect_at: 0.0,
        }
    }

    #[test]
    fn new_widget_splits_focused_tile_and_closing_it_keeps_neighbors() {
        let mut app = test_app();
        let original = pane_ids(&app.workspaces[0]);
        let chart = original
            .iter()
            .copied()
            .find(|id| {
                matches!(
                    app.workspaces[0].tiles.get_pane(id).map(|p| p.kind),
                    Some(WidgetKind::Chart)
                )
            })
            .unwrap();
        let market = Market {
            exchange: Exchange::Okx,
            kind: MarketKind::Spot,
            symbol: "ETH-USDT".to_owned(),
        };
        if let Some(Tile::Pane(pane)) = app.workspaces[0].tiles.get_mut(chart) {
            pane.market = market.clone();
        }
        app.focused = Some(chart);
        app.add_widget(WidgetKind::Book);
        let added = app.focused.unwrap();
        assert_eq!(pane_ids(&app.workspaces[0]).len(), 3);
        assert_eq!(
            app.workspaces[0].tiles.get_pane(&added).unwrap().market,
            market
        );
        assert!(
            original
                .iter()
                .all(|id| pane_ids(&app.workspaces[0]).contains(id))
        );

        app.close_widget(added);
        assert_eq!(pane_ids(&app.workspaces[0]).len(), 2);
        assert!(
            original
                .iter()
                .all(|id| pane_ids(&app.workspaces[0]).contains(id))
        );
    }

    #[test]
    fn terminals_keep_separate_widgets_and_settings_when_switched_and_saved() {
        let mut app = test_app();
        let first_chart = pane_ids(&app.workspaces[0])[0];
        let market = Market {
            exchange: Exchange::Gate,
            kind: MarketKind::Spot,
            symbol: "PONS_USDT".to_owned(),
        };
        if let Some(Tile::Pane(pane)) = app.workspaces[0].tiles.get_mut(first_chart) {
            pane.market = market.clone();
        }
        app.focused = Some(first_chart);
        app.add_widget(WidgetKind::Tape);

        app.switch_workspace(1);
        assert!(app.focused.is_none());
        assert_eq!(pane_ids(&app.workspaces[1]).len(), 2);
        let second_chart = pane_ids(&app.workspaces[1])[0];
        assert_ne!(
            app.workspaces[1]
                .tiles
                .get_pane(&second_chart)
                .unwrap()
                .market,
            market
        );

        app.switch_workspace(0);
        assert_eq!(pane_ids(&app.workspaces[0]).len(), 3);
        assert_eq!(
            app.workspaces[0]
                .tiles
                .get_pane(&first_chart)
                .unwrap()
                .market,
            market
        );

        let saved = serde_json::to_string(&app.workspaces).unwrap();
        let restored: Vec<Tree<Pane>> = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored.len(), 3);
        assert_eq!(pane_ids(&restored[0]).len(), 3);
        assert_eq!(pane_ids(&restored[1]).len(), 2);
        assert_eq!(
            restored[0].tiles.get_pane(&first_chart).unwrap().market,
            market
        );
    }

    #[test]
    fn deleting_terminals_keeps_remaining_layouts_and_a_valid_active_tab() {
        let mut app = test_app();
        let third_chart = pane_ids(&app.workspaces[2])[0];
        let market = Market {
            exchange: Exchange::Okx,
            kind: MarketKind::Spot,
            symbol: "SOL-USDT".to_owned(),
        };
        if let Some(Tile::Pane(pane)) = app.workspaces[2].tiles.get_mut(third_chart) {
            pane.market = market.clone();
        }
        app.switch_workspace(2);

        app.remove_workspace(1);
        assert_eq!(app.workspaces.len(), 2);
        assert_eq!(app.active_workspace, 1);
        assert_eq!(
            app.workspaces[1]
                .tiles
                .get_pane(&third_chart)
                .unwrap()
                .market,
            market
        );
        assert_ne!(app.workspaces[0].id(), app.workspaces[1].id());

        app.remove_workspace(1);
        assert_eq!(app.workspaces.len(), 1);
        assert_eq!(app.active_workspace, 0);
        app.remove_workspace(0);
        assert_eq!(app.workspaces.len(), 1);
    }

    #[test]
    fn old_layout_panes_still_load() {
        let old = Tree::new_horizontal("workspace", vec![WidgetKind::Chart, WidgetKind::Book]);
        let saved = serde_json::to_string(&old).unwrap();
        let restored: Tree<Pane> = serde_json::from_str(&saved).unwrap();
        assert_eq!(pane_ids(&restored).len(), 2);
        assert!(
            pane_ids(&restored)
                .iter()
                .all(|id| restored.tiles.get_pane(id).unwrap().legacy)
        );
    }

    #[test]
    fn saved_comparison_defaults_to_live_trades() {
        let saved = r#"{"kind":"Compare","market":{"exchange":"binance","symbol":"BTCUSDT"},"series":[{"exchange":"binance","symbol":"BTCUSDT"}],"compare_percent":false}"#;
        let pane: Pane = serde_json::from_str(saved).unwrap();
        assert_eq!(pane.compare_mode, CompareMode::Trades);
        assert_eq!(pane.compare_window_secs, 60);
    }

    #[test]
    fn snapshot_prices_update_the_label_without_creating_trade_points() {
        let mut data = MarketData::default();
        for (time_ms, price) in [(1_000, "100"), (1_020, "101"), (1_060, "102")] {
            data.set_price(LastPrice {
                price: price.to_owned(),
                time_ms,
            });
        }
        assert_eq!(data.last_price.as_ref().unwrap().price, "102");
        assert!(data.price_trades.is_empty());
        data.set_price(LastPrice {
            price: "99".to_owned(),
            time_ms: 1_050,
        });
        assert_eq!(data.last_price.as_ref().unwrap().price, "102");
    }

    #[test]
    fn tape_and_price_chart_both_keep_individual_trades() {
        let mut data = MarketData::default();
        for index in 0..=MAX_TAPE_TRADES {
            data.push_trade(Trade {
                price: (100 + index).to_string(),
                size: "0.5".to_owned(),
                time_ms: 1_000 + index as i64,
                side: TradeSide::Buy,
            });
        }
        assert_eq!(data.trades.len(), MAX_TAPE_TRADES);
        assert_eq!(data.trades.front().unwrap().price, "101");
        assert_eq!(data.trades.back().unwrap().price, "600");
        assert_eq!(data.price_trades.len(), MAX_TAPE_TRADES + 1);
        assert_eq!(data.price_trades.front().unwrap().price, 100.0);
        assert_eq!(data.last_price.as_ref().unwrap().price, "600");
    }

    #[test]
    fn prices_history_keeps_each_trade_from_tape() {
        let mut data = MarketData::default();
        for (time_ms, price, side) in [
            (1_000, "100", TradeSide::Buy),
            (1_000, "101", TradeSide::Sell),
            (1_020, "102", TradeSide::Buy),
        ] {
            data.push_trade(Trade {
                price: price.to_owned(),
                size: "0.5".to_owned(),
                time_ms,
                side,
            });
        }
        assert_eq!(
            data.price_trades
                .iter()
                .map(|trade| trade.price)
                .collect::<Vec<_>>(),
            [100.0, 101.0, 102.0]
        );
        assert_eq!(
            data.price_trades
                .iter()
                .map(|trade| trade.time_ms)
                .collect::<Vec<_>>(),
            [1_000, 1_000, 1_020]
        );
        assert_eq!(
            data.price_trades
                .iter()
                .map(|trade| trade.side)
                .collect::<Vec<_>>(),
            [TradeSide::Buy, TradeSide::Sell, TradeSide::Buy]
        );
    }

    #[test]
    fn trade_history_rolls_after_ten_minutes() {
        let mut data = MarketData::default();
        for time_ms in [1_000, 601_001] {
            data.push_trade(Trade {
                price: "100".to_owned(),
                size: "1".to_owned(),
                time_ms,
                side: TradeSide::Buy,
            });
        }
        assert_eq!(data.price_trades.len(), 1);
        assert_eq!(data.price_trades.front().unwrap().time_ms, 601_001);
    }

    #[test]
    fn best_bid_and_ask_samples_follow_live_book_updates() {
        let mut data = MarketData::default();
        data.set_book(
            Book {
                bids: vec![terminal_core::Level {
                    price: "100.1".to_owned(),
                    size: "2".to_owned(),
                    quote_size: "200.2".to_owned(),
                    depth_base: "2".to_owned(),
                    depth_quote: "200.2".to_owned(),
                }],
                asks: vec![terminal_core::Level {
                    price: "100.2".to_owned(),
                    size: "3".to_owned(),
                    quote_size: "300.6".to_owned(),
                    depth_base: "3".to_owned(),
                    depth_quote: "300.6".to_owned(),
                }],
                updated_at_ms: 1_000,
            },
            Exchange::Binance,
        );
        let quote = data.quotes.back().unwrap();
        assert_eq!((quote.bid, quote.ask), (100.1, 100.2));
        assert_eq!(quote.time_ms, 1_000);
    }

    #[test]
    fn book_deltas_update_deep_levels_and_cumulative_sizes() {
        let mut data = MarketData::default();
        data.set_book(
            Book {
                bids: vec![Level {
                    price: "100".into(),
                    size: "2".into(),
                    quote_size: "200".into(),
                    depth_base: "2".into(),
                    depth_quote: "200".into(),
                }],
                asks: vec![Level {
                    price: "101".into(),
                    size: "1".into(),
                    quote_size: "101".into(),
                    depth_base: "1".into(),
                    depth_quote: "101".into(),
                }],
                updated_at_ms: 1,
            },
            Exchange::Binance,
        );
        data.apply_book_delta(
            vec![
                BookChange {
                    price: "100".into(),
                    size: "0".into(),
                },
                BookChange {
                    price: "99".into(),
                    size: "3".into(),
                },
            ],
            vec![BookChange {
                price: "102".into(),
                size: "2".into(),
            }],
            2,
            Exchange::Binance,
        );
        let book = data.book.as_ref().unwrap();
        assert_eq!(book.bids[0].price, "99");
        assert_eq!(book.bids[0].depth_quote, "297");
        assert_eq!(book.asks[1].depth_base, "3");
        assert_eq!(book.asks[1].depth_quote, "305");
    }

    #[test]
    fn newer_bbo_stays_live_when_hyperliquid_depth_snapshot_is_older() {
        let mut data = MarketData::default();
        data.set_best_bid_ask(BestBidAsk {
            bid: "101".to_owned(),
            ask: "102".to_owned(),
            time_ms: 1_100,
        });
        data.set_book(
            Book {
                bids: vec![terminal_core::Level {
                    price: "99".to_owned(),
                    size: "1".to_owned(),
                    quote_size: String::new(),
                    depth_base: String::new(),
                    depth_quote: String::new(),
                }],
                asks: vec![terminal_core::Level {
                    price: "100".to_owned(),
                    size: "1".to_owned(),
                    quote_size: String::new(),
                    depth_base: String::new(),
                    depth_quote: String::new(),
                }],
                updated_at_ms: 1_050,
            },
            Exchange::Hyperliquid,
        );
        let quote = data.best_bid_ask.as_ref().unwrap();
        assert_eq!((quote.bid.as_str(), quote.ask.as_str()), ("101", "102"));
        let latest = data.quotes.back().unwrap();
        assert_eq!((latest.bid, latest.ask), (101.0, 102.0));
    }

    #[test]
    fn gate_book_cannot_replace_dedicated_bbo_with_crossed_prices() {
        let mut data = MarketData::default();
        data.set_best_bid_ask(BestBidAsk {
            bid: "0.6204".into(),
            ask: "0.6207".into(),
            time_ms: 1_000,
        });
        data.set_book(
            Book {
                bids: vec![Level {
                    price: "0.6205".into(),
                    size: "1".into(),
                    quote_size: String::new(),
                    depth_base: String::new(),
                    depth_quote: String::new(),
                }],
                asks: vec![Level {
                    price: "0.6203".into(),
                    size: "1".into(),
                    quote_size: String::new(),
                    depth_base: String::new(),
                    depth_quote: String::new(),
                }],
                updated_at_ms: 1_100,
            },
            Exchange::Gate,
        );
        data.set_best_bid_ask(BestBidAsk {
            bid: "0.6205".into(),
            ask: "0.6203".into(),
            time_ms: 1_200,
        });
        let quote = data.best_bid_ask.as_ref().unwrap();
        assert_eq!(
            (quote.bid.as_str(), quote.ask.as_str()),
            ("0.6204", "0.6207")
        );
        assert_eq!(data.quotes.len(), 1);
    }
}
