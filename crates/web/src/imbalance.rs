use std::collections::{HashMap, VecDeque};

use eframe::egui::{self, Align2, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use terminal_core::{Book, Level, Market, SymbolInfo};

use crate::{
    BORDER, MUTED, MarketData, TEXT,
    orderflow::{self, Units},
    view,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Weighting {
    #[default]
    Flat,
    Exponential,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub levels: usize,
    pub window_secs: u32,
    pub units: Units,
    pub weighting: Weighting,
    pub half_life_bps: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            levels: 10,
            window_secs: 60,
            units: Units::Quote,
            weighting: Weighting::Flat,
            half_life_bps: 2.0,
        }
    }
}

impl Settings {
    fn half_life(&self) -> f64 {
        if self.half_life_bps.is_finite() {
            self.half_life_bps.clamp(0.01, 10_000.0)
        } else {
            2.0
        }
    }

    fn model_label(&self) -> String {
        match self.weighting {
            Weighting::Flat => "FLAT".into(),
            Weighting::Exponential => format!("EXP h={:.2}bps", self.half_life()),
        }
    }
}

pub fn config_ui(ui: &mut egui::Ui, settings: &mut Settings) {
    ui.horizontal(|ui| {
        ui.label("Levels per side");
        ui.add(egui::DragValue::new(&mut settings.levels).range(1..=5000));
    }).response.on_hover_text("Use exactly N native price levels on each side. Insufficient depth is unavailable, never silently capped. Equal level counts can cover different price distances across venues.");
    ui.horizontal(|ui| {
        ui.label("Window");
        ui.add(
            egui::DragValue::new(&mut settings.window_secs)
                .range(10..=600)
                .suffix("s"),
        );
    });
    ui.horizontal(|ui| {
        ui.label("Volume");
        ui.selectable_value(&mut settings.units, Units::Base, "Base");
        ui.selectable_value(&mut settings.units, Units::Quote, "Quote");
    });
    ui.horizontal(|ui| {
        ui.label("Weighting");
        ui.selectable_value(&mut settings.weighting, Weighting::Flat, "Flat");
        ui.selectable_value(
            &mut settings.weighting,
            Weighting::Exponential,
            "Exponential",
        );
    });
    if settings.weighting == Weighting::Exponential {
        settings.half_life_bps = settings.half_life();
        ui.horizontal(|ui| {
            ui.label("Half-life");
            ui.add(egui::DragValue::new(&mut settings.half_life_bps).speed(0.1).range(0.01..=10_000.0).suffix(" bps"));
        }).response.on_hover_text("w = 2^(−distance from mid in bps / half-life). At h bps weight is 1/2; at 2h it is 1/4. 1 bp = 0.01%. Applies to the selected N levels on each side.");
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Values {
    bid: Decimal,
    ask: Decimal,
    weighted_percent: Option<f64>,
}

impl Values {
    fn percent(self) -> Option<f64> {
        if let Some(value) = self.weighted_percent {
            return Some(value);
        }
        self.bid
            .checked_sub(self.ask)?
            .checked_div(self.bid.checked_add(self.ask)?)?
            .checked_mul(Decimal::from(100))?
            .to_f64()
    }
}

fn total(levels: &[Level], n: usize, units: Units) -> Option<Decimal> {
    let last = levels.get(n.checked_sub(1)?)?;
    // Books already cache prefix sums, so additional widgets need no depth scan.
    let cached = match units {
        Units::Base => &last.depth_base,
        Units::Quote => &last.depth_quote,
    };
    if let Ok(value) = cached.parse::<Decimal>()
        && value > Decimal::ZERO
    {
        return Some(value);
    }
    levels
        .iter()
        .take(n)
        .try_fold(Decimal::ZERO, |sum, level| {
            let price: Decimal = level.price.parse().ok()?;
            let size: Decimal = level.size.parse().ok()?;
            if price <= Decimal::ZERO || size <= Decimal::ZERO {
                return None;
            }
            sum.checked_add(match units {
                Units::Base => size,
                Units::Quote => price.checked_mul(size)?,
            })
        })
        .filter(|sum| *sum > Decimal::ZERO)
}

pub fn measure(book: &Book, settings: &Settings) -> Option<Values> {
    let bid: Decimal = book.bids.first()?.price.parse().ok()?;
    let ask: Decimal = book.asks.first()?.price.parse().ok()?;
    if bid <= Decimal::ZERO || ask < bid {
        return None;
    }
    let mut result = Values {
        bid: total(&book.bids, settings.levels, settings.units)?,
        ask: total(&book.asks, settings.levels, settings.units)?,
        weighted_percent: None,
    };
    if settings.weighting == Weighting::Exponential {
        let mid = (bid.to_f64()? + ask.to_f64()?) / 2.0;
        let half_life = settings.half_life();
        let weighted = |levels: &[Level], best: Decimal, bids: bool| {
            levels
                .iter()
                .take(settings.levels)
                .try_fold(0.0, |sum, level| {
                    let price: Decimal = level.price.parse().ok()?;
                    let size: Decimal = level.size.parse().ok()?;
                    if price <= Decimal::ZERO || size <= Decimal::ZERO {
                        return None;
                    }
                    let distance = if bids {
                        best.checked_sub(price)?
                    } else {
                        price.checked_sub(best)?
                    };
                    if distance < Decimal::ZERO {
                        return None;
                    }
                    // Both best prices have the same distance from mid. Cancel that
                    // common exponent to avoid underflow on wide-spread markets.
                    let weight = (-distance.to_f64()? / mid * 10_000.0 / half_life).exp2();
                    let volume = match settings.units {
                        Units::Base => size,
                        Units::Quote => price.checked_mul(size)?,
                    }
                    .to_f64()?;
                    Some(sum + volume * weight)
                })
        };
        let bid = weighted(&book.bids, bid, true)?;
        let ask = weighted(&book.asks, ask, false)?;
        let value = 100.0 * ((bid - ask) / (bid + ask));
        if !value.is_finite() {
            return None;
        }
        result.weighted_percent = Some(value.clamp(-100.0, 100.0));
    }
    result.percent()?;
    Some(result)
}

#[derive(Clone, Copy)]
struct Sample {
    time_ms: i64,
    values: Option<f64>,
}

#[derive(Default)]
pub struct View {
    configuration: Option<(usize, Units, Weighting, f64)>,
    histories: HashMap<Market, VecDeque<Sample>>,
}

impl View {
    pub fn observe(
        &mut self,
        market: &Market,
        settings: &Settings,
        data: Option<&MarketData>,
        now: i64,
    ) {
        let configuration = (
            settings.levels.clamp(1, 5000),
            settings.units,
            settings.weighting,
            if settings.weighting == Weighting::Exponential {
                settings.half_life()
            } else {
                0.0
            },
        );
        if self.configuration != Some(configuration) {
            self.histories.clear();
            self.configuration = Some(configuration);
        }
        let values = data
            .filter(|d| d.connected)
            .and_then(|d| d.book.as_ref())
            .and_then(|b| measure(b, settings))
            .and_then(Values::percent);
        let history = self.histories.entry(market.clone()).or_default();
        while history
            .get(1)
            .is_some_and(|sample| sample.time_ms < now - 600_000)
        {
            history.pop_front();
        }
        if history.back().is_some_and(|last| last.values == values) {
            return;
        }
        // Keep the live endpoint while sampling chart history at up to 10 Hz.
        if let Some(last) = history.back_mut()
            && last.time_ms.div_euclid(100) == now.div_euclid(100)
            && last.values.is_some() == values.is_some()
        {
            *last = Sample {
                time_ms: now,
                values,
            };
        } else {
            history.push_back(Sample {
                time_ms: now,
                values,
            });
        }
        while history.len() > 12004
            || history
                .get(1)
                .is_some_and(|sample| sample.time_ms < now - 600_000)
        {
            history.pop_front();
        }
    }
}

pub fn ui(
    ui: &mut egui::Ui,
    markets: &[Market],
    data: &HashMap<Market, MarketData>,
    catalogs: &HashMap<(terminal_core::Exchange, terminal_core::MarketKind), Vec<SymbolInfo>>,
    settings: &Settings,
    state: &mut View,
) {
    let now = crate::utc_now_ms();
    let start = now - i64::from(settings.window_secs.clamp(10, 600)) * 1000;
    let mut unique = Vec::new();
    for market in markets {
        if !unique.contains(&market) {
            unique.push(market);
            state.observe(market, settings, data.get(market), now);
        }
    }
    state.histories.retain(|market, _| unique.contains(&market));
    ui.label(egui::RichText::new(format!("{}s · top {} / side · {} · {}",settings.window_secs.clamp(10,600),settings.levels.clamp(1,5000),if settings.units==Units::Base {"BASE"}else{"QUOTE"}, settings.model_label())).monospace().size(10.0).color(MUTED))
        .on_hover_text("OBI = (Bid − Ask) / (Bid + Ask) × 100%, using Flat or exponential distance-weighted volumes. BID/ASK table totals always show raw depth. Observation times are UTC; no past depth is reconstructed.");
    ui.horizontal_wrapped(|ui| {
        for &market in &unique {
            ui.label(
                egui::RichText::new(market.exchange.label())
                    .size(10.0)
                    .color(view::venue_color(market.exchange)),
            )
            .on_hover_text(format!(
                "{} {} {}",
                market.exchange.label(),
                market.kind.label(),
                market.symbol
            ));
        }
    });
    let available = ui.available_height();
    let table_height = (23.0 + unique.len() as f32 * 34.0).min(available * 0.4);
    let chart_height = (available - table_height - 8.0).max(0.0);
    if chart_height >= 80.0 {
        let (rect, response) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), chart_height),
            Sense::hover(),
        );
        let plot = Rect::from_min_max(
            rect.left_top() + Vec2::new(43.0, 9.0),
            rect.right_bottom() - Vec2::new(9.0, 23.0),
        );
        let painter = ui.painter_at(rect);
        if plot.is_positive() {
            let x = |time: i64| {
                plot.left()
                    + ((time.clamp(start, now) - start) as f64 / (now - start) as f64) as f32
                        * plot.width()
            };
            let y = |value: f64| plot.center().y - value as f32 / 200.0 * plot.height();
            for value in [-100, -50, 0, 50, 100] {
                painter.line_segment(
                    [
                        Pos2::new(plot.left(), y(value as f64)),
                        Pos2::new(plot.right(), y(value as f64)),
                    ],
                    Stroke::new(
                        if value == 0 { 1.0 } else { 0.5 },
                        if value == 0 {
                            MUTED.gamma_multiply(0.7)
                        } else {
                            BORDER
                        },
                    ),
                );
                painter.text(
                    Pos2::new(plot.left() - 6.0, y(value as f64)),
                    Align2::RIGHT_CENTER,
                    if value == 0 {
                        "0%".into()
                    } else {
                        format!("{value:+}%")
                    },
                    FontId::monospace(9.0),
                    MUTED,
                );
            }
            let ticks = (plot.width() / 75.0).floor().clamp(2.0, 8.0) as i64;
            for i in 0..=ticks {
                let time = start + (now - start) * i / ticks;
                painter.text(
                    Pos2::new(x(time), rect.bottom() - 7.0),
                    if i == 0 {
                        Align2::LEFT_CENTER
                    } else if i == ticks {
                        Align2::RIGHT_CENTER
                    } else {
                        Align2::CENTER_CENTER
                    },
                    &view::trade_utc(time, true)[3..],
                    FontId::monospace(9.0),
                    MUTED,
                );
            }
            for &market in &unique {
                let Some(history) = state.histories.get(market) else {
                    continue;
                };
                let color = view::venue_color(market.exchange);
                let clipped = painter.with_clip_rect(plot);
                let draw = |points: &[(i64, f64)]| {
                    if points.len() > 1 {
                        let reduced = view::simplify_points(
                            points,
                            start,
                            now,
                            plot.width().max(1.0) as usize,
                        );
                        clipped.add(egui::Shape::line(
                            reduced
                                .into_iter()
                                .map(|(time, value)| Pos2::new(x(time), y(value)))
                                .collect(),
                            Stroke::new(1.5, color),
                        ));
                    }
                };
                let endpoint = history.back().map(|last| Sample {
                    time_ms: now,
                    values: last.values,
                });
                let mut points = Vec::new();
                for sample in history.iter().chain(endpoint.as_ref()) {
                    if let Some(value) = sample.values {
                        points.push((sample.time_ms, value));
                    } else {
                        draw(&points);
                        points.clear();
                    }
                }
                draw(&points);
                if let Some((time, value)) = points.last() {
                    clipped.circle_filled(Pos2::new(x(*time), y(*value)), 2.5, color);
                }
            }

            if let Some(pointer) = response.hover_pos().filter(|p| plot.contains(*p)) {
                let time = start
                    + ((pointer.x - plot.left()) / plot.width() * (now - start) as f32) as i64;
                painter.line_segment(
                    [
                        Pos2::new(pointer.x, plot.top()),
                        Pos2::new(pointer.x, plot.bottom()),
                    ],
                    Stroke::new(0.7, MUTED),
                );
                response.on_hover_ui_at_pointer(|ui| {
                    ui.monospace(format!("{} UTC · observed", view::trade_utc(time, false)));
                    for &market in &unique {
                        let value = state
                            .histories
                            .get(market)
                            .and_then(|h| h.iter().rev().find(|p| p.time_ms <= time))
                            .and_then(|s| s.values);
                        ui.colored_label(
                            view::venue_color(market.exchange),
                            format!(
                                "{} {} {} · {}",
                                market.exchange.label(),
                                market.kind.label(),
                                market.symbol,
                                value.map_or_else(|| "—".into(), |v| format!("{v:+.2}%"))
                            ),
                        );
                    }
                });
            }
        }
    }
    let (header, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 20.0), Sense::hover());
    for (x, label, align) in [
        (header.left() + 5.0, "MARKET", Align2::LEFT_CENTER),
        (
            header.left() + header.width() * 0.59,
            "BID",
            Align2::RIGHT_CENTER,
        ),
        (
            header.left() + header.width() * 0.79,
            "ASK",
            Align2::RIGHT_CENTER,
        ),
        (header.right() - 5.0, "OBI", Align2::RIGHT_CENTER),
    ] {
        ui.painter().text(
            Pos2::new(x, header.center().y),
            align,
            label,
            FontId::monospace(9.0),
            MUTED,
        );
    }
    egui::ScrollArea::vertical()
        .id_salt("imbalance-markets")
        .auto_shrink([false, false])
        .show_rows(ui, 34.0, unique.len(), |ui, range| {
            for index in range {
                let market = unique[index];
                let book = data
                    .get(market)
                    .filter(|d| d.connected)
                    .and_then(|d| d.book.as_ref());
                let values = book.and_then(|b| measure(b, settings));
                let info = orderflow::symbol_info(market, catalogs);
                let (base, multiplier) =
                    info.map_or(("base", 1), |i| orderflow::base_units(&i.base, market.kind));
                let unit = if settings.units == Units::Base {
                    base
                } else {
                    info.map_or("quote", |i| i.quote.as_str())
                };
                let scale = if settings.units == Units::Base {
                    Decimal::from(multiplier)
                } else {
                    Decimal::ONE
                };
                let amount = |v: Decimal| {
                    view::quote_size_text(
                        &v.checked_mul(scale).unwrap_or(v).normalize().to_string(),
                    )
                };
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::hover());
                let painter = ui.painter_at(rect);
                let name = Rect::from_min_max(
                    rect.left_top(),
                    Pos2::new(rect.left() + rect.width() * 0.38, rect.bottom()),
                );
                painter.with_clip_rect(name).text(
                    rect.left_top() + Vec2::new(5.0, 10.0),
                    Align2::LEFT_CENTER,
                    market.exchange.label(),
                    FontId::monospace(10.0),
                    view::venue_color(market.exchange),
                );
                painter.with_clip_rect(name).text(
                    rect.left_top() + Vec2::new(5.0, 24.0),
                    Align2::LEFT_CENTER,
                    format!("{} · {unit}", market.symbol),
                    FontId::monospace(9.0),
                    MUTED,
                );
                for (column, text) in [
                    (0.59, values.map_or_else(|| "—".into(), |v| amount(v.bid))),
                    (0.79, values.map_or_else(|| "—".into(), |v| amount(v.ask))),
                    (
                        1.0,
                        values
                            .and_then(Values::percent)
                            .map_or_else(|| "—".into(), |v| format!("{v:+.1}%")),
                    ),
                ] {
                    painter.text(
                        Pos2::new(rect.left() + rect.width() * column - 5.0, rect.center().y),
                        Align2::RIGHT_CENTER,
                        text,
                        FontId::monospace(10.0),
                        TEXT,
                    );
                }
                if values.is_none()
                    && let Some(book) = book
                    && book.bids.len().min(book.asks.len()) < settings.levels.clamp(1, 5000)
                {
                    painter.text(
                        rect.right_center() + Vec2::new(-5.0, 10.0),
                        Align2::RIGHT_CENTER,
                        format!(
                            "{}/{}",
                            book.bids.len().min(book.asks.len()),
                            settings.levels.clamp(1, 5000)
                        ),
                        FontId::monospace(9.0),
                        MUTED,
                    );
                }
                painter.line_segment(
                    [rect.left_bottom(), rect.right_bottom()],
                    Stroke::new(0.5, BORDER),
                );
                response.on_hover_ui_at_pointer(|ui| {
                    ui.label(format!(
                        "{} {} {}",
                        market.exchange.label(),
                        market.kind.label(),
                        market.symbol
                    ));
                    if let Some(v) = values {
                        ui.monospace(format!(
                            "Bid {} / Ask {} {unit}",
                            amount(v.bid),
                            amount(v.ask)
                        ));
                        ui.monospace(format!(
                            "OBI {:+.3}% · {} levels per side · {}",
                            v.percent().unwrap(),
                            settings.levels, settings.model_label()
                        ));
                        if settings.weighting == Weighting::Exponential {
                            ui.weak("Bid/Ask totals are raw depth; OBI uses exponential distance weights.");
                        }
                    } else if let Some(book) = book {
                        ui.label(format!(
                            "Available: {} bid / {} ask levels. Requested: {} per side.",
                            book.bids.len(),
                            book.asks.len(),
                            settings.levels
                        ));
                    } else {
                        ui.weak("Waiting for a connected order book.");
                    }
                });
            }
        });
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(100));
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_core::Exchange;

    fn weighted_book() -> Book {
        let level = |price: &str, size: &str| Level {
            price: price.into(),
            size: size.into(),
            quote_size: String::new(),
            depth_base: String::new(),
            depth_quote: String::new(),
        };
        Book {
            bids: vec![level("99.99", "1"), level("99.97", "20")],
            asks: vec![level("100.01", "3"), level("100.03", "1")],
            updated_at_ms: 1,
        }
    }

    #[test]
    fn exponential_weights_match_the_mid_distance_formula_and_keep_raw_totals() {
        let book = weighted_book();
        let mut settings = Settings {
            levels: 2,
            units: Units::Base,
            ..Default::default()
        };
        assert_eq!(measure(&book, &settings).unwrap().percent(), Some(68.0));
        settings.weighting = Weighting::Exponential;
        let weighted = measure(&book, &settings).unwrap();
        // Relative to each best price the second level is 2 bps away: w = 1/2.
        assert!((weighted.percent().unwrap() - 51.72413793103448).abs() < 1e-10);
        assert_eq!(
            (weighted.bid, weighted.ask),
            (Decimal::from(21), Decimal::from(4))
        );
        settings.units = Units::Quote;
        let expected = 100.0 * (1099.69 - 350.045) / (1099.69 + 350.045);
        assert!((measure(&book, &settings).unwrap().percent().unwrap() - expected).abs() < 1e-10);
        settings.units = Units::Base;
        settings.half_life_bps = 0.2;
        assert!(
            measure(&book, &settings).unwrap().percent().unwrap() < -40.0,
            "nearby asks outweigh the distant bid wall"
        );
        settings.levels = 3;
        assert!(
            measure(&book, &settings).is_none(),
            "weighting must not silently reduce requested depth"
        );
    }

    #[test]
    fn exponential_is_stable_on_wide_spreads_and_bundle_price_scales() {
        let settings = Settings {
            levels: 2,
            units: Units::Quote,
            weighting: Weighting::Exponential,
            ..Default::default()
        };
        let mut book = weighted_book();
        let original = measure(&book, &settings).unwrap().percent().unwrap();
        for level in book.bids.iter_mut().chain(&mut book.asks) {
            level.price =
                (level.price.parse::<Decimal>().unwrap() * Decimal::from(1000)).to_string();
            level.size = (level.size.parse::<Decimal>().unwrap() / Decimal::from(1000)).to_string();
        }
        assert!((measure(&book, &settings).unwrap().percent().unwrap() - original).abs() < 1e-10);
        book.bids.truncate(1);
        book.asks.truncate(1);
        book.bids[0].price = "1".into();
        book.asks[0].price = "3".into();
        let narrow_kernel = Settings {
            levels: 1,
            units: Units::Base,
            half_life_bps: 0.01,
            ..settings
        };
        assert_eq!(
            measure(&book, &narrow_kernel).unwrap().percent(),
            Some(-50.0),
            "a common very small mid-distance weight must cancel, not underflow"
        );
        for bad in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            let settings = Settings {
                half_life_bps: bad,
                ..narrow_kernel
            };
            assert!(
                measure(&book, &settings)
                    .unwrap()
                    .percent()
                    .unwrap()
                    .is_finite()
            );
        }
    }

    #[test]
    fn history_does_not_mix_weighting_models_or_half_lives() {
        let market = Market::default();
        let mut state = View::default();
        let mut data = data();
        data.book = Some(weighted_book());
        let mut settings = Settings {
            levels: 2,
            units: Units::Base,
            ..Default::default()
        };
        state.observe(&market, &settings, Some(&data), 1000);
        assert_eq!(state.histories[&market].back().unwrap().values, Some(68.0));
        settings.weighting = Weighting::Exponential;
        state.observe(&market, &settings, Some(&data), 1200);
        assert_eq!(state.histories[&market].len(), 1);
        assert!((state.histories[&market][0].values.unwrap() - 51.72413793103448).abs() < 1e-10);
        settings.half_life_bps = 0.2;
        state.observe(&market, &settings, Some(&data), 1400);
        assert_eq!(state.histories[&market].len(), 1);
        assert!(state.histories[&market][0].values.unwrap() < -40.0);
        settings.weighting = Weighting::Flat;
        state.observe(&market, &settings, Some(&data), 1600);
        assert_eq!(state.histories[&market].len(), 1);
        assert_eq!(state.histories[&market][0].values, Some(68.0));
    }

    fn data() -> MarketData {
        let mut data = MarketData {
            connected: true,
            ..Default::default()
        };
        let level = |p: &str, s: &str| Level {
            price: p.into(),
            size: s.into(),
            quote_size: String::new(),
            depth_base: String::new(),
            depth_quote: String::new(),
        };
        data.set_book(
            Book {
                bids: vec![level("100", "6"), level("99", "2")],
                asks: vec![level("101", "2"), level("102", "6")],
                updated_at_ms: 1,
            },
            Exchange::Binance,
            false,
        );
        data
    }

    #[test]
    fn exact_depth_and_quote_weighting_use_cached_prefixes() {
        let data = data();
        let book = data.book.as_ref().unwrap();
        let measure = |book: &Book, levels, units| {
            super::measure(
                book,
                &Settings {
                    levels,
                    units,
                    ..Default::default()
                },
            )
        };
        assert_eq!(measure(book, 1, Units::Base).unwrap().percent(), Some(50.0));
        assert_eq!(measure(book, 2, Units::Base).unwrap().percent(), Some(0.0));
        let quote = measure(book, 1, Units::Quote).unwrap();
        assert_eq!(quote.bid, Decimal::from(600));
        assert_eq!(quote.ask, Decimal::from(202));
        assert!((quote.percent().unwrap() - 49.62593516209476).abs() < 1e-10);
        assert!(
            measure(book, 3, Units::Quote).is_none(),
            "never silently use fewer levels"
        );
        assert!(measure(book, 0, Units::Base).is_none());
        let mut without_cache = book.clone();
        for level in without_cache.bids.iter_mut().chain(&mut without_cache.asks) {
            level.depth_base.clear();
            level.depth_quote.clear();
        }
        assert_eq!(
            measure(book, 2, Units::Quote),
            measure(&without_cache, 2, Units::Quote)
        );
        let mut crossed = book.clone();
        crossed.asks[0].price = "99".into();
        assert!(measure(&crossed, 1, Units::Base).is_none());
    }

    #[test]
    fn history_resets_on_settings_change_and_keeps_disconnect_gaps() {
        let market = Market::default();
        let mut state = View::default();
        let mut settings = Settings {
            levels: 1,
            units: Units::Base,
            ..Default::default()
        };
        let data = data();
        state.observe(&market, &settings, Some(&data), 1000);
        state.observe(&market, &settings, None, 1200);
        state.observe(&market, &settings, Some(&data), 1300);
        assert_eq!(state.histories[&market].len(), 3);
        assert!(state.histories[&market][1].values.is_none());
        settings.levels = 3;
        state.observe(&market, &settings, Some(&data), 1400);
        assert_eq!(state.histories[&market].len(), 1);
        assert!(state.histories[&market][0].values.is_none());
        settings.levels = 2;
        state.observe(&market, &settings, Some(&data), 1500);
        assert_eq!(state.histories[&market].back().unwrap().values, Some(0.0));
    }

    #[test]
    fn narrow_and_wide_widgets_show_explicit_insufficient_depth_once_per_market() {
        let market = Market::default();
        let data = HashMap::from([(market.clone(), data())]);
        for width in [220.0, 640.0] {
            let ctx = egui::Context::default();
            let mut state = View::default();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, 400.0))),
                    ..Default::default()
                },
                |ui| {
                    super::ui(
                        ui,
                        &[market.clone(), market.clone()],
                        &data,
                        &HashMap::new(),
                        &Settings {
                            levels: 3,
                            ..Default::default()
                        },
                        &mut state,
                    );
                },
            );
            output.textures_delta.clear();
            let labels: Vec<_> = output
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Text(t) => Some(t.galley.text()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                labels.iter().filter(|&&label| label == "2/3").count(),
                1,
                "{labels:?}"
            );
            assert_eq!(state.histories.len(), 1);
        }
    }
}
