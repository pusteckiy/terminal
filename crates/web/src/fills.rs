use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use terminal_core::{Account, Exchange, Market, MarketKind, OwnFill, SymbolInfo, TradeSide};

use crate::{
    BORDER, GREEN, MUTED, MarketData, RED, SURFACE, TEXT, orderflow, utc_now_ms,
    view::quote_size_text,
};

const QUOTE_FRESH_MS: i64 = 2_000;
const SETTLE_MS: i64 = 250;
const MAX_QUOTES: usize = 25_000;

#[derive(Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
enum Units {
    #[default]
    Bps,
    Percent,
}

impl Units {
    fn label(self) -> &'static str {
        match self {
            Self::Bps => "bp",
            Self::Percent => "%",
        }
    }
    fn value(self, bp: f64) -> f64 {
        if self == Self::Percent {
            bp / 100.0
        } else {
            bp
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    accounts: Option<Vec<Account>>,
    symbols: Option<Vec<Market>>,
    horizons_ms: Vec<u32>,
    units: Units,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            accounts: None,
            symbols: None,
            horizons_ms: vec![1_000, 5_000, 30_000],
            units: Units::Bps,
        }
    }
}

impl Settings {
    fn horizons(&self) -> Vec<u32> {
        let mut values: Vec<_> = self
            .horizons_ms
            .iter()
            .copied()
            .filter(|value| (100..=600_000).contains(value))
            .collect();
        values.sort_unstable();
        values.dedup();
        values.truncate(8);
        values
    }

    fn accepts(&self, account: &Account, fill: &OwnFill, hidden: &HashSet<Account>) -> bool {
        !hidden.contains(account)
            && self
                .accounts
                .as_ref()
                .is_none_or(|accounts| accounts.contains(account))
            && self
                .symbols
                .as_ref()
                .is_none_or(|symbols| symbols.contains(&fill.market))
    }
}

#[derive(Default)]
pub struct Store {
    pub histories: HashMap<Account, Vec<OwnFill>>,
}

impl Store {
    pub fn push(&mut self, account: Account, fills: Vec<OwnFill>) {
        terminal_core::merge_account_fills(self.histories.entry(account).or_default(), fills);
    }

    fn rows<'a>(
        &'a self,
        settings: &Settings,
        hidden: &HashSet<Account>,
    ) -> Vec<(&'a Account, &'a OwnFill)> {
        let mut rows: Vec<_> = self
            .histories
            .iter()
            .flat_map(|(account, fills)| fills.iter().map(move |fill| (account, fill)))
            .filter(|(account, fill)| settings.accepts(account, fill, hidden))
            .collect();
        rows.sort_unstable_by(|(a, left), (b, right)| {
            right.key().cmp(&left.key()).then(a.address.cmp(&b.address))
        });
        rows
    }

    pub fn wanted_markets(
        &self,
        settings: &Settings,
        hidden: &HashSet<Account>,
        now_ms: i64,
    ) -> HashSet<Market> {
        if !settings.enabled {
            return HashSet::new();
        }
        let longest = settings.horizons().last().copied().unwrap_or(0) as i64;
        self.rows(settings, hidden)
            .into_iter()
            .filter(|(_, fill)| (0..=longest + 60_000).contains(&(now_ms - fill.time_ms)))
            .map(|(_, fill)| fill.market.clone())
            .collect()
    }
}

#[derive(Clone, Copy)]
struct Quote {
    time_ms: i64,
    received_ms: i64,
    mid: f64,
}

#[derive(Default)]
pub struct QuoteHistory {
    since_ms: Option<i64>,
    quotes: VecDeque<Quote>,
}

