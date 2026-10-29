mod accounts;
mod audio;
mod batch;
mod candles;
mod dom;
mod fills;
mod orderflow;
mod position;
mod private;
mod socket;
mod vault;
mod view;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    str::FromStr,
    time::Duration,
};

use eframe::egui::{self, Color32, RichText, Sense};
use egui_tiles::{Behavior, Container, Linear, LinearDir, Tile, TileId, Tiles, Tree, UiResponse};
use ewebsock::{WsEvent, WsMessage, WsSender};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};
use terminal_core::{
    Account, AccountState, BestBidAsk, Book, BookChange, Candle, ClientMessage, Exchange,
    LastPrice, Level, Market, MarketKind, OwnOrder, ServerMessage, SymbolInfo, Trade, TradeSide,
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
const SOCKET_EVENTS_PER_PASS: usize = 256;
const SOCKET_BUDGET_MS: u64 = 4;
const MAX_TRADE_POINTS: usize = 200_000;
const MAX_QUOTE_SAMPLES: usize = 12_100;
const MAX_TAPE_TRADES: usize = 500;
const TAB_DELETE_HOVER_SECS: f64 = 0.45;

const fn default_compare_window_secs() -> u32 {
    60
}

const fn default_compare_show_orders() -> bool {
    true
}

fn utc_now_ms() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|window| window.performance())
            .map_or(0, |clock| (clock.time_origin() + clock.now()) as i64)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis() as i64)
    }
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

fn account_visibility_button(ui: &mut egui::Ui, visible: bool) -> bool {
    let response = ui.add_sized(
        [23.0, 21.0],
        egui::Button::new("")
            .fill(RAISED)
            .stroke(egui::Stroke::new(1.0, BORDER)),
    );
    let center = response.rect.center();
    let color = if visible { TEXT } else { MUTED };
    ui.painter().add(egui::epaint::PathShape::line(
        [
            (-6.0, 0.0),
            (-3.0, -2.8),
            (0.0, -3.8),
            (3.0, -2.8),
            (6.0, 0.0),
            (3.0, 2.8),
            (0.0, 3.8),
            (-3.0, 2.8),
            (-6.0, 0.0),
        ]
        .into_iter()
        .map(|(x, y)| center + egui::vec2(x, y))
        .collect(),
        egui::Stroke::new(1.2, color),
    ));
    ui.painter()
        .circle_stroke(center, 1.7, egui::Stroke::new(1.2, color));
    if !visible {
        let from = center + egui::vec2(-6.0, 5.0);
        let to = center + egui::vec2(6.0, -5.0);
        ui.painter()
            .line_segment([from, to], egui::Stroke::new(3.0, RAISED));
        ui.painter()
            .line_segment([from, to], egui::Stroke::new(1.2, color));
    }
    response
        .on_hover_text(if visible {
            "Hide account on widgets"
        } else {
            "Show account on widgets"
        })
        .clicked()
}

#[derive(Clone, Copy, Serialize, Deserialize)]
enum WidgetKind {
    Chart,
    Book,
    Compare,
    Tape,
    Dom,
    Fills,
    Position,
}

impl WidgetKind {
    fn label(self) -> &'static str {
        match self {
            Self::Chart => "OHLCV",
            Self::Book => "Order Book",
            Self::Compare => "Live Price",
            Self::Tape => "Trade Tape",
            Self::Dom => "DOM",
            Self::Fills => "Fills",
            Self::Position => "Position",
        }
    }

    fn short_label(self) -> &'static str {
        match self {
            Self::Chart => "CHART",
            Self::Book => "BOOK",
            Self::Compare => "PRICES",
            Self::Tape => "TRADES",
            Self::Dom => "DOM",
            Self::Fills => "FILLS",
            Self::Position => "POSITION",
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
    chart_timeframe: candles::Timeframe,
    series: Vec<Market>,
    compare_percent: bool,
    compare_mode: CompareMode,
    compare_window_secs: u32,
    compare_show_orders: bool,
    compare_show_fills: bool,
    orderflow: orderflow::Settings,
    dom: dom::Settings,
    fills: fills::Settings,
    position: position::Settings,
    unavailable_markets: Vec<Market>,
    batch_symbol: Option<String>,
    #[serde(skip)]
    position_view: position::View,
    #[serde(skip)]
    fills_view: fills::View,
    #[serde(skip)]
    dom_view: dom::View,
    #[serde(skip)]
    chart: view::ChartView,
    #[serde(skip)]
    compare_view: view::YAxisView,
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
        } else if matches!(kind, WidgetKind::Dom) {
            vec![market.clone()]
        } else {
            Vec::new()
        };
        Self {
            kind,
            market,
            chart_timeframe: candles::Timeframe::default(),
            series,
            compare_percent: false,
            compare_mode: CompareMode::default(),
            compare_window_secs: default_compare_window_secs(),
            compare_show_orders: default_compare_show_orders(),
            compare_show_fills: true,
            orderflow: orderflow::Settings::default(),
            dom: dom::Settings::default(),
            fills: fills::Settings::default(),
            position: position::Settings::default(),
            unavailable_markets: Vec::new(),
            batch_symbol: None,
            position_view: position::View::default(),
            fills_view: fills::View::default(),
            dom_view: dom::View::default(),
            chart: view::ChartView::default(),
            compare_view: view::YAxisView::default(),
            book_view: view::BookView::default(),
            legacy: false,
            search: String::new(),
            editing_series: 0,
        }
    }
}

impl Pane {
    fn uses_sources(&self) -> bool {
        matches!(
            self.kind,
            WidgetKind::Compare | WidgetKind::Tape | WidgetKind::Dom
        )
    }

