use std::collections::BTreeMap;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use terminal_core::{Account, Book, OwnOrder, SymbolInfo, Trade, TradeSide};

use crate::{BORDER, GREEN, MUTED, MarketData, RED, SURFACE, TEXT, view::quote_size_text};

const ROW_HEIGHT: f32 = 22.0;
const HISTORY_SECONDS: i64 = 600;
const MAX_PRICE_BUCKETS: usize = 200_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum Grouping {
    #[default]
    Auto,
    Tick,
    Ten,
    Hundred,
    Thousand,
}

impl Grouping {
    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Tick => "1 tick",
            Self::Ten => "10 ticks",
            Self::Hundred => "100 ticks",
            Self::Thousand => "1,000 ticks",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
enum Units {
    #[default]
    Base,
    Quote,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    grouping: Grouping,
    window_secs: u32,
    units: Units,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            grouping: Grouping::Auto,
            window_secs: 60,
            units: Units::Base,
        }
    }
}

pub fn config_ui(ui: &mut egui::Ui, settings: &mut Settings) {
    ui.horizontal(|ui| {
        ui.label("Price grouping");
        // ComboBox opens a second root popup and replaces the enclosing config
        // menu in egui's popup memory. A submenu keeps both menus alive.
        egui::menu::SubMenuButton::new(settings.grouping.label())
            .config(egui::menu::MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside))
            .ui(ui, |ui| {
                for grouping in [Grouping::Auto, Grouping::Tick, Grouping::Ten, Grouping::Hundred, Grouping::Thousand] {
                    ui.selectable_value(&mut settings.grouping, grouping, grouping.label());
                }
            })
            .0.on_hover_text("Auto chooses a readable price step. Tick options use the exchange's minimum price increment.");
    });
    ui.horizontal(|ui| {
        ui.label("Trade window");
        ui.add(
            egui::DragValue::new(&mut settings.window_secs)
                .range(1..=600)
                .suffix(" s"),
        );
    });
    ui.horizontal(|ui| {
        ui.label("Size");
        ui.selectable_value(&mut settings.units, Units::Base, "Base");
        ui.selectable_value(&mut settings.units, Units::Quote, "Quote");
    });
}

#[derive(Clone, Copy, Debug, Default)]
struct Volume {
    base: f64,
    quote: f64,
}

impl Volume {
    fn add(&mut self, other: Self) {
        self.base += other.base;
        self.quote += other.quote;
    }