impl QuoteHistory {
    pub fn push(&mut self, time_ms: i64, bid: f64, ask: f64, received_ms: i64) {
        if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask < bid {
            return;
        }
        if self
            .quotes
            .back()
            .is_some_and(|last| last.time_ms > time_ms)
        {
            return;
        }
        self.since_ms.get_or_insert(received_ms);
        let quote = Quote {
            time_ms,
            received_ms,
            mid: (bid + ask) / 2.0,
        };
        if self
            .quotes
            .back()
            .is_some_and(|last| last.time_ms == time_ms)
        {
            self.quotes.pop_back();
        }
        self.quotes.push_back(quote);
        while self.quotes.len() > MAX_QUOTES
            || self
                .quotes
                .front()
                .is_some_and(|first| time_ms - first.time_ms > 602_000)
        {
            self.quotes.pop_front();
        }
    }

    fn at(&self, deadline: i64) -> Option<Quote> {
        if self.since_ms.is_none_or(|since| since > deadline) {
            return None;
        }
        let index = self
            .quotes
            .partition_point(|quote| quote.time_ms <= deadline);
        let quote = *self.quotes.get(index.checked_sub(1)?)?;
        ((0..=QUOTE_FRESH_MS).contains(&(deadline - quote.time_ms))
            && quote.received_ms <= deadline + SETTLE_MS)
            .then_some(quote)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Outcome {
    Pending,
    Missing,
    Ready { bp: f64, mid: f64, age_ms: i64 },
}

fn evaluate(fill: &OwnFill, horizon: u32, data: Option<&MarketData>, now: i64) -> Outcome {
    let deadline = fill.time_ms.saturating_add(i64::from(horizon));
    if now < deadline + SETTLE_MS {
        return Outcome::Pending;
    }
    let Some(quote) = data
        .filter(|data| data.connected)
        .and_then(|data| data.fill_quotes.at(deadline))
    else {
        return Outcome::Missing;
    };
    let Ok(price) = fill.price.parse::<f64>() else {
        return Outcome::Missing;
    };
    if !price.is_finite() || price <= 0.0 {
        return Outcome::Missing;
    }
    let sign = match fill.side {
        TradeSide::Buy => 1.0,
        TradeSide::Sell => -1.0,
        TradeSide::Unknown => return Outcome::Missing,
    };
    Outcome::Ready {
        bp: sign * (quote.mid - price) / price * 10_000.0,
        mid: quote.mid,
        age_ms: deadline - quote.time_ms,
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct Selection {
    account: Account,
    market: Market,
    time_ms: i64,
    trade_id: u64,
    order_id: u64,
}

impl Selection {
    fn new(account: &Account, fill: &OwnFill) -> Self {
        Self {
            account: account.clone(),
            market: fill.market.clone(),
            time_ms: fill.time_ms,
            trade_id: fill.trade_id,
            order_id: fill.order_id,
        }
    }
}

#[derive(Default)]
pub struct View {
    results: HashMap<Selection, BTreeMap<u32, Outcome>>,
    selected: Option<Selection>,
    pub search: String,
}

impl View {
    fn resolve(
        &mut self,
        key: Selection,
        fill: &OwnFill,
        horizons: &[u32],
        data: Option<&MarketData>,
        now: i64,
    ) {
        let results = self.results.entry(key).or_default();
        if results.len() > 16 {
            results.retain(|horizon, _| horizons.contains(horizon));
        }
        for &horizon in horizons {
            if !results.contains_key(&horizon) {
                let outcome = evaluate(fill, horizon, data, now);
                if outcome != Outcome::Pending {
                    results.insert(horizon, outcome);
                }
            }
        }
    }
}

fn duration(ms: u32) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else {
        format!("{}s", ms as f64 / 1_000.0)
    }
}

fn market_label(
    market: &Market,
    catalogs: &HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
) -> String {
    let symbol = orderflow::symbol_info(market, catalogs).map_or_else(
        || market.symbol.clone(),
        |info| {
            if market.kind == MarketKind::Spot {
                format!("{}/{}", info.base, info.quote)
            } else {
                market.symbol.clone()
            }
        },
    );
    format!("{} {}", symbol, market.kind.label())
}

pub fn config_ui(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    view: &mut View,
    accounts: &[Account],
    hidden: &HashSet<Account>,
    store: &Store,
    catalogs: &HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
) {
    ui.checkbox(&mut settings.enabled, "Show markout");
    if settings.enabled {
        ui.horizontal(|ui| {
            ui.label("Units");
            ui.selectable_value(&mut settings.units, Units::Bps, "bp");
            ui.selectable_value(&mut settings.units, Units::Percent, "%");
        });
        ui.label(egui::RichText::new("HORIZONS").small().color(MUTED));
        let mut remove = None;
        for (index, horizon) in settings.horizons_ms.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                let mut seconds = *horizon as f64 / 1_000.0;
                if ui
                    .add(
                        egui::DragValue::new(&mut seconds)
                            .range(0.1..=600.0)
                            .speed(0.1)
                            .suffix(" s")
                            .max_decimals(3),
                    )
                    .changed()
                {
                    *horizon = (seconds * 1_000.0).round() as u32;
                }
                if ui
                    .small_button("×")
                    .on_hover_text("Remove horizon")
                    .clicked()
                {
                    remove = Some(index);
                }
            });
        }
        if let Some(index) = remove {
            settings.horizons_ms.remove(index);
        }
        if ui
            .add_enabled(
                settings.horizons_ms.len() < 8,
                egui::Button::new("+ Horizon"),
            )
            .clicked()
        {
            let next = [
                1_000, 5_000, 30_000, 100, 250, 500, 10_000, 60_000, 300_000, 600_000,
            ]
            .into_iter()
            .find(|ms| !settings.horizons_ms.contains(ms))
            .unwrap_or(1_000);
            settings.horizons_ms.push(next);
        }
        ui.weak("Mid-price on the fill venue. Gross of fees.").on_hover_text("1 bp = 0.01%. Requires a quote no older than 2 seconds at the horizon. Up to 250 ms is allowed for feed delivery. Historical quotes are not fetched.");
    }
    ui.separator();
    ui.label(egui::RichText::new("ACCOUNTS").small().color(MUTED));
    let mut all = settings.accounts.is_none();
    if ui.checkbox(&mut all, "All visible accounts").changed() {
        settings.accounts = if all { None } else { Some(Vec::new()) };
    }
    if accounts.is_empty() {
        ui.weak("Connect an account from the account menu.");
    }
    for account in accounts {
        let mut selected = settings
            .accounts
            .as_ref()
            .is_none_or(|list| list.contains(account));
        let suffix = &account.address[account.address.len().saturating_sub(4)..];
        let response = ui
            .add_enabled(
                !hidden.contains(account),
                egui::Checkbox::new(
                    &mut selected,
                    format!("{} · …{}", account.exchange.label(), suffix),
                ),
            )
            .on_hover_text(&account.address);
        if response.changed() {
            let list = settings.accounts.get_or_insert_with(|| {
                accounts
                    .iter()
                    .filter(|account| !hidden.contains(*account))
                    .cloned()
                    .collect()
            });
            list.retain(|saved| saved != account);
            if selected {
                list.push(account.clone());
            }
        }
    }
    ui.separator();
    ui.label(egui::RichText::new("SYMBOLS").small().color(MUTED));
    let mut all = settings.symbols.is_none();
    if ui.checkbox(&mut all, "All symbols").changed() {
        settings.symbols = if all { None } else { Some(Vec::new()) };
    }
    if let Some(selected) = &mut settings.symbols {
        let mut remove = None;
        for (index, market) in selected.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(market_label(market, catalogs));
                if ui.small_button("×").clicked() {
                    remove = Some(index);
                }
            });
        }
        if let Some(index) = remove {
            selected.remove(index);
        }
        ui.add(
            egui::TextEdit::singleline(&mut view.search)
                .hint_text("Search symbol")
                .desired_width(240.0),
        );
        let query = view.search.trim().to_ascii_uppercase();
        let mut markets: HashSet<Market> = store
            .histories
            .values()
            .flatten()
            .map(|fill| fill.market.clone())
            .collect();
        for (&(exchange, kind), symbols) in catalogs {
            if exchange == Exchange::Hyperliquid {
                markets.extend(symbols.iter().map(|info| Market {
                    exchange,
                    kind,
                    symbol: info.symbol.clone(),
                }));
            }
        }
        let mut matches: Vec<_> = markets
            .into_iter()
            .filter(|market| {
                market_label(market, catalogs)
                    .to_ascii_uppercase()
                    .contains(&query)
            })
            .collect();
        matches.sort_by_key(|market| market_label(market, catalogs));
        egui::ScrollArea::vertical()
            .id_salt("fill_symbols")
            .max_height(120.0)
            .show_rows(ui, 22.0, matches.len(), |ui, range| {
                for index in range {
                    let market = &matches[index];
                    if ui
                        .selectable_label(selected.contains(market), market_label(market, catalogs))
                        .clicked()
                    {
                        if selected.contains(market) {
                            selected.retain(|saved| saved != market);
                        } else {
                            selected.push(market.clone());
                        }
                    }
                }
            });
    }
}