    fn swap_sources(&mut self, from: usize, to: usize) {
        self.series.swap(from, to);
        if self.editing_series == from {
            self.editing_series = to;
        } else if self.editing_series == to {
            self.editing_series = from;
        }
        if matches!(self.kind, WidgetKind::Dom) {
            self.market = self.series[0].clone();
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
                chart_timeframe: candles::Timeframe,
                #[serde(default)]
                series: Vec<Market>,
                #[serde(default)]
                compare_percent: bool,
                #[serde(default)]
                compare_mode: CompareMode,
                #[serde(default = "default_compare_window_secs")]
                compare_window_secs: u32,
                #[serde(default = "default_compare_show_orders")]
                compare_show_orders: bool,
                #[serde(default = "default_compare_show_orders")]
                compare_show_fills: bool,
                #[serde(default)]
                orderflow: orderflow::Settings,
                #[serde(default)]
                dom: dom::Settings,
                #[serde(default)]
                fills: fills::Settings,
                #[serde(default)]
                position: position::Settings,
                #[serde(default)]
                unavailable_markets: Vec<Market>,
                #[serde(default)]
                batch_symbol: Option<String>,
            },
            Legacy(WidgetKind),
        }
        Ok(match SavedPane::deserialize(deserializer)? {
            SavedPane::Current {
                kind,
                market,
                chart_timeframe,
                series,
                compare_percent,
                compare_mode,
                compare_window_secs,
                compare_show_orders,
                compare_show_fills,
                orderflow,
                dom,
                fills,
                position,
                unavailable_markets,
                batch_symbol,
            } => {
                let mut pane = Pane::new(kind, market);
                pane.chart_timeframe = chart_timeframe;
                if matches!(
                    kind,
                    WidgetKind::Compare | WidgetKind::Tape | WidgetKind::Dom
                ) && !series.is_empty()
                {
                    pane.series = series;
                    if matches!(kind, WidgetKind::Dom) {
                        pane.market = pane.series[0].clone();
                    }
                }
                pane.compare_percent = compare_percent;
                pane.compare_mode = compare_mode;
                pane.compare_window_secs = compare_window_secs.clamp(10, 600);
                pane.compare_show_orders = compare_show_orders;
                pane.compare_show_fills = compare_show_fills;
                pane.orderflow = orderflow;
                pane.dom = dom;
                pane.fills = fills;
                pane.position = position;
                pane.unavailable_markets = unavailable_markets;
                pane.batch_symbol = batch_symbol;
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
    trade_candles: candles::History,
    book: Option<Book>,
    best_bid_ask: Option<BestBidAsk>,
    last_price: Option<LastPrice>,
    price_trades: VecDeque<TradePoint>,
    latest_trade: Option<TradePoint>,
    orderflow: orderflow::History,
    dom: dom::History,
    trades: VecDeque<Trade>,
    quotes: VecDeque<QuoteTick>,
    fill_quotes: fills::QuoteHistory,
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
    exact_price: Option<Decimal>,
    size: f64,
    side: TradeSide,
}

impl MarketData {
    fn push_trade(&mut self, trade: Trade) {
        self.dom.push(&trade);
        self.set_price(LastPrice {
            price: trade.price.clone(),
            time_ms: trade.time_ms,
        });
        if let Ok(price) = trade.price.parse::<f64>()
            && price.is_finite()
            && price > 0.0
        {
            let point = TradePoint {
                time_ms: trade.time_ms,
                price,
                exact_price: trade.price.parse().ok(),
                size: trade.size.parse().unwrap_or(0.0),
                side: trade.side,
            };
            if self
                .latest_trade
                .is_none_or(|latest| point.time_ms >= latest.time_ms)
            {
                self.latest_trade = Some(point);
            }
            self.orderflow.push(point);
            self.trade_candles.push(point);
            self.price_trades.push_back(point);
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

    fn set_book(&mut self, mut book: Book, exchange: Exchange) {
        if self
            .book
            .as_ref()
            .is_some_and(|previous| book.updated_at_ms < previous.updated_at_ms)
        {
            return;
        }
        // REST/WS snapshots carry only price and base size. Derive quote sizes
        // and cumulative depth locally, once, preserving Decimal precision.
        for levels in [&mut book.bids, &mut book.asks] {
            if levels.iter().any(|level| level.depth_quote.is_empty()) {
                let (mut base, mut quote) = (Decimal::ZERO, Decimal::ZERO);
                for level in levels {
                    if let (Ok(price), Ok(size)) = (
                        Decimal::from_str(&level.price),
                        Decimal::from_str(&level.size),
                    ) {
                        let notional = price * size;
                        base += size;
                        quote += notional;
                        level.quote_size = notional.normalize().to_string();
                        level.depth_base = base.normalize().to_string();
                        level.depth_quote = quote.normalize().to_string();
                    }
                }
            }
        }
        if !matches!(
            exchange,
            Exchange::Gate | Exchange::Hyperliquid | Exchange::Lighter | Exchange::Pacifica
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
        if updated_at_ms < book.updated_at_ms {
            return;
        }
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
            Exchange::Gate | Exchange::Hyperliquid | Exchange::Lighter | Exchange::Pacifica
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
        if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask < bid {
            return;
        }
        self.fill_quotes.push(quote.time_ms, bid, ask, utc_now_ms());
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
    audio: audio::TradeAudio,
    private: private::Controller,
    workspaces: Vec<Tree<Pane>>,
    active_workspace: usize,
    tab_hover: Option<(usize, f64)>,
    batch_change: Option<batch::Dialog>,
    batch_undo: Option<batch::Undo>,
    data: HashMap<Market, MarketData>,
    catalogs: HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
    catalog_errors: HashMap<(Exchange, MarketKind), String>,
    catalog_requests: HashSet<(Exchange, MarketKind)>,
    subscribed: HashSet<Market>,
    accounts: Vec<Account>,
    hidden_widget_accounts: HashSet<Account>,
    account_data: HashMap<Account, AccountState>,
    fills: fills::Store,
    subscribed_accounts: HashSet<Account>,
    account_ui: accounts::Menu,
    account_address: String,
    account_error: Option<String>,
    focused: Option<TileId>,
    socket_open: bool,
    sender: Option<WsSender>,
    receiver: Option<socket::Inbox>,
    reconnect_at: u64,
    resync_on_visible: bool,
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
            audio: audio::TradeAudio::new(
                cc.storage
                    .and_then(|storage| storage.get_string("trade_audio"))
                    .and_then(|saved| serde_json::from_str(&saved).ok())
                    .unwrap_or_default(),
            ),
            private: private::Controller::default(),
            workspaces,
            active_workspace,
            tab_hover: None,
            batch_change: None,
            batch_undo: None,
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
                .filter(|a| a.valid() || private::is_private(a))
                .collect(),
            hidden_widget_accounts: cc
                .storage
                .and_then(|storage| storage.get_string("hidden_widget_accounts"))
                .and_then(|saved| serde_json::from_str(&saved).ok())
                .unwrap_or_default(),
            account_data: HashMap::new(),
            fills: fills::Store::default(),
            subscribed_accounts: HashSet::new(),
            account_ui: accounts::Menu::default(),
            account_address: String::new(),
            account_error: None,
            focused: None,
            socket_open: false,
            sender: None,
            receiver: None,
            reconnect_at: 0,
            resync_on_visible: false,
        };
        #[cfg(target_arch = "wasm32")]
        app.private.vault.load(&cc.egui_ctx);
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
        match socket::connect(ws_url(), ctx) {
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
                if matches!(pane.kind, WidgetKind::Fills) {
                    wanted.extend(self.fills.wanted_markets(
                        &pane.fills,
                        &self.hidden_widget_accounts,
                        utc_now_ms(),
                    ));
                } else if matches!(pane.kind, WidgetKind::Position) {
                    if pane.market.kind == MarketKind::Spot
                        && pane.position.metric == position::Metric::Notional
                        && !pane.unavailable_markets.contains(&pane.market)
                    {
                        wanted.insert(pane.market.clone());
                    }
                } else if pane.uses_sources() {
                    wanted.extend(
                        pane.series
                            .iter()
                            .filter(|market| !pane.unavailable_markets.contains(market))
                            .cloned(),
                    );
                } else if !pane.unavailable_markets.contains(&pane.market) {
                    wanted.insert(pane.market.clone());
                }
            }
        }
        // POSITION history keeps recording across terminal switches. Spot
        // notional therefore keeps its price source while that widget exists.
        for tree in &self.workspaces {
            for (_, tile) in tree.tiles.iter() {
                if let Tile::Pane(pane) = tile
                    && matches!(pane.kind, WidgetKind::Position)
                    && !pane.unavailable_markets.contains(&pane.market)
                    && pane.market.kind == MarketKind::Spot
                    && pane.position.metric == position::Metric::Notional
                {
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
        let wanted_accounts: HashSet<_> = self
            .accounts
            .iter()
            .filter(|a| !private::is_private(a))
            .cloned()
            .collect();
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
            self.fills.histories.remove(&account);
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

    fn record_positions(&mut self, changed_market: Option<&Market>) {
        let now = utc_now_ms();
        for tree in &mut self.workspaces {
            for (_, tile) in tree.tiles.iter_mut() {
                if let Tile::Pane(pane) = tile
                    && matches!(pane.kind, WidgetKind::Position)
                    && !pane.unavailable_markets.contains(&pane.market)
                    && changed_market.is_none_or(|market| market == &pane.market)
                {
                    pane.position_view.observe(
                        &pane.market,
                        &self.accounts,
                        &self.account_data,
                        orderflow::symbol_info(&pane.market, &self.catalogs),
                        self.data.get(&pane.market),
                        now,
                    );
                }
            }
        }
    }

    fn poll_socket(&mut self, ctx: &egui::Context) {
        let started = socket::now_ms();
        if self
            .receiver
            .as_ref()
            .is_some_and(|receiver| receiver.needs_resync(started))
        {
            self.disconnect(ctx);
            self.reconnect_at = started;
            self.resync_on_visible = ctx.input(|input| input.viewport().visible() == Some(false));
            return;
        }
        for index in 0..SOCKET_EVENTS_PER_PASS {
            // A snapshot/delta message stays atomic; yield between messages.
            if index > 0 && socket::now_ms().saturating_sub(started) >= SOCKET_BUDGET_MS {
                break;
            }
            let Some(event) = self.receiver.as_ref().and_then(socket::Inbox::try_recv) else {
                break;
            };
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
                            self.record_positions(None);
                            continue;
                        }
                        if let ServerMessage::AccountFills { account, fills, .. } = message {
                            if self.subscribed_accounts.contains(&account) {
                                self.fills.push(account, fills);
                            }
                            continue;
                        }
                        if let ServerMessage::AccountPosition { account, position } = message {
                            if self.subscribed_accounts.contains(&account) {
                                let market = Market {
                                    exchange: account.exchange,
                                    kind: MarketKind::Perp,
                                    symbol: position.coin.clone(),
                                };
                                if self
                                    .account_data
                                    .entry(account)
                                    .or_default()
                                    .apply_live_position(position)
                                {
                                    self.record_positions(Some(&market));
                                }
                            }
                            continue;
                        }
                        if let ServerMessage::AccountState { account, state } = message {
                            if self.subscribed_accounts.contains(&account) {
                                self.account_data.insert(account, state);
                                self.record_positions(None);
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
                        let data = self.data.entry(market.clone()).or_default();
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
                                self.audio.trades(&trades, utc_now_ms());
                                for trade in trades {
                                    data.push_trade(trade);
                                }
                            }
                            ServerMessage::Status { connected, .. } => {
                                data.connected = connected;
                                if !connected {
                                    data.book = None;
                                    data.best_bid_ask = None;
                                    data.fill_quotes = fills::QuoteHistory::default();
                                }
                            }
                            ServerMessage::Symbols { .. } => {}
                            ServerMessage::AccountState { .. }
                            | ServerMessage::AccountFills { .. }
                            | ServerMessage::AccountPosition { .. } => {}
                        }
                        self.record_positions(Some(&market));
                    }
                }
                WsEvent::Closed | WsEvent::Error(_) => {
                    self.disconnect(ctx);
                    break;
                }
                _ => {}
            }
        }
        if self
            .receiver
            .as_ref()
            .is_some_and(socket::Inbox::has_pending)
        {
            ctx.request_repaint();
        }
    }

    fn disconnect(&mut self, ctx: &egui::Context) {
        self.audio.clear_pending();
        self.socket_open = false;
        self.subscribed.clear();
        self.subscribed_accounts.clear();
        self.account_data
            .retain(|account, _| private::is_private(account));
        for data in self.data.values_mut() {
            data.connected = false;
            data.book = None;
            data.best_bid_ask = None;
            data.fill_quotes = fills::QuoteHistory::default();
        }
        self.record_positions(None);
        self.sender = None;
        self.receiver = None;
        self.reconnect_at = socket::now_ms() + 3_000;
        ctx.request_repaint_after(Duration::from_secs(3));
    }

    fn header(&mut self, ui: &mut egui::Ui) -> Option<WidgetKind> {
        let mut add = None;
        let mut switch_to = None;
        let mut remove_workspace = None;
        let mut create_workspace = false;
        let mut change_symbol = None;
        let now = ui.input(|input| input.time);
        let mut hovered_tab = None;
        let header = ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), 36.0),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.add_space(10.0);
                let tabs_width = (ui.available_width() - 295.0).max(100.0);
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
                        response.context_menu(|ui| {
                            ui.set_min_width(160.0);
                            ui.label(
                                RichText::new(format!("Terminal {}", index + 1))
                                    .small()
                                    .color(MUTED),
                            );
                            ui.separator();
                            if ui.button("Change symbol…").clicked() {
                                change_symbol = Some(index);
                                ui.close();
                            }
                        });
                        if response.clicked() {
                            switch_to = Some(index);
                        }
                        let badge_center =
                            egui::pos2(response.rect.right() - 5.0, response.rect.top() + 5.0);
                        let badge_rect =
                            egui::Rect::from_center_size(badge_center, egui::vec2(16.0, 16.0));
                        let corner_rect = egui::Rect::from_min_size(
                            egui::pos2(response.rect.right() - 10.0, response.rect.top()),
                            egui::vec2(10.0, 10.0),
                        );
                        let badge_visible = self.tab_hover.is_some_and(|(hovered, started)| {
                            hovered == index && now - started >= TAB_DELETE_HOVER_SECS
                        });
                        let pointer = ui.input(|input| input.pointer.hover_pos());
                        let hovering = self.workspaces.len() > 1
                            && !response.context_menu_opened()
                            && pointer.is_some_and(|pointer| {
                                if badge_visible {
                                    badge_rect.contains(pointer)
                                } else {
                                    corner_rect.contains(pointer)
                                }
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
                                        "Terminal {}{}\nRight-click for terminal actions",
                                        index + 1,
                                        shortcut
                                    ));
                                }
                            } else if self.workspaces.len() > 1 {
                                ui.ctx().request_repaint_after(Duration::from_secs_f64(
                                    (TAB_DELETE_HOVER_SECS - (now - started)).max(0.0),
                                ));
                                response.on_hover_text(format!(
                                    "Terminal {}{}\nRight-click for terminal actions",
                                    index + 1,
                                    shortcut
                                ));
                            }
                        } else {
                            response.on_hover_text(format!(
                                "Terminal {}{}\nRight-click for terminal actions",
                                index + 1,
                                shortcut
                            ));
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
                        RichText::new("Add Widget").strong().size(12.0).color(BG),
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
                                WidgetKind::Dom,
                                WidgetKind::Fills,
                                WidgetKind::Position,
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
                    self.audio.menu(ui);
                    ui.add_space(4.0);
                    let now_ms = utc_now_ms();
                    let seconds = now_ms.div_euclid(1_000).rem_euclid(86_400);
                    ui.label(
                        RichText::new(format!(
                            "{:02}:{:02}:{:02} UTC",
                            seconds / 3_600,
                            seconds / 60 % 60,
                            seconds % 60,
                        ))
                        .monospace()
                        .size(12.0)
                        .color(MUTED),
                    );
                    ui.ctx().request_repaint_after(Duration::from_millis(
                        (1_000 - now_ms.rem_euclid(1_000)) as u64,
                    ));
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
        if let Some(index) = change_symbol {
            self.batch_change = Some(batch::Dialog::new(index));
        }
        add
    }

