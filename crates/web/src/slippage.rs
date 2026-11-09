use std::collections::VecDeque;

use eframe::egui::{self, Align2, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use terminal_core::{Book, Level, Market, SymbolInfo};

use crate::{BORDER, GREEN, MUTED, MarketData, RED, TEXT, view};

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub capitals: Vec<String>,
    pub selected: usize,
    pub window_secs: u32,
    pub show_values: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            capitals: ["1000", "10000", "50000", "250000"]
                .map(str::to_owned)
                .into(),
            selected: 2,
            window_secs: 60,
            show_values: true,
        }
    }
}

fn capital(text: &str) -> Option<Decimal> {
    let value: Decimal = text.trim().parse().ok()?;
    (value >= Decimal::new(1, 2) && value <= Decimal::from(1_000_000_000_000u64)).then_some(value)
}

impl Settings {
    pub fn sanitize(&mut self) {
        self.capitals.truncate(6);
        if self.capitals.is_empty() {
            self.capitals = Self::default().capitals;
        }
        for value in &mut self.capitals {
            *value = value.chars().take(32).collect();
        }
        self.selected = self.selected.min(self.capitals.len() - 1);
        self.window_secs = self.window_secs.clamp(10, 600);
    }
}

pub fn config_ui(ui: &mut egui::Ui, settings: &mut Settings, quote: &str) {
    ui.label(
        egui::RichText::new(format!("CAPITAL · {quote}"))
            .small()
            .color(MUTED),
    );
    let mut remove = None;
    for (index, value) in settings.capitals.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            let response = ui.add(
                egui::TextEdit::singleline(value)
                    .desired_width(150.0)
                    .char_limit(32),
            );
            if capital(value).is_none() {
                response.on_hover_text(
                    "Enter an amount from 0.01 to 1,000,000,000,000, without separators.",
                );
                ui.colored_label(RED, "Invalid size");
            } else {
                ui.weak(quote);
            }
            if ui
                .small_button("×")
                .on_hover_text("Remove capital size")
                .clicked()
            {
                remove = Some(index);
            }
        });
    }
    if let Some(index) = remove
        && settings.capitals.len() > 1
    {
        settings.capitals.remove(index);
        if index < settings.selected {
            settings.selected -= 1;
        }
        settings.selected = settings.selected.min(settings.capitals.len() - 1);
    }
    if ui
        .add_enabled(settings.capitals.len() < 6, egui::Button::new("+ Add size"))
        .clicked()
    {
        settings.capitals.push("100000".into());
    }
    ui.horizontal(|ui| {
        ui.label("Window");
        ui.add(
            egui::DragValue::new(&mut settings.window_secs)
                .range(10..=600)
                .suffix("s"),
        );
    });
    ui.checkbox(&mut settings.show_values, "Show current values");
}

#[derive(Clone, Copy, Debug)]
struct Execution {
    base: Decimal,
    vwap: Decimal,
    worst: Decimal,
    bps: f64,
}