fn outcome_text(outcome: Outcome, units: Units) -> (String, Color32) {
    match outcome {
        Outcome::Pending => ("…".into(), MUTED),
        Outcome::Missing => ("—".into(), MUTED),
        Outcome::Ready { bp, .. } => (
            format!(
                "{:+.*}",
                if units == Units::Bps { 2 } else { 4 },
                units.value(bp)
            ),
            if bp > 0.0 {
                GREEN
            } else if bp < 0.0 {
                RED
            } else {
                MUTED
            },
        ),
    }
}

pub(super) fn utc(ms: i64) -> String {
    let seconds = ms.div_euclid(1_000).rem_euclid(86_400);
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60,
        ms.rem_euclid(1_000)
    )
}

pub fn ui(
    ui: &mut egui::Ui,
    settings: &Settings,
    view: &mut View,
    store: &Store,
    accounts: &[Account],
    hidden: &HashSet<Account>,
    data: &HashMap<Market, MarketData>,
    catalogs: &HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
) {
    let rows = store.rows(settings, hidden);
    if rows.is_empty() {
        let text = if accounts.is_empty() {
            "Connect an account to see your fills"
        } else if accounts.iter().all(|account| hidden.contains(account)) {
            "Accounts are hidden"
        } else {
            "No fills for the selected accounts and symbols"
        };
        ui.centered_and_justified(|ui| {
            ui.weak(text);
        });
        return;
    }
    let now = utc_now_ms();
    let horizons = if settings.enabled {
        settings.horizons()
    } else {
        Vec::new()
    };
    let mut present = HashSet::new();
    for (account, fill) in &rows {
        let key = Selection::new(account, fill);
        present.insert(key.clone());
        if !horizons.is_empty() {
            view.resolve(key, fill, &horizons, data.get(&fill.market), now);
        }
    }
    if view.results.len() > terminal_core::MAX_ACCOUNT_FILLS * store.histories.len() {
        let retained: HashSet<_> = store
            .histories
            .iter()
            .flat_map(|(account, fills)| {
                fills.iter().map(move |fill| Selection::new(account, fill))
            })
            .collect();
        view.results.retain(|key, _| retained.contains(key));
    }
    if view
        .selected
        .as_ref()
        .is_some_and(|selected| !present.contains(selected))
    {
        view.selected = None;
    }
    let graph_height = if settings.enabled && view.selected.is_some() {
        (ui.available_height() * 0.35)
            .min(145.0)
            .min((ui.available_height() - 85.0).max(0.0))
    } else {
        0.0
    };
    let table_height =
        (ui.available_height() - graph_height - if graph_height > 0.0 { 6.0 } else { 0.0 })
            .max(40.0);
    let mut widths = vec![89.0, 82.0, 49.0, 74.0, 65.0];
    widths.extend(horizons.iter().map(|_| 59.0));
    let minimum_width: f32 = widths.iter().sum();
    let available_width = ui.available_width() - ui.spacing().scroll.allocated_width();
    let extra = (available_width - minimum_width).max(0.0);
    // Fill the tile while keeping compact, readable columns in narrow layouts.
    // Market names get more of the spare space than the fixed-format fields.
    let mut weights = vec![0.5, 2.0, 0.5, 1.0, 1.0];
    weights.extend(horizons.iter().map(|_| 1.0));
    let total_weight: f32 = weights.iter().sum();
    for (width, weight) in widths.iter_mut().zip(weights) {
        *width += extra * weight / total_weight;
    }
    let width: f32 = widths.iter().sum();
    let mut offsets = vec![0.0];
    for w in &widths {
        offsets.push(offsets.last().unwrap() + w);
    }
    let font = FontId::monospace(10.0);
    egui::ScrollArea::horizontal().id_salt("fills_horizontal").auto_shrink([false, false]).max_height(table_height).show(ui, |ui| {
        ui.set_min_width(width);
        let (header, _) = ui.allocate_exact_size(Vec2::new(width, 23.0), Sense::hover());
        let mut labels = vec!["UTC".into(), "MARKET".into(), "SIDE".into(), "PRICE".into(), "SIZE".into()];
        labels.extend(horizons.iter().map(|horizon| format!("{} {}", duration(*horizon), settings.units.label())));
        for (index, label) in labels.iter().enumerate() {
            let x = header.left() + if index < 3 { offsets[index] + 5.0 } else { offsets[index + 1] - 7.0 };
            ui.painter().text(Pos2::new(x, header.center().y), if index < 3 { Align2::LEFT_CENTER } else { Align2::RIGHT_CENTER }, label, FontId::monospace(9.0), MUTED);
        }
        ui.painter().line_segment([header.left_bottom(), header.right_bottom()], Stroke::new(1.0, BORDER));
        egui::ScrollArea::vertical().id_salt("fills_vertical").auto_shrink([false, false]).max_height((table_height - 33.0).max(22.0)).show_rows(ui, 23.0, rows.len(), |ui, range| {
            for index in range {
                let (account, fill) = rows[index];
                let key = Selection::new(account, fill);
                let selected = view.selected.as_ref() == Some(&key);
                let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 23.0), Sense::click());
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 0.0, if selected || response.hovered() { crate::HOVER } else if index % 2 == 0 { SURFACE } else { crate::BG });
                let color = if fill.side == TradeSide::Buy { GREEN } else { RED };
                let mut values = vec![(utc(fill.time_ms), MUTED), (market_label(&fill.market, catalogs), TEXT), (format!("{} {}", if fill.side == TradeSide::Buy { "BUY" } else { "SELL" }, if fill.taker { "T" } else { "M" }), color), (fill.price.clone(), color), (quote_size_text(&fill.size), color)];
                values.extend(horizons.iter().map(|horizon| outcome_text(view.results.get(&key).and_then(|results| results.get(horizon)).copied().unwrap_or(Outcome::Pending), settings.units)));
                for (col, (text, color)) in values.iter().enumerate() {
                    let cell = Rect::from_min_max(Pos2::new(rect.left() + offsets[col], rect.top()), Pos2::new(rect.left() + offsets[col + 1], rect.bottom()));
                    let x = if col < 3 { cell.left() + 5.0 } else { cell.right() - 7.0 };
                    painter.with_clip_rect(cell.intersect(ui.clip_rect())).text(Pos2::new(x, rect.center().y), if col < 3 { Align2::LEFT_CENTER } else { Align2::RIGHT_CENTER }, text, font.clone(), *color);
                }
                if settings.enabled && response.clicked() { view.selected = if selected { None } else { Some(key.clone()) }; }
                response.on_hover_ui_at_pointer(|ui| {
                    ui.label(format!("{} · {}", fill.market.exchange.label(), market_label(&fill.market, catalogs)));
                    ui.monospace(format!("{} UTC · {}", utc(fill.time_ms), if fill.taker { "Taker" } else { "Maker" }));
                    ui.monospace(&account.address);
                    let quote = fill.price.parse::<rust_decimal::Decimal>().ok().zip(fill.size.parse::<rust_decimal::Decimal>().ok()).and_then(|(price, size)| price.checked_mul(size)).map(|value| value.normalize().to_string()).unwrap_or_else(|| "—".into());
                    let info = orderflow::symbol_info(&fill.market, catalogs);
                    let base = info.map_or("base", |info| info.base.as_str());
                    let quote_unit = info.map_or("quote", |info| info.quote.as_str());
                    ui.label(format!("Price {} · size {} {} · {} {}", fill.price, fill.size, base, quote, quote_unit));
                    ui.label(format!("Fee {} {} · order #{} · fill #{}", fill.fee, fill.fee_token, fill.order_id, fill.trade_id));
                    for &horizon in &horizons {
                        match view.results.get(&key).and_then(|results| results.get(&horizon)).copied().unwrap_or(Outcome::Pending) {
                            Outcome::Ready { bp, mid, age_ms } => {
                                let (_, color) = outcome_text(Outcome::Ready { bp, mid, age_ms }, settings.units);
                                ui.colored_label(color, format!("{}: {bp:+.2} bp · {:+.4}% · mid {mid} · quote age {age_ms} ms", duration(horizon), bp / 100.0));
                            }
                            Outcome::Pending => { ui.weak(format!("{}: waiting for horizon", duration(horizon))); }
                            Outcome::Missing => { ui.weak(format!("{}: no fresh observed quote at this horizon", duration(horizon))); }
                        }
                    }
                    if settings.enabled { ui.weak("Click to show or hide the markout graph."); }
                });
            }
        });
    });
    if graph_height > 0.0
        && let Some(selected) = &view.selected
    {
        ui.add_space(4.0);
        graph(
            ui,
            selected,
            view.results.get(selected),
            &horizons,
            settings.units,
            graph_height,
        );
    }
    if settings.enabled {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(50));
    }
}