    fn switch_workspace(&mut self, index: usize) {
        if index < self.workspaces.len() && index != self.active_workspace {
            self.audio.clear_pending();
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
        self.batch_change = None;
        self.batch_undo = None;
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
        ui.checkbox(&mut pane.compare_percent, "Compare % change")
            .on_hover_text("Compare all prices against the latest trade of the first source (0%)");
    }
    ui.checkbox(&mut pane.orderflow.enabled, "Show orderflow")
        .on_hover_text("Live buy/sell volume grouped into UTC seconds; hover for delta");
    ui.checkbox(&mut pane.compare_show_fills, "Show my fills");
    ui.checkbox(&mut pane.compare_show_orders, "Show account orders")
        .on_hover_text("Show open orders from visible accounts on this Prices widget");
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

fn source_move_button(ui: &mut egui::Ui, up: bool, enabled: bool) -> egui::Response {
    let response = ui.add_enabled(
        enabled,
        egui::Button::new("")
            .small()
            .min_size(egui::vec2(20.0, 20.0)),
    );
    let center = response.rect.center();
    let direction = if up { 1.0 } else { -1.0 };
    ui.painter().add(egui::Shape::line(
        vec![
            center + egui::vec2(-3.5, 2.0 * direction),
            center + egui::vec2(0.0, -2.0 * direction),
            center + egui::vec2(3.5, 2.0 * direction),
        ],
        egui::Stroke::new(
            1.2,
            if enabled {
                TEXT
            } else {
                MUTED.gamma_multiply(0.4)
            },
        ),
    ));
    response.on_hover_text(if up {
        "Move source up"
    } else {
        "Move source down"
    })
}

fn sources_list(ui: &mut egui::Ui, pane: &mut Pane) {
    ui.label(RichText::new("SOURCES").small().color(MUTED));
    let mut remove = None;
    let mut reorder = None;
    egui::ScrollArea::vertical()
        .id_salt("comparison-sources")
        .max_width(280.0)
        .max_height(118.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for (index, market) in pane.series.iter().enumerate() {
                ui.horizontal(|ui| {
                    let controls_width = if pane.series.len() > 1 {
                        60.0 + ui.spacing().item_spacing.x * 3.0
                    } else {
                        0.0
                    };
                    let name = format!(
                        "{} {}  {}",
                        market.exchange.label(),
                        market.kind.label(),
                        market.symbol
                    );
                    if ui
                        .add_sized(
                            [(ui.available_width() - controls_width).max(40.0), 20.0],
                            egui::Button::selectable(pane.editing_series == index, "")
                                .left_text(
                                    RichText::new(&name).color(view::venue_color(market.exchange)),
                                )
                                .truncate(),
                        )
                        .on_hover_text(name)
                        .clicked()
                    {
                        pane.editing_series = index;
                        pane.search.clear();
                    }
                    if pane.series.len() > 1 {
                        if source_move_button(ui, true, index > 0).clicked() {
                            reorder = Some((index, index - 1));
                        }
                        if source_move_button(ui, false, index + 1 < pane.series.len()).clicked() {
                            reorder = Some((index, index + 1));
                        }
                        if ui
                            .small_button("×")
                            .on_hover_text("Remove source")
                            .clicked()
                        {
                            remove = Some(index);
                        }
                    }
                });
            }
        });
    if let Some(index) = remove {
        pane.series.remove(index);
        if index < pane.editing_series {
            pane.editing_series -= 1;
        }
        pane.editing_series = pane.editing_series.min(pane.series.len() - 1);
        pane.search.clear();
    } else if let Some((from, to)) = reorder {
        pane.swap_sources(from, to);
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

fn visible_account_states<'a>(
    market: &'a Market,
    states: &'a HashMap<Account, AccountState>,
    hidden: &'a HashSet<Account>,
) -> impl Iterator<Item = (&'a Account, &'a AccountState)> + 'a {
    states.iter().filter(move |(account, state)| {
        account.exchange == market.exchange && state.connected && !hidden.contains(*account)
    })
}

fn visible_source_orders<'a>(
    sources: &'a [Market],
    states: &'a HashMap<Account, AccountState>,
    hidden: &'a HashSet<Account>,
) -> Vec<(usize, &'a Account, &'a OwnOrder)> {
    let mut orders = Vec::new();
    for (index, market) in sources.iter().enumerate() {
        for (account, state) in visible_account_states(market, states, hidden) {
            for order in &state.orders {
                if order.matches_market(market) {
                    orders.push((index, account, order));
                }
            }
        }
    }
    orders
}