    fn value(self, units: Units) -> f64 {
        match units {
            Units::Base => self.base,
            Units::Quote => self.quote,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Flow {
    buy: Volume,
    sell: Volume,
}

// Aggregate each execution once, before the chart's raw-point limit. Native
// decimal prices preserve exact grouping when a widget changes its display step.
#[derive(Default)]
pub struct History {
    seconds: BTreeMap<i64, BTreeMap<Decimal, Flow>>,
    entries: usize,
    latest_buy: Option<(Decimal, i64)>,
    latest_sell: Option<(Decimal, i64)>,
}

fn positive(text: &str) -> Option<Decimal> {
    text.parse::<Decimal>()
        .ok()
        .filter(|value| *value > Decimal::ZERO)
}

fn volume(price: Decimal, size: &str) -> Option<Volume> {
    let base = positive(size)?;
    Some(Volume {
        base: base.to_f64()?,
        quote: price.checked_mul(base)?.to_f64()?,
    })
}

impl History {
    pub fn push(&mut self, trade: &Trade) {
        if trade.side == TradeSide::Unknown {
            return;
        }
        let Some(price) = positive(&trade.price) else {
            return;
        };
        let Some(size) = volume(price, &trade.size) else {
            return;
        };
        let second = trade.time_ms.div_euclid(1_000);
        let latest = self
            .seconds
            .last_key_value()
            .map_or(second, |(&time, _)| time.max(second));
        let cutoff = latest - HISTORY_SECONDS + 1;
        while self
            .seconds
            .first_key_value()
            .is_some_and(|(&time, _)| time < cutoff)
        {
            self.remove_oldest();
        }
        if second < cutoff {
            return;
        }
        let bucket = self.seconds.entry(second).or_default();
        if !bucket.contains_key(&price) {
            self.entries += 1;
        }
        let flow = bucket.entry(price).or_default();
        let (total, recent) = match trade.side {
            TradeSide::Buy => (&mut flow.buy, &mut self.latest_buy),
            TradeSide::Sell => (&mut flow.sell, &mut self.latest_sell),
            TradeSide::Unknown => unreachable!(),
        };
        total.add(size);
        if recent.is_none_or(|(_, time)| time <= trade.time_ms) {
            *recent = Some((price, trade.time_ms));
        }
        while self.entries > MAX_PRICE_BUCKETS {
            self.remove_oldest();
        }
    }

    fn remove_oldest(&mut self) {
        if let Some((_, prices)) = self.seconds.pop_first() {
            self.entries -= prices.len();
        }
    }

    fn collect(
        &self,
        now_ms: i64,
        window_secs: u32,
        step: Decimal,
        low: Decimal,
        high: Decimal,
    ) -> BTreeMap<Decimal, Flow> {
        let now = now_ms.div_euclid(1_000);
        let first = now - i64::from(window_secs.clamp(1, 600)) + 1;
        let mut result: BTreeMap<Decimal, Flow> = BTreeMap::new();
        for (_, prices) in self.seconds.range(first..=now) {
            for (&price, flow) in prices.range(low..high) {
                let row = result.entry(group(price, step)).or_default();
                row.buy.add(flow.buy);
                row.sell.add(flow.sell);
            }
        }
        result
    }
}

fn group(price: Decimal, step: Decimal) -> Decimal {
    (price / step).floor() * step
}

fn native_step(book: &Book, info: Option<&SymbolInfo>) -> Decimal {
    if let Some(step) = info
        .and_then(|info| info.price_step.as_deref())
        .and_then(positive)
    {
        return step;
    }
    // Some venues omit tick metadata. Infer the precision actually published in
    // the book rather than inventing a finer ladder than its available data.
    let scale = book
        .bids
        .iter()
        .chain(&book.asks)
        .filter_map(|level| positive(&level.price))
        .map(|price| price.normalize().scale())
        .max()
        .unwrap_or(0);
    Decimal::new(1, scale)
}

fn display_step(
    book: &Book,
    info: Option<&SymbolInfo>,
    settings: &Settings,
    reference: Decimal,
) -> Decimal {
    let tick = native_step(book, info);
    let multiplier = match settings.grouping {
        Grouping::Tick => 1,
        Grouping::Ten => 10,
        Grouping::Hundred => 100,
        Grouping::Thousand => 1_000,
        Grouping::Auto => {
            // Roughly one displayed step per 0.001%: BTC gets a $1 ladder,
            // while lower-price instruments still respect their native tick.
            let target = (reference / Decimal::from(100_000)).to_f64().unwrap_or(1.0);
            if target <= 0.0 {
                return tick;
            }
            let power = 10_f64.powf(target.log10().floor());
            let ratio = target / power;
            let nice = if ratio <= 1.0 {
                1.0
            } else if ratio <= 2.0 {
                2.0
            } else if ratio <= 5.0 {
                5.0
            } else {
                10.0
            } * power;
            let desired = Decimal::from_f64_retain(nice).unwrap_or(tick);
            // Avoid floating round-off turning 100 exact ticks into 101.
            return (desired.round_dp(12) / tick).ceil().max(Decimal::ONE) * tick;
        }
    };
    tick * Decimal::from(multiplier)
}

#[derive(Default)]
pub struct View {
    anchor: Option<Decimal>,
    step: Option<Decimal>,
    following: bool,
    scroll_remainder: f32,
}

#[derive(Clone, Copy, Default)]
struct Row {
    bid: Volume,
    ask: Volume,
    own: Flow,
}

fn book_rows(
    book: &Book,
    orders: &[(&Account, &OwnOrder)],
    step: Decimal,
    low: Decimal,
    high: Decimal,
) -> BTreeMap<Decimal, Row> {
    let mut rows: BTreeMap<Decimal, Row> = BTreeMap::new();
    for (levels, is_bid) in [(&book.bids, true), (&book.asks, false)] {
        for level in levels {
            let Some(price) = positive(&level.price).filter(|price| *price >= low && *price < high)
            else {
                continue;
            };
            let Some(size) = volume(price, &level.size) else {
                continue;
            };
            let row = rows.entry(group(price, step)).or_default();
            if is_bid {
                row.bid.add(size);
            } else {
                row.ask.add(size);
            }
        }
    }
    for (_, order) in orders {
        let Some(price) = positive(&order.price).filter(|price| *price >= low && *price < high)
        else {
            continue;
        };
        let Some(size) = volume(price, &order.size) else {
            continue;
        };
        let row = rows.entry(group(price, step)).or_default();
        match order.side {
            TradeSide::Buy => row.own.buy.add(size),
            TradeSide::Sell => row.own.sell.add(size),
            TradeSide::Unknown => {}
        }
    }
    rows
}

fn amount(value: f64) -> String {
    quote_size_text(&value.to_string())
}

fn anchor_bounds(
    book: &Book,
    history: &History,
    orders: &[(&Account, &OwnOrder)],
    now_ms: i64,
    window: u32,
    step: Decimal,
    reference: Decimal,
) -> (Decimal, Decimal) {
    let mut low = reference;
    let mut high = reference;
    let mut include = |price: Decimal| {
        low = low.min(price);
        high = high.max(price);
    };
    for level in book.bids.iter().chain(&book.asks) {
        if let Some(price) = positive(&level.price) {
            include(price);
        }
    }
    for (_, order) in orders {
        if let Some(price) = positive(&order.price) {
            include(price);
        }
    }
    let now = now_ms.div_euclid(1_000);
    for (_, prices) in history.seconds.range((now - i64::from(window) + 1)..=now) {
        if let Some((&price, _)) = prices.first_key_value() {
            include(price);
        }
        if let Some((&price, _)) = prices.last_key_value() {
            include(price);
        }
    }
    (group(low, step).max(step), group(high, step).max(step))
}

pub fn ui(
    ui: &mut egui::Ui,
    data: Option<&MarketData>,
    info: Option<&SymbolInfo>,
    settings: &Settings,
    view: &mut View,
    orders: &[(&Account, &OwnOrder)],
) {
    let Some(data) = data else {
        ui.centered_and_justified(|ui| {
            ui.weak("Waiting for market data…");
        });
        return;
    };
    let Some(book) = data.book.as_ref() else {
        ui.centered_and_justified(|ui| {
            ui.weak("Waiting for order book…");
        });
        return;
    };
    let bid = book.bids.first().and_then(|level| positive(&level.price));
    let ask = book.asks.first().and_then(|level| positive(&level.price));
    let Some(reference) = bid
        .zip(ask)
        .map(|(bid, ask)| (bid + ask) / Decimal::TWO)
        .or(bid)
        .or(ask)
    else {
        ui.centered_and_justified(|ui| {
            ui.weak("Waiting for order book levels…");
        });
        return;
    };
    let step = display_step(book, info, settings, reference);
    let utc_ms = crate::utc_now_ms();
    if view.step != Some(step) || view.anchor.is_none() {
        view.step = Some(step);
        view.following = true;
        view.scroll_remainder = 0.0;
    }
    if view.following {
        view.anchor = Some(group(reference, step));
    }
    let window = settings.window_secs.clamp(1, 600);
    let spread = bid
        .and_then(|price| price.to_f64())
        .zip(ask.and_then(|price| price.to_f64()))
        .and_then(|(bid, ask)| crate::view::spread_percent_text(bid, ask))
        .unwrap_or_else(|| "—".to_owned());
    ui.horizontal(|ui| {
        let unit = info.map(|info| if settings.units == Units::Base { info.base.as_str() } else { info.quote.as_str() })
            .unwrap_or(if settings.units == Units::Base { "BASE" } else { "QUOTE" });
        ui.label(egui::RichText::new(format!("{unit} · {window}s · step {} · SPR {spread}", step.normalize())).monospace().size(10.0).color(MUTED))
            .on_hover_text("BID / ASK: resting liquidity. SOLD / BOUGHT: taker volume grouped by price and UTC second in the selected rolling window. ORDERS: visible accounts' open orders. Flow starts when the market connects; unavailable book depth is shown as —. Scroll to explore; double-click the ladder to recenter.");
        if !view.following && ui.small_button("Center").on_hover_text("Follow the current best bid / ask").clicked() {
            view.following = true;
            view.anchor = Some(group(reference, step));
        }
    });
    let (rect, response) = ui.allocate_exact_size(
        ui.available_size().max(Vec2::splat(1.0)),
        Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, SURFACE);
    let body = Rect::from_min_max(
        Pos2::new(rect.left(), rect.top() + 24.0),
        rect.right_bottom(),
    );
    if body.height() < ROW_HEIGHT || rect.width() < 100.0 {
        return;
    }
    if response.double_clicked() {
        view.following = true;
        view.anchor = Some(group(reference, step));
    }
    let scroll = if response.hovered() {
        ui.input(|input| input.smooth_scroll_delta.y)
    } else {
        0.0
    };
    let drag = if response.dragged() {
        ui.input(|input| input.pointer.delta().y)
    } else {
        0.0
    };
    if scroll != 0.0 || drag != 0.0 {
        view.scroll_remainder += scroll + drag;
        let rows = (view.scroll_remainder / ROW_HEIGHT).trunc() as i64;
        if rows != 0 {
            view.following = false;
            view.anchor = view
                .anchor
                .map(|price| (price + step * Decimal::from(rows)).max(step));
            view.scroll_remainder -= rows as f32 * ROW_HEIGHT;
        }
    }
    let count = (body.height() / ROW_HEIGHT).ceil() as i64;
    if !view.following {
        let (low, high) = anchor_bounds(book, &data.dom, orders, utc_ms, window, step, reference);
        view.anchor = view.anchor.map(|price| price.clamp(low, high));
    }
    let anchor = view.anchor.unwrap_or(reference);
    let top_price = anchor + step * Decimal::from(count / 2);
    let low = (top_price - step * Decimal::from(count - 1)).max(Decimal::ZERO);
    let high = top_price + step;
    let rows = book_rows(book, orders, step, low, high);
    // Use wall time, including idle periods: execution volume expires even when
    // the venue stops sending trades. Exchange timestamps are UTC milliseconds.
    let flows = data.dom.collect(utc_ms, window, step, low, high);
    let max_book = rows
        .values()
        .flat_map(|row| [row.bid.value(settings.units), row.ask.value(settings.units)])
        .fold(0.0_f64, f64::max)
        .max(1e-20);
    let max_flow = flows
        .values()
        .flat_map(|flow| {
            [
                flow.buy.value(settings.units),
                flow.sell.value(settings.units),
            ]
        })
        .fold(0.0_f64, f64::max)
        .max(1e-20);
    let edges =
        [0.0, 0.12, 0.29, 0.46, 0.66, 0.83, 1.0].map(|weight| rect.left() + rect.width() * weight);
    let font = FontId::monospace((rect.width() / 42.0).clamp(8.0, 12.0));
    for (index, label) in ["ORDERS", "BID", "SOLD", "PRICE", "BOUGHT", "ASK"]
        .into_iter()
        .enumerate()
    {
        painter.text(
            Pos2::new((edges[index] + edges[index + 1]) / 2.0, rect.top() + 12.0),
            Align2::CENTER_CENTER,
            label,
            FontId::monospace(
                font.size
                    .min(10.0)
                    .min((edges[index + 1] - edges[index] - 4.0) / (label.len() as f32 * 0.6)),
            ),
            MUTED,
        );
    }
    let cells_painter = painter.with_clip_rect(body);
    let hovered = response
        .hover_pos()
        .filter(|pos| body.contains(*pos))
        .map(|pos| ((pos.y - body.top()) / ROW_HEIGHT) as i64);
    let best_bid_row = bid.map(|price| group(price, step));
    let best_ask_row = ask.map(|price| group(price, step));
    let deepest_bid = book
        .bids
        .last()
        .and_then(|level| positive(&level.price))
        .map(|price| group(price, step));
    let deepest_ask = book
        .asks
        .last()
        .and_then(|level| positive(&level.price))
        .map(|price| group(price, step));
    for index in 0..count {
        let price = top_price - step * Decimal::from(index);
        if price <= Decimal::ZERO {
            continue;
        }
        let y = body.top() + index as f32 * ROW_HEIGHT;
        let row = rows.get(&price).copied().unwrap_or_default();
        let flow = flows.get(&price).copied().unwrap_or_default();
        let cell = |col: usize| {
            Rect::from_min_max(
                Pos2::new(edges[col], y),
                Pos2::new(edges[col + 1], y + ROW_HEIGHT),
            )
        };
        if hovered == Some(index) {
            cells_painter.rect_filled(
                Rect::from_min_max(
                    Pos2::new(rect.left(), y),
                    Pos2::new(rect.right(), y + ROW_HEIGHT),
                ),
                0.0,
                Color32::from_white_alpha(8),
            );
        }
        for (col, value, color, max, align_right) in [
            (1, row.bid.value(settings.units), GREEN, max_book, true),
            (2, flow.sell.value(settings.units), RED, max_flow, true),
            (4, flow.buy.value(settings.units), GREEN, max_flow, false),
            (5, row.ask.value(settings.units), RED, max_book, false),
        ] {
            let box_rect = cell(col);
            let on_side = (col == 1 && best_bid_row.is_some_and(|best| price <= best))
                || (col == 5 && best_ask_row.is_some_and(|best| price >= best));
            if on_side {
                cells_painter.rect_filled(box_rect, 0.0, color.gamma_multiply(0.08));
            }
            if value > 0.0 {
                let width = box_rect.width() * (value / max).clamp(0.0, 1.0) as f32;
                let bar = if align_right {
                    Rect::from_min_max(
                        Pos2::new(box_rect.right() - width, y + 1.0),
                        box_rect.right_bottom(),
                    )
                } else {
                    Rect::from_min_size(
                        Pos2::new(box_rect.left(), y + 1.0),
                        Vec2::new(width, ROW_HEIGHT - 1.0),
                    )
                };
                cells_painter.rect_filled(bar, 0.0, color.gamma_multiply(0.24));
                let x = if align_right {
                    box_rect.right() - 4.0
                } else {
                    box_rect.left() + 4.0
                };
                let align = if align_right {
                    Align2::RIGHT_CENTER
                } else {
                    Align2::LEFT_CENTER
                };
                cells_painter.with_clip_rect(box_rect.intersect(body)).text(
                    Pos2::new(x, y + ROW_HEIGHT / 2.0),
                    align,
                    amount(value),
                    font.clone(),
                    if col == 1 || col == 5 { TEXT } else { color },
                );
            } else if (col == 1 && deepest_bid.is_some_and(|last| price < last))
                || (col == 5 && deepest_ask.is_some_and(|last| price > last))
            {
                cells_painter.text(
                    cell(col).center(),
                    Align2::CENTER_CENTER,
                    "—",
                    font.clone(),
                    MUTED.gamma_multiply(0.45),
                );
            }
            let recent = if col == 2 {
                data.dom.latest_sell
            } else if col == 4 {
                data.dom.latest_buy
            } else {
                None
            };
            if recent.is_some_and(|(last, time)| {
                group(last, step) == price && (0..1_000).contains(&(utc_ms - time))
            }) {
                cells_painter.rect_stroke(
                    box_rect.shrink(0.5),
                    0.0,
                    Stroke::new(1.0, color),
                    egui::StrokeKind::Inside,
                );
            }
        }
        let price_cell = cell(3);
        for (best, color, left) in [(best_bid_row, GREEN, true), (best_ask_row, RED, false)] {
            if best == Some(price) {
                let mut highlight = price_cell;
                if best_bid_row == best_ask_row {
                    if left {
                        highlight.max.x = highlight.center().x;
                    } else {
                        highlight.min.x = highlight.center().x;
                    }
                }
                cells_painter.rect_filled(highlight, 0.0, color.gamma_multiply(0.4));
            }
        }
        cells_painter
            .with_clip_rect(price_cell.intersect(body))
            .text(
                price_cell.center(),
                Align2::CENTER_CENTER,
                price.normalize().to_string(),
                FontId::monospace(font.size.min(
                    (price_cell.width() - 6.0) / (price.normalize().to_string().len() as f32 * 0.6),
                )),
                TEXT,
            );
        let own_cell = cell(0);
        for (value, color, y_offset) in [
            (row.own.buy.value(settings.units), GREEN, -5.0),
            (row.own.sell.value(settings.units), RED, 5.0),
        ] {
            if value > 0.0 {
                let both = row.own.buy.base > 0.0 && row.own.sell.base > 0.0;
                cells_painter.with_clip_rect(own_cell.intersect(body)).text(
                    Pos2::new(
                        own_cell.right() - 4.0,
                        own_cell.center().y + if both { y_offset } else { 0.0 },
                    ),
                    Align2::RIGHT_CENTER,
                    amount(value),
                    FontId::monospace(if both { font.size.min(9.0) } else { font.size }),
                    color,
                );
            }
        }
        cells_painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            Stroke::new(0.5, BORDER.gamma_multiply(0.65)),
        );
    }
    for &x in &edges[1..6] {
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            Stroke::new(0.5, BORDER),
        );
    }
    if let Some(index) = hovered {
        let price = top_price - step * Decimal::from(index);
        let row = rows.get(&price).copied().unwrap_or_default();
        let flow = flows.get(&price).copied().unwrap_or_default();
        response.on_hover_ui_at_pointer(|ui| {
            ui.label(
                egui::RichText::new(if step == native_step(book, info) {
                    price.normalize().to_string()
                } else {
                    format!(
                        "{} ≤ price < {}",
                        price.normalize(),
                        (price + step).normalize()
                    )
                })
                .monospace(),
            );
            egui::Grid::new("dom_hover")
                .spacing(Vec2::new(12.0, 3.0))
                .show(ui, |ui| {
                    ui.weak("");
                    ui.weak(info.map_or("Base", |info| info.base.as_str()));
                    ui.weak(info.map_or("Quote", |info| info.quote.as_str()));
                    ui.end_row();
                    for (label, value, color) in [
                        ("Bid", row.bid, GREEN),
                        ("Ask", row.ask, RED),
                        ("Sold", flow.sell, RED),
                        ("Bought", flow.buy, GREEN),
                        ("Own buy", row.own.buy, GREEN),
                        ("Own sell", row.own.sell, RED),
                    ] {
                        ui.colored_label(color, label);
                        let unavailable = (label == "Bid"
                            && deepest_bid.is_none_or(|last| price < last))
                            || (label == "Ask" && deepest_ask.is_none_or(|last| price > last));
                        ui.monospace(if unavailable {
                            "—".to_owned()
                        } else {
                            amount(value.base)
                        });
                        ui.monospace(if unavailable {
                            "—".to_owned()
                        } else {
                            amount(value.quote)
                        });
                        ui.end_row();
                    }
                });
            for (account, order) in orders.iter().filter(|(_, order)| {
                positive(&order.price).is_some_and(|order_price| group(order_price, step) == price)
            }) {
                let suffix = &account.address[account.address.len().saturating_sub(4)..];
                ui.weak(format!(
                    "#{} · {} × {} · …{}",
                    order.order_id, order.price, order.size, suffix
                ));
            }
            if data
                .dom
                .seconds
                .first_key_value()
                .is_some_and(|(&second, _)| {
                    second > utc_ms.div_euclid(1_000) - i64::from(window) + 1
                })
            {
                ui.weak("Flow includes available live history only.");
            }
        });
    }
    // Request a modest refresh for expiration/highlights, independently of feed events.
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(100));
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_core::Level;