// Prefix totals are calculated once when a book changes, so each size costs O(log depth).
fn sweep(levels: &[Level], quote: Decimal, buy: bool) -> Option<Execution> {
    if quote <= Decimal::ZERO {
        return None;
    }
    let best: Decimal = levels.first()?.price.parse().ok()?;
    if best <= Decimal::ZERO {
        return None;
    }
    let (mut low, mut high) = (0, levels.len());
    while low < high {
        let mid = low + (high - low) / 2;
        let depth: Decimal = levels[mid].depth_quote.parse().ok()?;
        if depth < quote {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    let level = levels.get(low)?;
    let price: Decimal = level.price.parse().ok()?;
    let size: Decimal = level.size.parse().ok()?;
    if price <= Decimal::ZERO
        || size <= Decimal::ZERO
        || (buy && price < best)
        || (!buy && price > best)
    {
        return None;
    }
    let (previous_quote, previous_base) = if low == 0 {
        (Decimal::ZERO, Decimal::ZERO)
    } else {
        let previous = &levels[low - 1];
        (
            previous.depth_quote.parse::<Decimal>().ok()?,
            previous.depth_base.parse::<Decimal>().ok()?,
        )
    };
    let remainder = quote.checked_sub(previous_quote)?;
    if previous_base < Decimal::ZERO
        || remainder <= Decimal::ZERO
        || remainder > price.checked_mul(size)?
    {
        return None;
    }
    let base = previous_base.checked_add(remainder.checked_div(price)?)?;
    let vwap = quote.checked_div(base)?;
    let difference = if buy {
        vwap.checked_sub(best)?
    } else {
        best.checked_sub(vwap)?
    };
    let bps = difference
        .checked_div(best)?
        .checked_mul(Decimal::from(10_000))?
        .to_f64()?;
    bps.is_finite().then_some(Execution {
        base,
        vwap,
        worst: price,
        bps: bps.max(0.0),
    })
}

fn valid_book(data: Option<&MarketData>) -> Option<&Book> {
    let book = data.filter(|d| d.connected)?.book.as_ref()?;
    let bid: Decimal = book.bids.first()?.price.parse().ok()?;
    let ask: Decimal = book.asks.first()?.price.parse().ok()?;
    (bid > Decimal::ZERO && ask >= bid).then_some(book)
}

struct Sample {
    time_ms: i64,
    values: Vec<[Option<f64>; 2]>,
}

#[derive(Default)]
pub struct View {
    configuration: Option<(Market, Vec<Option<Decimal>>)>,
    history: VecDeque<Sample>,
    current: Vec<[Option<Execution>; 2]>,
}

impl View {
    #[cfg(test)]
    pub fn sample_count(&self) -> usize {
        self.history.len()
    }

    pub fn observe(
        &mut self,
        market: &Market,
        settings: &Settings,
        data: Option<&MarketData>,
        now: i64,
    ) {
        let capitals: Vec<_> = settings
            .capitals
            .iter()
            .take(6)
            .map(|v| capital(v))
            .collect();
        let configuration = (market.clone(), capitals);
        if self.configuration.as_ref() != Some(&configuration) {
            self.history.clear();
        }
        let book = valid_book(data);
        self.current = configuration
            .1
            .iter()
            .map(|amount| {
                if let (Some(book), Some(amount)) = (book, amount) {
                    [
                        sweep(&book.asks, *amount, true),
                        sweep(&book.bids, *amount, false),
                    ]
                } else {
                    [None, None]
                }
            })
            .collect();
        self.configuration = Some(configuration);
        let values: Vec<_> = self
            .current
            .iter()
            .map(|sides| sides.map(|v| v.map(|v| v.bps)))
            .collect();
        while self
            .history
            .get(1)
            .is_some_and(|s| s.time_ms < now - 600_000)
        {
            self.history.pop_front();
        }
        if self.history.back().is_some_and(|s| s.values == values) {
            return;
        }
        let sample = Sample {
            time_ms: now,
            values,
        };
        if let Some(last) = self.history.back_mut()
            && last.time_ms.div_euclid(100) == now.div_euclid(100)
            && last
                .values
                .iter()
                .zip(&sample.values)
                .all(|(a, b)| (0..2).all(|side| a[side].is_some() == b[side].is_some()))
        {
            *last = sample;
        } else {
            self.history.push_back(sample);
        }
        while self.history.len() > 12004 {
            self.history.pop_front();
        }
    }
}

fn amount(text: &str) -> String {
    capital(text).map_or_else(
        || "Invalid size".into(),
        |v| view::quote_size_text(&v.normalize().to_string()),
    )
}

pub fn ui(
    ui: &mut egui::Ui,
    market: &Market,
    data: Option<&MarketData>,
    info: Option<&SymbolInfo>,
    settings: &mut Settings,
    state: &mut View,
) {
    let now = crate::utc_now_ms();
    state.observe(market, settings, data, now);
    let quote = info.map_or("quote", |i| i.quote.as_str());
    let base = info.map_or("base", |i| i.base.as_str());
    ui.label(egui::RichText::new(format!("{}s · vs best ask / bid", settings.window_secs)).monospace().size(10.0).color(MUTED))
        .on_hover_text("Expected slippage from the observed book, excluding fees. Buy spends the configured quote amount; Sell receives that quote amount. VWAP is compared with that side’s best price. Missing depth is unavailable, never a partial-fill estimate.");
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(format!("Capital · {quote}"))
                .size(10.0)
                .color(MUTED),
        );
        for (index, value) in settings.capitals.iter().enumerate() {
            ui.selectable_value(&mut settings.selected, index, amount(value))
                .on_hover_text(format!("{value} {quote}"));
        }
    });
    ui.horizontal(|ui| {
        ui.colored_label(GREEN, egui::RichText::new("Buy · asks").size(10.0));
        ui.colored_label(RED, egui::RichText::new("Sell · bids").size(10.0));
    });
    let selected = settings.selected;
    let available = ui.available_height();
    let table_height = if settings.show_values {
        (43.0 + 27.0 * settings.capitals.len() as f32).min(available * 0.45)
    } else {
        0.0
    };
    let chart_height =
        (available - table_height - if settings.show_values { 6.0 } else { 0.0 }).max(0.0);
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), chart_height),
        Sense::hover(),
    );
    let plot = Rect::from_min_max(
        rect.left_top() + Vec2::new(47.0, 16.0),
        rect.right_bottom() - Vec2::new(9.0, 27.0),
    );
    let start = now - i64::from(settings.window_secs) * 1000;
    let first = state
        .history
        .partition_point(|s| s.time_ms < start)
        .saturating_sub(1);
    let value = |sample: &Sample, side: usize| sample.values.get(selected).and_then(|v| v[side]);
    if plot.is_positive() {
        let maximum = state
            .history
            .iter()
            .skip(first)
            .flat_map(|s| (0..2).filter_map(|side| value(s, side)))
            .fold(0.0_f64, f64::max);
        let maximum = if maximum > 0.0 { maximum * 1.15 } else { 1.0 };
        let x = |time: i64| {
            plot.left()
                + (time.clamp(start, now) - start) as f32 / (now - start) as f32 * plot.width()
        };
        let y = |v: f64| plot.bottom() - (v / maximum) as f32 * plot.height();
        let painter = ui.painter_at(rect);
        for tick in 0..=4 {
            let v = maximum * f64::from(tick) / 4.0;
            painter.line_segment(
                [Pos2::new(plot.left(), y(v)), Pos2::new(plot.right(), y(v))],
                Stroke::new(0.5, BORDER),
            );
            painter.text(
                Pos2::new(plot.left() - 6.0, y(v)),
                Align2::RIGHT_CENTER,
                format!("{v:.2}"),
                FontId::monospace(9.0),
                MUTED,
            );
        }
        painter.text(
            Pos2::new(plot.left(), rect.top() + 4.0),
            Align2::LEFT_CENTER,
            "bps",
            FontId::monospace(9.0),
            MUTED,
        );
        let ticks = (plot.width() / 86.0).floor().clamp(1.0, 8.0) as i64;
        for i in 0..=ticks {
            let time = start + (now - start) * i / ticks;
            painter.text(
                Pos2::new(x(time), rect.bottom() - 16.0),
                if i == 0 {
                    Align2::LEFT_CENTER
                } else if i == ticks {
                    Align2::RIGHT_CENTER
                } else {
                    Align2::CENTER_CENTER
                },
                view::trade_utc(time, true),
                FontId::monospace(9.0),
                MUTED,
            );
        }
        painter.text(
            rect.right_bottom() - Vec2::new(9.0, 4.0),
            Align2::RIGHT_CENTER,
            "UTC",
            FontId::monospace(8.0),
            MUTED,
        );
        let clipped = painter.with_clip_rect(plot);
        for (side, color) in [GREEN, RED].into_iter().enumerate() {
            let draw = |points: &[(i64, f64)]| {
                if points.len() < 2 {
                    return;
                }
                let reduced =
                    view::simplify_points(points, start, now, plot.width().max(1.0) as usize);
                let mut points: Vec<Pos2> = Vec::with_capacity(reduced.len() * 2);
                // A book value stays unchanged until the next observation; never imply a gradual move.
                for (time, v) in reduced {
                    if let Some(previous) = points.last() {
                        points.push(Pos2::new(x(time), previous.y));
                    }
                    points.push(Pos2::new(x(time), y(v)));
                }
                if side == 0 {
                    clipped.add(egui::Shape::line(points, Stroke::new(1.5, color)));
                } else {
                    clipped.extend(egui::Shape::dashed_line(
                        &points,
                        Stroke::new(1.5, color),
                        5.0,
                        3.0,
                    ));
                }
            };
            let mut points = Vec::new();
            for sample in state.history.iter().skip(first) {
                if let Some(v) = value(sample, side) {
                    points.push((sample.time_ms.max(start), v));
                } else {
                    if let Some((_, v)) = points.last().copied() {
                        points.push((sample.time_ms.max(start), v));
                    }
                    draw(&points);
                    points.clear();
                }
            }
            if let Some(v) = state.history.back().and_then(|s| value(s, side)) {
                points.push((now, v));
                clipped.circle_filled(Pos2::new(x(now), y(v)), 2.5, color);
            }
            draw(&points);
        }
        if state
            .current
            .get(selected)
            .is_none_or(|v| v.iter().all(Option::is_none))
        {
            painter.text(
                plot.center(),
                Align2::CENTER_CENTER,
                if capital(&settings.capitals[selected]).is_none() {
                    "Invalid capital size"
                } else if valid_book(data).is_some() {
                    "Insufficient visible depth"
                } else {
                    "Waiting for a valid order book…"
                },
                FontId::proportional(11.0),
                MUTED,
            );
        }
        if let Some(pointer) = response.hover_pos().filter(|p| plot.contains(*p)) {
            let time =
                start + ((pointer.x - plot.left()) / plot.width() * (now - start) as f32) as i64;
            painter.line_segment(
                [
                    Pos2::new(pointer.x, plot.top()),
                    Pos2::new(pointer.x, plot.bottom()),
                ],
                Stroke::new(0.7, MUTED),
            );
            response.on_hover_ui_at_pointer(|ui| {
                ui.monospace(format!(
                    "{} UTC · {} {quote}",
                    view::trade_utc(time, false),
                    settings.capitals[selected]
                ));
                let sample = state.history.iter().rev().find(|s| s.time_ms <= time);
                for (side, label, color) in [(0, "Buy · asks", GREEN), (1, "Sell · bids", RED)] {
                    ui.colored_label(
                        color,
                        format!(
                            "{label}: {}",
                            sample
                                .and_then(|s| value(s, side))
                                .map_or_else(|| "—".into(), |v| format!("{v:.3} bps"))
                        ),
                    );
                }
            });
        }
    }
    if settings.show_values {
        ui.label(egui::RichText::new("CURRENT VALUES").size(9.0).color(MUTED));
        let (header, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
        for (fraction, label, align) in [
            (0.0, format!("CAPITAL · {quote}"), Align2::LEFT_CENTER),
            (0.65, "BUY · bps".into(), Align2::RIGHT_CENTER),
            (1.0, "SELL · bps".into(), Align2::RIGHT_CENTER),
        ] {
            ui.painter().text(
                Pos2::new(header.left() + header.width() * fraction, header.center().y),
                align,
                label,
                FontId::monospace(9.0),
                MUTED,
            );
        }
        egui::ScrollArea::vertical()
            .id_salt("slippage-values")
            .auto_shrink([false, false])
            .show_rows(ui, 27.0, settings.capitals.len(), |ui, range| {
                for index in range {
                    let (row, response) = ui
                        .allocate_exact_size(Vec2::new(ui.available_width(), 27.0), Sense::hover());
                    let painter = ui.painter_at(row);
                    painter.text(
                        row.left_center(),
                        Align2::LEFT_CENTER,
                        amount(&settings.capitals[index]),
                        FontId::monospace(10.0),
                        TEXT,
                    );
                    for (side, fraction, color) in [(0, 0.65, GREEN), (1, 1.0, RED)] {
                        let text = state.current[index][side].map_or_else(
                            || {
                                if capital(&settings.capitals[index]).is_none() {
                                    "Invalid size".into()
                                } else if valid_book(data).is_some() {
                                    "No depth".into()
                                } else {
                                    "—".into()
                                }
                            },
                            |v| format!("{:.2}", v.bps),
                        );
                        painter.text(
                            Pos2::new(row.left() + row.width() * fraction, row.center().y),
                            Align2::RIGHT_CENTER,
                            text,
                            FontId::monospace(10.0),
                            color,
                        );
                    }
                    painter.line_segment(
                        [row.left_bottom(), row.right_bottom()],
                        Stroke::new(0.5, BORDER),
                    );
                    response.on_hover_ui_at_pointer(|ui| {
                        ui.label(format!(
                            "{} {} {}",
                            market.exchange.label(),
                            market.kind.label(),
                            market.symbol
                        ));
                        ui.monospace(format!("{} {quote}", settings.capitals[index]));
                        for (side, label, color) in
                            [(0, "Buy · asks", GREEN), (1, "Sell · bids", RED)]
                        {
                            if let Some(v) = state.current[index][side] {
                                ui.colored_label(color, format!("{label}: {:.3} bps", v.bps));
                                ui.monospace(format!(
                                    "VWAP {} · Worst {}",
                                    v.vwap.normalize(),
                                    v.worst.normalize()
                                ));
                                ui.monospace(format!("Size {} {base}", v.base.normalize()));
                            } else {
                                ui.weak(format!("{label}: unavailable; no partial-fill estimate."));
                            }
                        }
                    });
                }
            });
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(100));
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_core::Exchange;

    fn data() -> MarketData {
        let level = |price: &str, size: &str| Level {
            price: price.into(),
            size: size.into(),
            quote_size: String::new(),
            depth_base: String::new(),
            depth_quote: String::new(),
        };
        let mut data = MarketData::default();
        data.set_book(
            Book {
                bids: vec![level("100", "2"), level("99", "3")],
                asks: vec![level("101", "1"), level("102", "3")],
                updated_at_ms: 1000,
            },
            Exchange::Binance,
            false,
        );
        data.connected = true;
        data
    }

    #[test]
    fn quote_walk_handles_partial_levels_boundaries_and_both_sides() {
        let data = data();
        let book = data.book.as_ref().unwrap();
        let buy = sweep(&book.asks, Decimal::from(202), true).unwrap();
        let expected_base = Decimal::ONE + Decimal::from(101) / Decimal::from(102);
        assert_eq!(buy.base, expected_base);
        assert_eq!(buy.worst, Decimal::from(102));
        assert!(
            (buy.bps - ((202.0 / expected_base.to_f64().unwrap() - 101.0) / 101.0 * 10000.0)).abs()
                < 1e-9
        );
        assert_eq!(
            sweep(&book.asks, Decimal::from(101), true).unwrap().bps,
            0.0
        );
        let full = sweep(&book.asks, Decimal::from(407), true).unwrap();
        assert_eq!(full.base, Decimal::from(4));
        assert_eq!(full.vwap, Decimal::new(10175, 2));
        let sell = sweep(&book.bids, Decimal::from(299), false).unwrap();
        assert_eq!(sell.base, Decimal::from(3));
        assert_eq!(sell.worst, Decimal::from(99));
        assert!((sell.bps - 100.0 / 3.0).abs() < 1e-9);
        assert_eq!(
            sweep(&book.bids, Decimal::from(200), false).unwrap().bps,
            0.0
        );
        assert!(sweep(&book.bids, Decimal::from(497), false).is_some());
        assert!(sweep(&book.asks, Decimal::from(408), true).is_none());
        assert!(sweep(&book.bids, Decimal::from(498), false).is_none());
        assert!(sweep(&book.bids, Decimal::ZERO, false).is_none());
    }

    #[test]
    fn bundle_units_and_fractional_capital_preserve_quote_slippage() {
        let data = data();
        let book = data.book.as_ref().unwrap();
        let expected = sweep(&book.asks, Decimal::new(20225, 2), true).unwrap().bps;
        let mut bundled = book.clone();
        for levels in [&mut bundled.bids, &mut bundled.asks] {
            for level in levels {
                level.price =
                    (level.price.parse::<Decimal>().unwrap() * Decimal::from(1000)).to_string();
                level.size =
                    (level.size.parse::<Decimal>().unwrap() / Decimal::from(1000)).to_string();
                level.depth_base = (level.depth_base.parse::<Decimal>().unwrap()
                    / Decimal::from(1000))
                .to_string();
            }
        }
        assert!(
            (sweep(&bundled.asks, Decimal::new(20225, 2), true)
                .unwrap()
                .bps
                - expected)
                .abs()
                < 1e-9
        );
        assert!(capital("0.01").is_some());
        for text in ["0", "-1", "NaN", "inf", "1,000", "1000000000001"] {
            assert!(capital(text).is_none(), "{text}");
        }
        let mut invalid = book.asks.clone();
        invalid[1].depth_quote = "NaN".into();
        assert!(sweep(&invalid, Decimal::from(202), true).is_none());
        let mut crossed = self::data();
        crossed.book.as_mut().unwrap().asks[0].price = "99".into();
        assert!(valid_book(Some(&crossed)).is_none());
    }

    #[test]
    fn history_keeps_disconnect_and_one_sided_depth_gaps_without_mixing_sizes() {
        let market = Market::default();
        let mut state = View::default();
        let mut data = data();
        let mut settings = Settings {
            capitals: vec!["450".into(), "202".into()],
            selected: 0,
            ..Default::default()
        };
        state.observe(&market, &settings, Some(&data), 1000);
        assert!(state.current[0][0].is_none());
        assert!(state.current[0][1].is_some());
        data.connected = false;
        state.observe(&market, &settings, Some(&data), 1050);
        data.connected = true;
        state.observe(&market, &settings, Some(&data), 1070);
        assert_eq!(
            state.history.len(),
            3,
            "do not drop a short disconnect within a sampling interval"
        );
        assert!(
            state.history[1]
                .values
                .iter()
                .all(|sides| sides.iter().all(Option::is_none))
        );
        settings.selected = 1;
        settings.show_values = false;
        settings.window_secs = 300;
        state.observe(&market, &settings, Some(&data), 1200);
        assert_eq!(
            state.history.len(),
            3,
            "presentation changes preserve history for all sizes"
        );
        settings.capitals[0] = "400".into();
        state.observe(&market, &settings, Some(&data), 1300);
        assert_eq!(state.history.len(), 1);
        let other = Market::for_exchange(Exchange::Hyperliquid);
        state.observe(&other, &settings, Some(&data), 1400);
        assert_eq!(state.history.len(), 1, "markets cannot mix");
    }

    #[test]
    fn history_is_sampled_and_bounded_while_current_values_remain_live() {
        let market = Market::default();
        let mut state = View::default();
        let mut data = data();
        let settings = Settings {
            capitals: vec!["202".into()],
            selected: 0,
            ..Default::default()
        };
        for now in 1000..3000 {
            let size = Decimal::ONE + Decimal::new(now % 100, 3);
            let mut book = data.book.clone().unwrap();
            book.asks[0].size = size.to_string();
            book.asks[0].depth_quote.clear();
            book.updated_at_ms = now;
            data.set_book(book, Exchange::Binance, false);
            state.observe(&market, &settings, Some(&data), now);
        }
        assert!(state.history.len() <= 21);
        assert!(
            state.history.len() > 5,
            "changed book sizes must produce new observations"
        );
        let current = state.current[0][0].unwrap().bps;
        assert_eq!(state.history.back().unwrap().values[0][0], Some(current));
        state.observe(&market, &settings, None, 1_000_000);
        assert!(
            state.history.len() <= 2,
            "keep only a boundary sample and the current gap"
        );
    }

    #[test]
    fn chart_keeps_constant_values_flat_until_the_next_book_change() {
        let market = Market::default();
        let mut data = data();
        let mut state = View::default();
        let mut settings = Settings {
            capitals: vec!["202".into()],
            selected: 0,
            show_values: false,
            ..Default::default()
        };
        let now = crate::utc_now_ms();
        state.observe(&market, &settings, Some(&data), now - 30_000);
        let mut book = data.book.clone().unwrap();
        book.asks[0].size = "10".into();
        book.asks[0].depth_quote.clear();
        book.updated_at_ms += 1;
        data.set_book(book, terminal_core::Exchange::Binance, false);
        state.observe(&market, &settings, Some(&data), now - 10_000);
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(640.0, 400.0))),
                ..Default::default()
            },
            |ui| super::ui(ui, &market, Some(&data), None, &mut settings, &mut state),
        );
        output.textures_delta.clear();
        let path=output.shapes.iter().find_map(|s| match &s.shape {egui::Shape::Path(p) if matches!(p.stroke.color,egui::epaint::ColorMode::Solid(c) if c==GREEN)=>Some(p),_=>None}).unwrap();
        assert!(
            path.points
                .windows(2)
                .any(|p| (p[0].x - p[1].x).abs() < 0.01 && (p[0].y - p[1].y).abs() > 1.0)
        );
        assert!(
            path.points
                .windows(2)
                .all(|p| (p[0].x - p[1].x).abs() < 0.01 || (p[0].y - p[1].y).abs() < 0.01),
            "unchanged books must not produce diagonal ramps"
        );
    }

    #[test]
    fn narrow_widgets_hide_the_values_panel_and_give_its_space_to_the_chart() {
        let market = Market::default();
        let data = data();
        for width in [240.0, 640.0] {
            let mut chart_bottoms = Vec::new();
            for show_values in [true, false] {
                let ctx = egui::Context::default();
                let mut state = View::default();
                let mut settings = Settings {
                    capitals: vec!["202".into(), "450".into()],
                    selected: 0,
                    show_values,
                    ..Default::default()
                };
                let mut output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, 420.0))),
                        ..Default::default()
                    },
                    |ui| super::ui(ui, &market, Some(&data), None, &mut settings, &mut state),
                );
                output.textures_delta.clear();
                let text: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|s| match &s.shape {
                        egui::Shape::Text(t) => Some((&t.galley.job.text, t.pos)),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    text.iter().any(|(s, _)| s.as_str() == "CURRENT VALUES"),
                    show_values
                );
                assert_eq!(
                    text.iter().any(|(s, _)| s.as_str() == "No depth"),
                    show_values
                );
                let utc = text.iter().find(|(s, _)| s.as_str() == "UTC").unwrap().1;
                chart_bottoms.push(utc.y);
                assert!(text.iter().any(|(s,_)|s.len()==8&&s.bytes().filter(|b|*b==b':').count()==2),"full UTC time ticks");
            }
            assert!(chart_bottoms[1] > chart_bottoms[0] + 80.0);
        }
    }
}