struct PaneBehavior<'a> {
    accounts: &'a [Account],
    fills: &'a fills::Store,
    data: &'a HashMap<Market, MarketData>,
    account_data: &'a HashMap<Account, AccountState>,
    hidden_widget_accounts: &'a HashSet<Account>,
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
        let old_compare = (pane.series.clone(), pane.compare_mode, pane.compare_percent);
        let live = if !pane.unavailable_markets.is_empty() {
            false
        } else if matches!(pane.kind, WidgetKind::Position) {
            position::is_live(
                &pane.position,
                &pane.market,
                self.accounts,
                self.hidden_widget_accounts,
                self.account_data,
            ) && (pane.market.kind != MarketKind::Spot
                || pane.position.metric != position::Metric::Notional
                || self
                    .data
                    .get(&pane.market)
                    .is_some_and(|data| data.connected && data.best_bid_ask.is_some()))
        } else if matches!(pane.kind, WidgetKind::Fills) {
            self.accounts.iter().any(|account| {
                !self.hidden_widget_accounts.contains(account)
                    && self
                        .account_data
                        .get(account)
                        .is_some_and(|state| state.connected)
            })
        } else if pane.uses_sources() {
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
                let subtitle =
                    if pane.unavailable_markets.contains(&pane.market) && !pane.uses_sources() {
                        format!(
                            "{} {}  {}",
                            pane.market.exchange.label(),
                            pane.market.kind.label(),
                            pane.batch_symbol.as_deref().unwrap_or(&pane.market.symbol)
                        )
                    } else if matches!(pane.kind, WidgetKind::Fills) {
                        "Connected accounts".to_owned()
                    } else if matches!(pane.kind, WidgetKind::Position)
                        && pane.market.kind == MarketKind::Spot
                    {
                        orderflow::symbol_info(&pane.market, self.catalogs).map_or_else(
                            || {
                                format!(
                                    "{} Spot {}",
                                    pane.market.exchange.label(),
                                    pane.market.symbol
                                )
                            },
                            |info| {
                                format!(
                                    "{} Spot {}/{}",
                                    pane.market.exchange.label(),
                                    info.base,
                                    info.quote
                                )
                            },
                        )
                    } else if pane.uses_sources()
                        && (!matches!(pane.kind, WidgetKind::Dom) || pane.series.len() > 1)
                    {
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
                                WidgetKind::Position => {
                                    position::config_ui(
                                        ui,
                                        &mut pane.position,
                                        &mut pane.market,
                                        &mut pane.search,
                                        self.accounts,
                                        self.hidden_widget_accounts,
                                        self.account_data,
                                        self.catalogs,
                                        self.catalog_errors,
                                        self.retry_catalogs,
                                    );
                                }
                                WidgetKind::Fills => {
                                    fills::config_ui(
                                        ui,
                                        &mut pane.fills,
                                        &mut pane.fills_view,
                                        self.accounts,
                                        self.hidden_widget_accounts,
                                        self.fills,
                                        self.catalogs,
                                    );
                                }
                                WidgetKind::Dom => {
                                    let previous_sources = pane.series.len();
                                    sources_list(ui, pane);
                                    if previous_sources == 1 && pane.series.len() > 1 {
                                        pane.dom.enable_multi();
                                    }
                                    ui.separator();
                                    dom::config_ui(ui, &mut pane.dom);
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
                    response.on_hover_text(if !pane.unavailable_markets.is_empty() {
                        "Market unavailable"
                    } else if live {
                        "Live"
                    } else {
                        "Connecting"
                    });
                });
                title.drag_started()
            })
            .inner;
        if old_market != pane.market {
            pane.unavailable_markets
                .retain(|market| market != &old_market);
            pane.chart = view::ChartView::default();
            pane.book_view = view::BookView::default();
            pane.dom_view = dom::View::default();
            pane.position_view = position::View::default();
        }
        if old_compare.0 != pane.series
            || old_compare.1 != pane.compare_mode
            || old_compare.2 != pane.compare_percent
        {
            if old_compare.0 != pane.series {
                if matches!(pane.kind, WidgetKind::Dom) {
                    if let Some(first) = pane.series.first() {
                        pane.market = first.clone();
                    }
                    pane.dom_view = dom::View::default();
                }
                pane.unavailable_markets
                    .retain(|market| pane.series.contains(market));
            }
            pane.compare_view = view::YAxisView::default();
        }
        ui.separator();
        if !pane.unavailable_markets.is_empty() {
            for market in &pane.unavailable_markets {
                ui.label(
                    RichText::new(format!(
                        "{} unavailable on {} {}",
                        pane.batch_symbol.as_deref().unwrap_or(&market.symbol),
                        market.exchange.label(),
                        market.kind.label()
                    ))
                    .small()
                    .color(MUTED),
                );
            }
            if !pane.uses_sources() {
                return if dragging {
                    UiResponse::DragStarted
                } else {
                    UiResponse::None
                };
            }
        }
        let available_sources;
        let sources = if pane.unavailable_markets.is_empty() {
            &pane.series
        } else {
            available_sources = pane
                .series
                .iter()
                .filter(|market| !pane.unavailable_markets.contains(market))
                .cloned()
                .collect::<Vec<_>>();
            &available_sources
        };
        let data = self.data.get(&pane.market);
        let market = &pane.market;
        let own_orders: Vec<_> =
            visible_account_states(market, self.account_data, self.hidden_widget_accounts)
                .flat_map(|(account, state)| {
                    state.orders.iter().filter_map(move |order| {
                        order.matches_market(market).then_some((account, order))
                    })
                })
                .collect();
        let positions: Vec<_> =
            visible_account_states(market, self.account_data, self.hidden_widget_accounts)
                .flat_map(|(account, state)| {
                    state.positions.iter().filter_map(move |position| {
                        (market.kind == MarketKind::Perp && position.coin == market.symbol)
                            .then_some((account, position))
                    })
                })
                .collect();
        match pane.kind {
            WidgetKind::Chart => {
                let previous = pane.chart_timeframe;
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    egui::ComboBox::from_id_salt("chart-timeframe")
                        .width(56.0)
                        .selected_text(pane.chart_timeframe.label())
                        .show_ui(ui, |ui| {
                            for timeframe in candles::Timeframe::ALL {
                                ui.selectable_value(&mut pane.chart_timeframe, timeframe, timeframe.label());
                            }
                        })
                        .response.on_hover_text("Timeframe · UTC\n1s / 5s / 30s: built from received trades, up to 10 minutes.\nOther intervals use available 1m candle history.\nThe first and current candles may be partial; missing trades are not backfilled.");
                });
                if pane.chart_timeframe != previous {
                    pane.chart = view::ChartView::default();
                }
                let candles = data.map_or_else(Vec::new, |data| {
                    if pane.chart_timeframe.uses_trades() {
                        data.trade_candles.candles(pane.chart_timeframe)
                    } else {
                        candles::aggregate(data.candles.iter(), pane.chart_timeframe)
                    }
                });
                view::chart_ui(
                    ui,
                    &candles,
                    pane.chart_timeframe,
                    &mut pane.chart,
                    &own_orders,
                    &positions,
                );
            }
            WidgetKind::Book => view::book_ui(
                ui,
                data.and_then(|data| data.book.as_ref()),
                &mut pane.book_view,
                &own_orders,
            ),
            WidgetKind::Compare => {
                self.needed_catalogs.extend(
                    pane.series
                        .iter()
                        .map(|market| (market.exchange, market.kind)),
                );
                let orders = if pane.compare_show_orders {
                    visible_source_orders(sources, self.account_data, self.hidden_widget_accounts)
                } else {
                    Vec::new()
                };
                let own_fills = if pane.compare_show_fills {
                    self.fills
                        .visible_sources(sources, self.accounts, self.hidden_widget_accounts)
                } else {
                    Vec::new()
                };
                view::compare_ui(
                    ui,
                    sources,
                    self.data,
                    pane.compare_mode,
                    pane.compare_percent,
                    pane.compare_window_secs,
                    &mut pane.orderflow,
                    self.catalogs,
                    &orders,
                    &own_fills,
                    &mut pane.compare_view,
                );
            }
            WidgetKind::Tape => view::tape_ui(ui, sources, self.data),
            WidgetKind::Position => {
                self.needed_catalogs
                    .insert((pane.market.exchange, pane.market.kind));
                position::ui(
                    ui,
                    market,
                    &pane.position,
                    &mut pane.position_view,
                    self.accounts,
                    self.hidden_widget_accounts,
                    self.account_data,
                    data,
                    orderflow::symbol_info(market, self.catalogs),
                    self.fills,
                );
            }

            WidgetKind::Fills => {
                for account in self.accounts {
                    for kind in MarketKind::ALL {
                        self.needed_catalogs.insert((account.exchange, kind));
                    }
                }
                fills::ui(
                    ui,
                    &pane.fills,
                    &mut pane.fills_view,
                    self.fills,
                    self.accounts,
                    self.hidden_widget_accounts,
                    self.data,
                    self.catalogs,
                );
            }
            WidgetKind::Dom => {
                let own_fills = if pane.dom.shows_fills() {
                    self.fills
                        .visible_sources(sources, self.accounts, self.hidden_widget_accounts)
                } else {
                    Vec::new()
                };
                self.needed_catalogs
                    .extend(sources.iter().map(|market| (market.exchange, market.kind)));
                if pane.series.len() > 1 {
                    let orders = visible_source_orders(
                        sources,
                        self.account_data,
                        self.hidden_widget_accounts,
                    );
                    dom::multi_ui(
                        ui,
                        sources,
                        self.data,
                        self.catalogs,
                        &pane.dom,
                        &mut pane.dom_view,
                        &orders,
                        &own_fills,
                    );
                } else if sources.is_empty() {
                    ui.weak("No available DOM sources. Choose a market in config.");
                } else {
                    dom::ui(
                        ui,
                        data,
                        orderflow::symbol_info(market, self.catalogs),
                        &pane.dom,
                        &mut pane.dom_view,
                        &own_orders,
                        &own_fills,
                        self.catalogs,
                        market,
                    );
                }
            }
        }
        if dragging {
            UiResponse::DragStarted
        } else {
            UiResponse::None
        }
    }

    fn tab_title_for_pane(&mut self, pane: &Pane) -> egui::WidgetText {
        if matches!(pane.kind, WidgetKind::Fills) {
            "FILLS".into()
        } else if pane.uses_sources()
            && (!matches!(pane.kind, WidgetKind::Dom) || pane.series.len() > 1)
        {
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
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_socket(ctx);
        let markets = self
            .workspaces
            .iter()
            .flat_map(|tree| tree.tiles.iter())
            .filter_map(|(_, tile)| match tile {
                Tile::Pane(pane) => Some(pane),
                _ => None,
            })
            .flat_map(|pane| std::iter::once(pane.market.clone()).chain(pane.series.clone()))
            .collect();
        if self.private.poll(
            ctx,
            &mut self.accounts,
            &mut self.account_data,
            &mut self.fills,
            markets,
        ) {
            self.record_positions(None);
        }
        let visible = ctx.input(|input| input.viewport().visible() != Some(false));
        if self.resync_on_visible && !visible {
            self.audio.clear_pending();
            return;
        }
        self.resync_on_visible = false;
        let now = socket::now_ms();
        if self.receiver.is_none() && now >= self.reconnect_at {
            self.reconnect_at = now + 3_000;
            self.connect(ctx);
        }
        if self.receiver.is_none() {
            ctx.request_repaint_after(Duration::from_millis(
                self.reconnect_at.saturating_sub(now).max(1),
            ));
        }
        if !visible {
            self.audio.clear_pending();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if ui.visuals().window_fill != terminal_visuals().window_fill {
            ui.ctx().set_visuals(terminal_visuals());
        }
        self.handle_workspace_shortcuts(ui.ctx());

        egui::Frame::new().fill(BG).show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            let add = self.header(ui);
            batch::ui(self, ui.ctx());
            let mut close = None;
            let mut needed_catalogs = HashSet::new();
            let mut retry_catalogs = HashSet::new();
            {
                let mut behavior = PaneBehavior {
                    accounts: &self.accounts,
                    fills: &self.fills,
                    data: &self.data,
                    account_data: &self.account_data,
                    hidden_widget_accounts: &self.hidden_widget_accounts,
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
        self.audio.flush(ui.ctx());
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let Ok(settings) = serde_json::to_string(self.audio.settings()) {
            storage.set_string("trade_audio", settings);
        }
        if let Ok(workspaces) = serde_json::to_string(&self.workspaces) {
            storage.set_string("terminal_workspaces", workspaces);
            storage.set_string("active_terminal", self.active_workspace.to_string());
        }
        if let Ok(accounts) = serde_json::to_string(&self.accounts) {
            storage.set_string("accounts", accounts);
        }
        if let Ok(hidden) = serde_json::to_string(&self.hidden_widget_accounts) {
            storage.set_string("hidden_widget_accounts", hidden);
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
        "Terminal",
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

    #[test]
    fn compact_accounts_keep_details_scrolling_and_removal_isolated() {
        fn texts(output: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
            fn collect(
                shape: &egui::Shape,
                clip: egui::Rect,
                texts: &mut Vec<(String, egui::Rect)>,
            ) {
                match shape {
                    egui::Shape::Text(text) => {
                        let rect = text.galley.rect.translate(text.pos.to_vec2());
                        if clip.intersects(rect) {
                            texts.push((text.galley.text().to_owned(), rect));
                        }
                    }
                    egui::Shape::Vec(shapes) => {
                        for shape in shapes {
                            collect(shape, clip, texts);
                        }
                    }
                    _ => {}
                }
            }
            let mut texts = Vec::new();
            for shape in &output.shapes {
                collect(&shape.shape, shape.clip_rect, &mut texts);
            }
            texts
        }
        fn draw(
            ctx: &egui::Context,
            app: &mut TerminalApp,
            events: Vec<egui::Event>,
        ) -> egui::FullOutput {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 900.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| {
                    egui::Popup::open_id(ui.ctx(), ui.next_auto_id().with("popup"));
                    app.account_menu(ui);
                },
            );
            output.textures_delta.clear();
            output
        }
        fn click(ctx: &egui::Context, app: &mut TerminalApp, pos: egui::Pos2) {
            for pressed in [true, false] {
                draw(
                    ctx,
                    app,
                    vec![
                        egui::Event::PointerMoved(pos),
                        egui::Event::PointerButton {
                            pos,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: Default::default(),
                        },
                    ],
                );
            }
        }
        fn label(ctx: &egui::Context, app: &mut TerminalApp, name: &str) -> egui::Pos2 {
            texts(&draw(ctx, app, Vec::new()))
                .into_iter()
                .find(|(text, _)| text == name)
                .unwrap_or_else(|| panic!("Missing {name}"))
                .1
                .center()
        }
        let mut app = test_app();
        for (index, address) in [
            "0xecb63caa47c7c4e77f60f1ce858cf28dc2b82b00",
            "0x223537ac9a856c31f4043e86ced86bb29f06653e",
            "0xd0956a1eba0a9060baf5f73ee26b9601dc47ec43",
        ]
        .into_iter()
        .enumerate()
        {
            let account = Account {
                exchange: Exchange::Hyperliquid,
                address: address.into(),
            };
            app.account_data.insert(
                account.clone(),
                AccountState {
                    connected: true,
                    orders: (0..50)
                        .map(|row| OwnOrder {
                            kind: None,
                            external_id: None,
                            coin: format!("ACCOUNT{index}-ROW{row}"),
                            order_id: row,
                            side: TradeSide::Buy,
                            price: "100".into(),
                            size: "1".into(),
                        })
                        .collect(),
                    ..Default::default()
                },
            );
            app.accounts.push(account);
        }
        let ctx = egui::Context::default();
        ctx.global_style_mut(|s| s.animation_time = 0.0);
        draw(&ctx, &mut app, Vec::new());
        let initial = texts(&draw(&ctx, &mut app, Vec::new()));
        assert!(!initial.iter().any(|(t, _)| t == "View live state"
            || t == "Remove account"
            || t.contains("password")));
        let arrows: Vec<_> = initial.iter().filter(|(t, _)| t == "›").collect();
        assert_eq!(arrows.len(), 3);
        click(&ctx, &mut app, arrows[2].1.center());
        let pos = label(&ctx, &mut app, "View live state");
        click(&ctx, &mut app, pos);
        let visible = texts(&draw(&ctx, &mut app, Vec::new()));
        assert!(visible.iter().any(|(t, _)| t.starts_with("ACCOUNT2-ROW0 ")));
        assert!(
            !visible
                .iter()
                .any(|(t, _)| t.starts_with("ACCOUNT0-") || t.starts_with("ACCOUNT1-"))
        );
        let pos = visible
            .iter()
            .find(|(t, _)| t.starts_with("ACCOUNT2-ROW0 "))
            .unwrap()
            .1
            .center();
        draw(
            &ctx,
            &mut app,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, -120.0),
                    phase: egui::TouchPhase::Move,
                    modifiers: Default::default(),
                },
            ],
        );
        for _ in 0..20 {
            draw(&ctx, &mut app, Vec::new());
        }
        assert!(
            !texts(&draw(&ctx, &mut app, Vec::new()))
                .iter()
                .any(|(t, _)| t.starts_with("ACCOUNT2-ROW0 "))
        );
        let pos = label(&ctx, &mut app, "‹ Accounts");
        click(&ctx, &mut app, pos);
        let initial = texts(&draw(&ctx, &mut app, Vec::new()));
        let first = initial.iter().find(|(t, _)| t == "›").unwrap().1.center();
        click(&ctx, &mut app, first);
        let pos = label(&ctx, &mut app, "View live state");
        click(&ctx, &mut app, pos);
        assert!(
            texts(&draw(&ctx, &mut app, Vec::new()))
                .iter()
                .any(|(t, _)| t.starts_with("ACCOUNT0-ROW0 "))
        );
        let pos = label(&ctx, &mut app, "View live state");
        click(&ctx, &mut app, pos);
        let pos = label(&ctx, &mut app, "Remove account");
        click(&ctx, &mut app, pos);
        assert_eq!(
            app.accounts.len(),
            3,
            "first click only asks for confirmation"
        );
        let pos = label(&ctx, &mut app, "Cancel");
        click(&ctx, &mut app, pos);
        assert_eq!(app.accounts.len(), 3);
        let removed = app.accounts[0].clone();
        let pos = label(&ctx, &mut app, "Remove account");
        click(&ctx, &mut app, pos);
        let pos = label(&ctx, &mut app, "Remove account");
        click(&ctx, &mut app, pos);
        assert_eq!(app.accounts.len(), 2);
        assert!(!app.account_data.contains_key(&removed));
        let pos = label(&ctx, &mut app, "+ Connect account");
        click(&ctx, &mut app, pos);
        let choices = texts(&draw(&ctx, &mut app, Vec::new()));
        for venue in ["Hyperliquid", "Binance", "Bybit", "OKX"] {
            assert!(choices.iter().any(|(t, _)| t == venue));
        }
        assert!(
            !choices
                .iter()
                .any(|(t, _)| t.contains("password") || t == "0x…")
        );
        let pos = label(&ctx, &mut app, "Hyperliquid");
        click(&ctx, &mut app, pos);
        app.account_address = "invalid".into();
        let pos = label(&ctx, &mut app, "Connect account");
        click(&ctx, &mut app, pos);
        assert!(app.account_error.is_some());
        assert_eq!(app.accounts.len(), 2);
    }

    #[test]
    fn deferred_socket_backlog_yields_before_blocking_a_frame() {
        let mut app = test_app();
        let market = Market::default();
        app.subscribed.insert(market.clone());
        let (receiver, on_event) = socket::channel(|| {});
        app.receiver = Some(receiver);
        let text = serde_json::to_string(&ServerMessage::Trades {
            market: market.clone(),
            trades: vec![Trade {
                time_ms: 1_000,
                price: "100".into(),
                size: "1".into(),
                side: TradeSide::Buy,
            }],
        })
        .unwrap();
        for _ in 0..10_000 {
            assert!(on_event(WsEvent::Message(WsMessage::Text(text.clone()))).is_continue());
        }
        let started = std::time::Instant::now();
        app.poll_socket(&egui::Context::default());
        let processed = app
            .data
            .get(&market)
            .map_or(0, |data| data.price_trades.len());
        eprintln!(
            "deferred 10000 messages: one poll processed {processed} in {:?}",
            started.elapsed()
        );
        assert!(
            processed > 0 && processed <= 256,
            "a deferred socket queue must yield to UI instead of draining in one frame"
        );
        while app
            .receiver
            .as_ref()
            .is_some_and(socket::Inbox::has_pending)
        {
            app.poll_socket(&egui::Context::default());
        }
        assert_eq!(
            app.data[&market].price_trades.len(),
            10_000,
            "yielding must not lose trades"
        );
    }

    #[test]
    fn hidden_logic_consumes_feed_without_rendering_ui() {
        let mut app = test_app();
        let market = Market::default();
        app.subscribed.insert(market.clone());
        let (receiver, on_event) = socket::channel(|| {});
        app.receiver = Some(receiver);
        let message = ServerMessage::Trades {
            market: market.clone(),
            trades: vec![Trade {
                time_ms: 1_000,
                price: "100".into(),
                size: "1".into(),
                side: TradeSide::Buy,
            }],
        };
        assert!(
            on_event(WsEvent::Message(WsMessage::Text(
                serde_json::to_string(&message).unwrap()
            )))
            .is_continue()
        );
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .occluded = Some(true);
        let _ = ctx.run_logic(&input, |ctx| {
            eframe::App::logic(&mut app, ctx, &mut eframe::Frame::_new_kittest());
        });
        assert_eq!(
            app.data.get(&market).map(|data| data.price_trades.len()),
            Some(1),
            "hidden logic must consume market data without calling ui"
        );
    }

    #[test]
    fn overflow_invalidates_books_and_recovers_only_from_a_fresh_snapshot() {
        let mut app = test_app();
        let market = Market::default();
        app.socket_open = true;
        app.subscribed.insert(market.clone());
        app.data.insert(
            market.clone(),
            MarketData {
                connected: true,
                book: Some(Book {
                    bids: Vec::new(),
                    asks: Vec::new(),
                    updated_at_ms: 1,
                }),
                ..Default::default()
            },
        );
        let (receiver, on_event) = socket::channel(|| {});
        app.receiver = Some(receiver);
        assert!(
            on_event(WsEvent::Message(WsMessage::Binary(vec![
                0;
                17 * 1024 * 1024
            ])))
            .is_break()
        );
        let ctx = egui::Context::default();
        app.poll_socket(&ctx);
        assert!(app.receiver.is_none());
        assert!(!app.data[&market].connected);
        assert!(app.data[&market].book.is_none());
        assert!(on_event(WsEvent::Opened).is_break());
        let (receiver, on_event) = socket::channel(|| {});
        app.receiver = Some(receiver);
        assert!(on_event(WsEvent::Opened).is_continue());
        let level = |price: &str| Level {
            price: price.into(),
            size: "1".into(),
            quote_size: String::new(),
            depth_base: String::new(),
            depth_quote: String::new(),
        };
        let snapshot = ServerMessage::Snapshot {
            market: market.clone(),
            candles: Vec::new(),
            book: Some(Book {
                bids: vec![level("100")],
                asks: vec![level("101")],
                updated_at_ms: 2,
            }),
            best_bid_ask: None,
            last_price: None,
            connected: true,
        };
        assert!(
            on_event(WsEvent::Message(WsMessage::Text(
                serde_json::to_string(&snapshot).unwrap()
            )))
            .is_continue()
        );
        app.poll_socket(&ctx);
        assert!(app.data[&market].connected);
        assert_eq!(
            app.data[&market].book.as_ref().unwrap().bids[0].price,
            "100"
        );
        let data = app.data.get_mut(&market).unwrap();
        data.apply_book_delta(
            vec![BookChange {
                price: "100".into(),
                size: "99".into(),
            }],
            Vec::new(),
            1,
            market.exchange,
        );
        assert_eq!(
            data.book.as_ref().unwrap().bids[0].size,
            "1",
            "a delayed delta cannot roll back a newer snapshot"
        );
        data.set_book(
            Book {
                bids: Vec::new(),
                asks: Vec::new(),
                updated_at_ms: 1,
            },
            market.exchange,
        );
        assert_eq!(data.book.as_ref().unwrap().bids[0].price, "100");
        data.apply_book_delta(
            vec![BookChange {
                price: "100".into(),
                size: "2".into(),
            }],
            Vec::new(),
            2,
            market.exchange,
        );
        assert_eq!(
            data.book.as_ref().unwrap().bids[0].size,
            "2",
            "updates sharing a millisecond remain valid"
        );
    }

    #[test]
    fn hidden_disconnect_deadline_uses_a_running_clock_not_egui_frame_time() {
        let mut app = test_app();
        let (receiver, on_event) = socket::channel(|| {});
        app.receiver = Some(receiver);
        assert!(on_event(WsEvent::Closed).is_continue());
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .occluded = Some(true);
        let before = socket::now_ms();
        let _ = ctx.run_logic(&input, |ctx| {
            eframe::App::logic(&mut app, ctx, &mut eframe::Frame::_new_kittest())
        });
        assert!(app.receiver.is_none());
        assert!(app.reconnect_at >= before + 3_000);
        assert!(app.reconnect_at <= socket::now_ms() + 3_000);
    }

    #[test]
    fn hidden_overload_waits_for_visibility_instead_of_reconnecting_in_a_loop() {
        let mut app = test_app();
        let (receiver, on_event) = socket::channel(|| {});
        app.receiver = Some(receiver);
        assert!(
            on_event(WsEvent::Message(WsMessage::Binary(vec![
                0;
                17 * 1024 * 1024
            ])))
            .is_break()
        );
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .occluded = Some(true);
        let _ = ctx.run_logic(&input, |ctx| {
            eframe::App::logic(&mut app, ctx, &mut eframe::Frame::_new_kittest())
        });
        assert!(app.resync_on_visible);
        assert!(app.receiver.is_none());
        let _ = ctx.run_logic(&input, |ctx| {
            eframe::App::logic(&mut app, ctx, &mut eframe::Frame::_new_kittest())
        });
        assert!(app.resync_on_visible);
        assert!(app.receiver.is_none());
    }

    fn pane_ids(tree: &Tree<Pane>) -> Vec<TileId> {
        tree.active_tiles()
            .into_iter()
            .filter(|id| tree.tiles.get_pane(id).is_some())
            .collect()
    }

    pub(super) fn test_app() -> TerminalApp {
        TerminalApp {
            audio: audio::TradeAudio::new(audio::Settings::default()),
            private: private::Controller::default(),
            workspaces: (0..3).map(TerminalApp::default_tree).collect(),
            active_workspace: 0,
            tab_hover: None,
            batch_change: None,
            batch_undo: None,
            data: HashMap::new(),
            catalogs: HashMap::new(),
            catalog_errors: HashMap::new(),
            catalog_requests: HashSet::new(),
            subscribed: HashSet::new(),
            accounts: Vec::new(),
            hidden_widget_accounts: HashSet::new(),
            account_data: HashMap::new(),
            fills: fills::Store::default(),
            subscribed_accounts: HashSet::new(),
            account_ui: accounts::Menu::default(),
            account_address: String::new(),
            account_error: None,
            focused: None,
            socket_open: false,
            sender: None,
            receiver: None,
            reconnect_at: 0,
            resync_on_visible: false,
        }
    }

    #[test]
    fn compact_book_snapshot_rebuilds_exact_quote_and_cumulative_depth() {
        let level = |price: &str, size: &str| Level {
            price: price.into(),
            size: size.into(),
            quote_size: "unused".into(),
            depth_base: "unused".into(),
            depth_quote: "unused".into(),
        };
        let original = Book {
            bids: vec![level("100.1", "2"), level("99.9", "3")],
            asks: vec![level("100.2", "1")],
            updated_at_ms: 10,
        };
        let json = serde_json::to_string(&original).unwrap();
        assert!(!json.contains("depth_quote"));
        assert!(!json.contains("quote_size"));
        let mut data = MarketData::default();
        data.set_book(serde_json::from_str(&json).unwrap(), Exchange::Binance);
        let book = data.book.unwrap();
        assert_eq!(book.bids.len(), 2);
        assert_eq!(book.bids[0].quote_size, "200.2");
        assert_eq!(book.bids[1].depth_base, "5");
        assert_eq!(book.bids[1].depth_quote, "499.9");
        assert_eq!(book.asks[0].depth_quote, "100.2");
    }

    #[test]
    fn hidden_account_stays_connected_but_is_excluded_from_widget_data() {
        let market = Market::for_exchange(Exchange::Hyperliquid);
        let account = Account {
            exchange: Exchange::Hyperliquid,
            address: "0x0000000000000000000000000000000000000000".into(),
        };
        let mut states = HashMap::new();
        states.insert(
            account.clone(),
            AccountState {
                connected: true,
                ..Default::default()
            },
        );
        let mut hidden = HashSet::new();
        assert_eq!(visible_account_states(&market, &states, &hidden).count(), 1);
        hidden.insert(account.clone());
        assert_eq!(visible_account_states(&market, &states, &hidden).count(), 0);
        assert!(states.get(&account).unwrap().connected);
    }

    #[test]
    fn prices_orders_match_each_source_and_account_visibility() {
        let hyper = Market::for_exchange(Exchange::Hyperliquid);
        let other_symbol = Market {
            symbol: "ETH".into(),
            ..hyper.clone()
        };
        let binance = Market::for_exchange(Exchange::Binance);
        let account = Account {
            exchange: Exchange::Hyperliquid,
            address: "0x0000000000000000000000000000000000000000".into(),
        };
        let mut states = HashMap::new();
        states.insert(
            account.clone(),
            AccountState {
                connected: true,
                orders: vec![OwnOrder {
                    kind: None,
                    external_id: None,
                    coin: hyper.symbol.clone(),
                    order_id: 7,
                    side: TradeSide::Buy,
                    price: "100".into(),
                    size: "1".into(),
                }],
                ..Default::default()
            },
        );
        let sources = [binance, other_symbol, hyper];
        let hidden = HashSet::new();
        let orders = visible_source_orders(&sources, &states, &hidden);
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].0, 2);
        assert!(visible_source_orders(&sources, &states, &HashSet::from([account])).is_empty());
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
    fn chart_timeframes_persist_per_widget_and_old_layouts_default_to_one_minute() {
        let saved = r#"{"kind":"Chart","market":{"exchange":"binance","symbol":"BTCUSDT"}}"#;
        let mut pane: Pane = serde_json::from_str(saved).unwrap();
        assert_eq!(pane.chart_timeframe, candles::Timeframe::M1);
        pane.chart_timeframe = candles::Timeframe::S30;
        let restored: Pane = serde_json::from_str(&serde_json::to_string(&pane).unwrap()).unwrap();
        assert_eq!(restored.chart_timeframe, candles::Timeframe::S30);
        assert_eq!(
            Pane::new(WidgetKind::Chart, pane.market).chart_timeframe,
            candles::Timeframe::M1
        );
    }

    #[test]
    fn saved_comparison_defaults_to_live_trades() {
        let saved = r#"{"kind":"Compare","market":{"exchange":"binance","symbol":"BTCUSDT"},"series":[{"exchange":"binance","symbol":"BTCUSDT"}],"compare_percent":false}"#;
        let pane: Pane = serde_json::from_str(saved).unwrap();
        assert_eq!(pane.compare_mode, CompareMode::Trades);
        assert_eq!(pane.compare_window_secs, 60);
        assert!(pane.compare_show_orders);
        assert!(pane.compare_show_fills);
        assert!(!pane.orderflow.enabled);
        let mut pane = pane;
        pane.compare_show_orders = false;
        pane.compare_show_fills = false;
        pane.orderflow.enabled = true;
        pane.orderflow.source = Some(pane.series[0].clone());
        let restored: Pane = serde_json::from_str(&serde_json::to_string(&pane).unwrap()).unwrap();
        assert!(!restored.compare_show_orders);
        assert!(!restored.compare_show_fills);
        assert!(restored.orderflow.enabled);
        assert_eq!(restored.orderflow.source, Some(pane.series[0].clone()));
    }

    #[test]
    fn dom_sources_survive_save_and_old_layouts_keep_their_market() {
        let old = r#"{"kind":"Dom","market":{"exchange":"hyperliquid","kind":"perp","symbol":"kPEPE"},"dom":{"compact":true,"trade_stream":true}}"#;
        let mut pane: Pane = serde_json::from_str(old).unwrap();
        assert_eq!(pane.series, [pane.market.clone()]);
        assert!(pane.uses_sources());
        pane.series.push(Market {
            exchange: Exchange::Binance,
            kind: MarketKind::Perp,
            symbol: "1000PEPEUSDT".into(),
        });
        pane.editing_series = 1;
        pane.swap_sources(1, 0);
        assert_eq!(pane.editing_series, 0);
        assert_eq!(pane.market, pane.series[0]);
        assert_eq!(pane.market.exchange, Exchange::Binance);
        let restored: Pane = serde_json::from_str(&serde_json::to_string(&pane).unwrap()).unwrap();
        assert_eq!(restored.market, pane.market);
        assert_eq!(restored.series, pane.series);
    }

    #[test]
    fn reordering_sources_keeps_selection_search_and_flow_source_and_saves_order() {
        let mut pane = Pane::new(WidgetKind::Compare, Market::default());
        pane.series = [Exchange::Binance, Exchange::Bybit, Exchange::Okx]
            .into_iter()
            .map(Market::for_exchange)
            .collect();
        let selected = pane.series[1].clone();
        pane.editing_series = 1;
        pane.search = "PEPE".into();
        pane.orderflow.source = Some(selected.clone());
        pane.swap_sources(1, 0);
        assert_eq!(pane.editing_series, 0);
        pane.swap_sources(2, 1);
        assert_eq!(pane.editing_series, 0);
        pane.swap_sources(0, 1);
        assert_eq!(pane.editing_series, 1);
        assert_eq!(pane.series[pane.editing_series], selected);
        assert_eq!(pane.search, "PEPE");
        assert_eq!(pane.orderflow.source.as_ref(), Some(&selected));
        assert_eq!(
            pane.series
                .iter()
                .map(|market| market.exchange)
                .collect::<Vec<_>>(),
            [Exchange::Okx, Exchange::Bybit, Exchange::Binance]
        );
        let restored: Pane = serde_json::from_str(&serde_json::to_string(&pane).unwrap()).unwrap();
        assert_eq!(restored.series, pane.series);
        assert_eq!(restored.orderflow.source, pane.orderflow.source);
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
    fn orderflow_volume_survives_price_point_and_tape_limits() {
        let mut data = MarketData::default();
        for _ in 0..=MAX_TRADE_POINTS {
            data.push_trade(Trade {
                price: "100".into(),
                size: "0.5".into(),
                time_ms: 1_500,
                side: TradeSide::Buy,
            });
        }
        assert_eq!(data.price_trades.len(), MAX_TRADE_POINTS);
        assert_eq!(data.trades.len(), MAX_TAPE_TRADES);
        let bucket = data.orderflow.buckets.front().unwrap();
        assert_eq!(bucket.trades, MAX_TRADE_POINTS as u64 + 1);
        assert_eq!(bucket.buy.quote, (MAX_TRADE_POINTS + 1) as f64 * 50.0);
        assert_eq!(
            data.trade_candles.candles(candles::Timeframe::S1)[0].volume,
            (MAX_TRADE_POINTS + 1) as f64 * 0.5
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