    #[test]
    fn grouping_options_remain_open_inside_widget_config() {
        let ctx = egui::Context::default();
        let mut settings = Settings::default();
        let mut time = 0.0;
        let mut frame = |events: Vec<egui::Event>| {
            time += 0.1;
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 800.0))),
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ui| {
                    egui::menu::MenuButton::new("Config")
                        .config(
                            egui::menu::MenuConfig::new()
                                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
                        )
                        .ui(ui, |ui| {
                            ui.set_min_width(280.0);
                            config_ui(ui, &mut settings);
                        });
                },
            );
            output.textures_delta.clear();
            output
        };
        let text_rect = |output: &egui::FullOutput, label: &str| {
            output
                .shapes
                .iter()
                .find_map(|shape| {
                    if let egui::Shape::Text(text) = &shape.shape {
                        (text.galley.job.text == label)
                            .then(|| Rect::from_min_size(text.pos, text.galley.size()))
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| panic!("Missing {label}"))
        };
        let click = |pos: Pos2, pressed| {
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::default(),
                },
            ]
        };
        frame(vec![]);
        let config = text_rect(&frame(vec![]), "Config").center();
        frame(click(config, true));
        frame(click(config, false));
        frame(vec![]);
        frame(vec![]);
        let auto = text_rect(&frame(vec![]), "Auto").center();
        frame(click(auto, true));
        frame(click(auto, false));
        frame(vec![]);
        let output = frame(vec![]);
        let ten = text_rect(&output, "10 ticks").center();
        text_rect(&output, "Price grouping");
        frame(click(ten, true));
        frame(click(ten, false));
        frame(vec![]);
        drop(frame);
        assert_eq!(settings.grouping, Grouping::Ten);
        let book = Book {
            bids: vec![level("85205", "1")],
            asks: vec![level("85206", "1")],
            updated_at_ms: 0,
        };
        assert_eq!(
            display_step(&book, None, &settings, Decimal::from(85_206)),
            Decimal::TEN
        );
    }

    fn trade(price: &str, size: &str, second: i64, side: TradeSide) -> Trade {
        Trade {
            price: price.into(),
            size: size.into(),
            time_ms: second * 1_000,
            side,
        }
    }

    #[test]
    fn exact_grouping_combines_trades_without_rounding_across_boundary() {
        let mut history = History::default();
        for item in [
            trade("0.6203", "2", 100, TradeSide::Buy),
            trade("0.6209", "3", 100, TradeSide::Sell),
            trade("0.6210", "4", 100, TradeSide::Buy),
        ] {
            history.push(&item);
        }
        let flow = history.collect(
            100_000,
            60,
            "0.001".parse().unwrap(),
            Decimal::ZERO,
            Decimal::ONE,
        );
        let row = flow.get(&"0.620".parse().unwrap()).unwrap();
        assert_eq!(row.buy.base, 2.0);
        assert_eq!(row.sell.base, 3.0);
        assert!((row.buy.quote - 1.2406).abs() < 1e-10);
        assert_eq!(flow.get(&"0.621".parse().unwrap()).unwrap().buy.base, 4.0);
    }

    #[test]
    fn rolling_window_expires_when_idle_and_accepts_late_trades() {
        let mut history = History::default();
        history.push(&trade("100", "2", 100, TradeSide::Buy));
        history.push(&trade("100", "3", 102, TradeSide::Sell));
        history.push(&trade("100", "4", 101, TradeSide::Buy));
        let flow = history.collect(102_000, 2, Decimal::ONE, Decimal::ZERO, Decimal::from(200));
        assert_eq!(flow[&Decimal::from(100)].buy.base, 4.0);
        assert_eq!(flow[&Decimal::from(100)].sell.base, 3.0);
        assert!(
            history
                .collect(104_000, 2, Decimal::ONE, Decimal::ZERO, Decimal::from(200))
                .is_empty()
        );
        assert_eq!(history.latest_buy, Some((Decimal::from(100), 101_000)));
    }

    #[test]
    fn invalid_and_unknown_trades_do_not_become_buys_or_sells() {
        let mut history = History::default();
        for item in [
            trade("100", "5", 0, TradeSide::Unknown),
            trade("100", "-1", 0, TradeSide::Buy),
            trade("NaN", "5", 0, TradeSide::Sell),
        ] {
            history.push(&item);
        }
        assert!(history.seconds.is_empty());
    }

    #[test]
    fn retention_and_visible_range_bound_work_and_memory() {
        let mut history = History::default();
        for second in 0..700 {
            history.push(&trade("100", "1", second, TradeSide::Buy));
        }
        assert_eq!(history.seconds.len(), 600);
        assert_eq!(history.entries, 600);
        assert!(
            history
                .collect(
                    699_000,
                    600,
                    Decimal::ONE,
                    Decimal::from(101),
                    Decimal::from(200)
                )
                .is_empty()
        );
    }

    fn level(price: &str, size: &str) -> Level {
        Level {
            price: price.into(),
            size: size.into(),
            quote_size: String::new(),
            depth_base: String::new(),
            depth_quote: String::new(),
        }
    }

    #[test]
    fn grouped_book_and_own_orders_keep_actual_quote_notional() {
        let book = Book {
            bids: vec![level("100.1", "2"), level("100.2", "3")],
            asks: vec![level("100.9", "4")],
            updated_at_ms: 0,
        };
        let account = Account {
            exchange: terminal_core::Exchange::Hyperliquid,
            address: "0x1234".into(),
        };
        let order = OwnOrder {
            coin: "BTC".into(),
            order_id: 1,
            side: TradeSide::Buy,
            price: "100.2".into(),
            size: "3".into(),
        };
        let rows = book_rows(
            &book,
            &[(&account, &order)],
            Decimal::ONE,
            Decimal::from(100),
            Decimal::from(101),
        );
        let row = rows[&Decimal::from(100)];
        assert_eq!(row.bid.base, 5.0);
        assert!((row.bid.quote - 500.8).abs() < 1e-10);
        assert_eq!(row.ask.base, 4.0);
        assert_eq!(row.own.buy.base, 3.0);
        assert_eq!(row.own.sell.base, 0.0);
    }

    #[test]
    fn auto_step_is_tick_aligned_and_native_tick_is_available() {
        let book = Book {
            bids: vec![level("85205.99", "1")],
            asks: vec![level("85206.01", "1")],
            updated_at_ms: 0,
        };
        let mut settings = Settings::default();
        assert_eq!(
            display_step(&book, None, &settings, Decimal::from(85_206)),
            Decimal::ONE
        );
        settings.grouping = Grouping::Tick;
        assert_eq!(
            display_step(&book, None, &settings, Decimal::new(85_206, 0)),
            Decimal::new(1, 2)
        );
    }

    #[test]
    fn scroll_bounds_include_own_orders_beyond_public_book_depth() {
        let book = Book {
            bids: vec![level("100", "1")],
            asks: vec![level("101", "1")],
            updated_at_ms: 0,
        };
        let account = Account {
            exchange: terminal_core::Exchange::Hyperliquid,
            address: "0x1234".into(),
        };
        let order = OwnOrder {
            coin: "BTC".into(),
            order_id: 1,
            side: TradeSide::Buy,
            price: "80".into(),
            size: "3".into(),
        };
        let mut history = History::default();
        history.push(&trade("105", "2", 100, TradeSide::Buy));
        assert_eq!(
            anchor_bounds(
                &book,
                &history,
                &[(&account, &order)],
                100_000,
                60,
                Decimal::ONE,
                Decimal::from(100)
            ),
            (Decimal::from(80), Decimal::from(105))
        );
    }

    #[test]
    fn dom_settings_and_market_survive_layout_save() {
        let mut pane = crate::Pane::new(
            crate::WidgetKind::Dom,
            terminal_core::Market::for_exchange(terminal_core::Exchange::Hyperliquid),
        );
        pane.dom = Settings {
            grouping: Grouping::Hundred,
            window_secs: 600,
            units: Units::Quote,
        };
        let saved = serde_json::to_string(&pane).unwrap();
        let restored: crate::Pane = serde_json::from_str(&saved).unwrap();
        assert!(matches!(restored.kind, crate::WidgetKind::Dom));
        assert_eq!(restored.market, pane.market);
        assert_eq!(restored.dom.grouping, Grouping::Hundred);
        assert_eq!(restored.dom.window_secs, 600);
        assert_eq!(restored.dom.units, Units::Quote);
    }
}