fn graph(
    ui: &mut egui::Ui,
    selection: &Selection,
    results: Option<&BTreeMap<u32, Outcome>>,
    horizons: &[u32],
    units: Units,
    height: f32,
) {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, SURFACE);
    painter.text(
        rect.left_top() + Vec2::new(8.0, 8.0),
        Align2::LEFT_TOP,
        format!(
            "MARKOUT · {} · {} UTC",
            selection.market.symbol,
            utc(selection.time_ms)
        ),
        FontId::monospace(9.0),
        MUTED,
    );
    let plot = Rect::from_min_max(
        rect.left_top() + Vec2::new(10.0, 28.0),
        rect.right_bottom() - Vec2::new(55.0, 20.0),
    );
    if plot.width() < 50.0 || plot.height() < 15.0 {
        return;
    }
    let values: Vec<_> = horizons
        .iter()
        .map(|horizon| {
            (
                *horizon,
                results
                    .and_then(|results| results.get(horizon))
                    .copied()
                    .unwrap_or(Outcome::Pending),
            )
        })
        .collect();
    let bound = values
        .iter()
        .filter_map(|(_, outcome)| {
            if let Outcome::Ready { bp, .. } = outcome {
                Some(units.value(*bp).abs())
            } else {
                None
            }
        })
        .fold(if units == Units::Bps { 1.0_f64 } else { 0.01 }, f64::max)
        * 1.2;
    let max_time = horizons.last().copied().unwrap_or(1) as f32;
    let pos = |horizon: u32, bp: f64| {
        Pos2::new(
            plot.left() + horizon as f32 / max_time * plot.width(),
            plot.center().y - (units.value(bp) / bound) as f32 * plot.height() / 2.0,
        )
    };
    let precision = if units == Units::Percent { 4 } else { 2 };
    for (y, label) in [
        (plot.top(), format!("+{bound:.precision$}")),
        (plot.center().y, "0".into()),
        (plot.bottom(), format!("−{bound:.precision$}")),
    ] {
        painter.line_segment(
            [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            Stroke::new(0.5, BORDER),
        );
        painter.text(
            Pos2::new(rect.right() - 5.0, y),
            Align2::RIGHT_CENTER,
            format!("{label} {}", units.label()),
            FontId::monospace(8.0),
            MUTED,
        );
    }
    let mut previous = None;
    let mut last_label_x = f32::NEG_INFINITY;
    for &(horizon, outcome) in &values {
        let x = pos(horizon, 0.0).x;
        if x - last_label_x >= 32.0 {
            painter.text(
                Pos2::new(x, plot.bottom() + 10.0),
                Align2::CENTER_CENTER,
                duration(horizon),
                FontId::monospace(8.0),
                MUTED,
            );
            last_label_x = x;
        }
        if let Outcome::Ready { bp, .. } = outcome {
            let point = pos(horizon, bp);
            if let Some(previous) = previous {
                painter.line_segment([previous, point], Stroke::new(1.3, TEXT));
            }
            painter.circle_filled(point, 3.0, if bp >= 0.0 { GREEN } else { RED });
            previous = Some(point);
        } else {
            previous = None;
        }
    }
    if values
        .iter()
        .all(|(_, outcome)| !matches!(outcome, Outcome::Ready { .. }))
    {
        painter.text(
            plot.center(),
            Align2::CENTER_CENTER,
            "No measured markout yet",
            FontId::monospace(10.0),
            MUTED,
        );
    }
    response.on_hover_ui_at_pointer(|ui| {
        ui.weak("Measured horizons; connecting lines are visual guides. Gross of fees.");
        for (horizon, outcome) in values {
            let (text, color) = outcome_text(outcome, units);
            ui.colored_label(
                color,
                format!("{} · {text} {}", duration(horizon), units.label()),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(side: TradeSide) -> OwnFill {
        OwnFill {
            market: Market::for_exchange(Exchange::Hyperliquid),
            time_ms: 10_000,
            trade_id: 1,
            order_id: 2,
            side,
            price: "100".into(),
            size: "0.2".into(),
            start_position: None,
            taker: false,
            fee: "-0.001".into(),
            fee_token: "USDC".into(),
        }
    }

    fn data() -> MarketData {
        let mut data = MarketData {
            connected: true,
            ..Default::default()
        };
        data.fill_quotes.push(10_500, 100.01, 100.03, 10_600);
        data.fill_quotes.push(11_100, 101.0, 101.02, 11_150);
        data
    }

    #[test]
    fn completed_markout_stays_fixed_across_new_quotes_and_visibility_toggles() {
        let account = Account {
            exchange: Exchange::Hyperliquid,
            address: "0x1234".into(),
        };
        let fill = fill(TradeSide::Buy);
        let key = Selection::new(&account, &fill);
        let mut view = View::default();
        let mut data = data();
        view.resolve(key.clone(), &fill, &[1_000], Some(&data), 11_300);
        let original = view.results[&key][&1_000];
        data.fill_quotes.push(20_000, 110.0, 111.0, 20_000);
        view.resolve(key.clone(), &fill, &[], None, 21_000);
        view.resolve(key.clone(), &fill, &[1_000], Some(&data), 22_000);
        assert_eq!(view.results[&key][&1_000], original);
    }

    #[test]
    fn fills_configuration_survives_layout_save() {
        let mut pane = crate::Pane::new(crate::WidgetKind::Fills, Market::default());
        pane.fills.enabled = true;
        pane.fills.horizons_ms = vec![100, 2_500, 600_000];
        pane.fills.symbols = Some(vec![Market::for_exchange(Exchange::Hyperliquid)]);
        pane.fills.units = Units::Percent;
        let saved = serde_json::to_string(&pane).unwrap();
        let loaded: crate::Pane = serde_json::from_str(&saved).unwrap();
        assert!(matches!(loaded.kind, crate::WidgetKind::Fills));
        assert_eq!(loaded.fills.horizons(), vec![100, 2_500, 600_000]);
        assert_eq!(loaded.fills.units.label(), "%");
        assert_eq!(loaded.fills.symbols, pane.fills.symbols);
    }

    #[test]
    fn markout_uses_side_and_quote_before_exact_horizon() {
        let data = data();
        match evaluate(&fill(TradeSide::Buy), 1_000, Some(&data), 11_300) {
            Outcome::Ready { bp, age_ms, .. } => {
                assert!((bp - 2.0).abs() < 1e-8);
                assert_eq!(age_ms, 500);
            }
            result => panic!("{result:?}"),
        }
        match evaluate(&fill(TradeSide::Sell), 1_000, Some(&data), 11_300) {
            Outcome::Ready { bp, .. } => assert!((bp + 2.0).abs() < 1e-8),
            result => panic!("{result:?}"),
        }
        assert_eq!(
            evaluate(&fill(TradeSide::Buy), 1_000, Some(&data), 11_100),
            Outcome::Pending
        );
    }

    #[test]
    fn missing_stale_reconnected_and_historical_quotes_are_not_fabricated() {
        let mut data = data();
        assert_eq!(
            evaluate(&fill(TradeSide::Buy), 5_000, Some(&data), 15_300),
            Outcome::Missing
        );
        data.connected = false;
        assert_eq!(
            evaluate(&fill(TradeSide::Buy), 1_000, Some(&data), 11_300),
            Outcome::Missing
        );
        data.connected = true;
        data.fill_quotes = QuoteHistory::default();
        data.fill_quotes.push(10_500, 100.01, 100.03, 12_000);
        assert_eq!(
            evaluate(&fill(TradeSide::Buy), 1_000, Some(&data), 12_000),
            Outcome::Missing
        );
    }

    #[test]
    fn quote_history_preserves_subsecond_deadline_and_is_bounded() {
        let mut history = QuoteHistory::default();
        history.push(10_010, 100.0, 100.02, 10_010);
        history.push(10_040, 101.0, 101.02, 10_040);
        assert!((history.at(10_025).unwrap().mid - 100.01).abs() < 1e-8);
        for time in 20_000..50_000 {
            history.push(time, 100.0, 100.02, time);
        }
        assert_eq!(history.quotes.len(), MAX_QUOTES);
        assert!(history.at(10_025).is_none());
    }

    #[test]
    fn filters_respect_hidden_accounts_and_snapshot_replay_is_deduplicated() {
        let account = Account {
            exchange: Exchange::Hyperliquid,
            address: "0x1234".into(),
        };
        let mut store = Store::default();
        store.push(account.clone(), vec![fill(TradeSide::Buy)]);
        store.push(account.clone(), vec![fill(TradeSide::Buy)]);
        let mut settings = Settings::default();
        let hidden = HashSet::new();
        assert_eq!(store.rows(&settings, &hidden).len(), 1);
        assert!(
            store
                .rows(&settings, &HashSet::from([account.clone()]))
                .is_empty()
        );
        settings.accounts = Some(vec![]);
        assert!(store.rows(&settings, &hidden).is_empty());
        settings.accounts = None;
        settings.symbols = Some(vec![Market::default()]);
        assert!(store.rows(&settings, &hidden).is_empty());
        settings.symbols = None;
        assert!(store.wanted_markets(&settings, &hidden, 10_100).is_empty());
        settings.enabled = true;
        assert!(
            store
                .wanted_markets(&settings, &hidden, 10_100)
                .contains(&fill(TradeSide::Buy).market)
        );
    }
}
