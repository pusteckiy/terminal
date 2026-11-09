use std::collections::{BTreeMap, HashMap, VecDeque};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use terminal_core::{Book, BookChange, Level, Market, SymbolInfo, Trade, TradeSide};

use crate::{BORDER, MUTED, MarketData, SURFACE, TEXT, orderflow, view};

const MAX_RECORDS: usize = 10_000;
const HISTORY_MS: i64 = 600_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Add,
    Update,
    Remove,
    Trade,
}

impl Kind {
    const ALL: [Self; 4] = [Self::Add, Self::Update, Self::Remove, Self::Trade];
    fn index(self) -> usize {
        self as usize
    }
    fn label(self) -> &'static str {
        match self {
            Self::Add => "ADD",
            Self::Update => "UPDATE",
            Self::Remove => "REMOVE",
            Self::Trade => "TRADE",
        }
    }
    fn color(self) -> Color32 {
        match self {
            Self::Add => Color32::from_rgb(92, 198, 255),
            Self::Update => Color32::from_rgb(255, 194, 97),
            Self::Remove => crate::RED,
            Self::Trade => crate::GREEN,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub window_secs: u32,
    pub types: [bool; 4],
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            window_secs: 60,
            types: [true; 4],
        }
    }
}

pub fn config_ui(ui: &mut egui::Ui, settings: &mut Settings) {
    ui.horizontal(|ui| {
        ui.label("Window");
        ui.add(
            egui::DragValue::new(&mut settings.window_secs)
                .range(10..=600)
                .suffix("s"),
        );
    });
    for kind in Kind::ALL {
        ui.checkbox(&mut settings.types[kind.index()], kind.label()).on_hover_text(match kind {
            Kind::Add => "An observed price level appears within known book coverage. This does not count individual order placements.",
            Kind::Update => "An existing price level changes size; both increases and decreases are included.",
            Kind::Remove => "An observed price level disappears. L2 data does not distinguish cancellation from execution.",
            Kind::Trade => "Public trade messages, including exchange aggregates where applicable.",
        });
    }
}

#[derive(Clone, Debug)]
pub struct Event {
    pub time_ms: i64,
    pub kind: Kind,
    pub side: TradeSide,
    pub price: Decimal,
    pub before: Decimal,
    pub after: Decimal,
}

impl Event {
    fn delta(&self) -> Decimal {
        self.after - self.before
    }
    fn side_label(&self) -> &'static str {
        match (self.kind, self.side) {
            (Kind::Trade, TradeSide::Buy) => "BUY",
            (Kind::Trade, TradeSide::Sell) => "SELL",
            (_, TradeSide::Buy) => "BID",
            (_, TradeSide::Sell) => "ASK",
            _ => "—",
        }
    }
}

#[derive(Default)]
pub struct History {
    pub enabled: bool,
    pub records: VecDeque<Event>,
    pub seconds: BTreeMap<i64, [u64; 4]>,
    latest_ms: i64,
}

fn number(value: &str) -> Option<Decimal> {
    value.parse().ok()
}
fn level(level: &Level) -> Option<(Decimal, Decimal)> {
    let price = number(&level.price)?;
    let size = number(&level.size)?;
    (price > Decimal::ZERO && size > Decimal::ZERO).then_some((price, size))
}

impl History {
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled && !enabled {
            *self = Self::default();
        }
        self.enabled = enabled;
    }

    fn push(&mut self, event: Event) {
        if !self.enabled || event.time_ms <= 0 {
            return;
        }
        self.latest_ms = self.latest_ms.max(event.time_ms);
        let cutoff = self.latest_ms - HISTORY_MS;
        if event.time_ms < cutoff {
            return;
        }
        self.seconds
            .entry(event.time_ms.div_euclid(1000))
            .or_default()[event.kind.index()] += 1;
        while self
            .seconds
            .first_key_value()
            .is_some_and(|(&second, _)| second < cutoff.div_euclid(1000))
        {
            self.seconds.pop_first();
        }
        let index = self
            .records
            .partition_point(|existing| existing.time_ms <= event.time_ms);
        self.records.insert(index, event);
        while self.records.len() > MAX_RECORDS
            || self.records.front().is_some_and(|e| e.time_ms < cutoff)
        {
            self.records.pop_front();
        }
    }

    fn change(
        &mut self,
        time_ms: i64,
        side: TradeSide,
        price: Decimal,
        before: Decimal,
        after: Decimal,
    ) {
        if before == after {
            return;
        }
        let kind = if before.is_zero() {
            Kind::Add
        } else if after.is_zero() {
            Kind::Remove
        } else {
            Kind::Update
        };
        self.push(Event {
            time_ms,
            kind,
            side,
            price,
            before,
            after,
        });
    }

    pub fn trade(&mut self, trade: &Trade) {
        if !self.enabled {
            return;
        }
        if let (Some(price), Some(size)) = (number(&trade.price), number(&trade.size))
            && price > Decimal::ZERO
            && size > Decimal::ZERO
            && price.checked_mul(size).is_some()
        {
            self.push(Event {
                time_ms: trade.time_ms,
                kind: Kind::Trade,
                side: trade.side,
                price,
                before: Decimal::ZERO,
                after: size,
            });
        }
    }

    pub fn delta(&mut self, book: &Book, bids: &[BookChange], asks: &[BookChange], time_ms: i64) {
        if !self.enabled || time_ms < book.updated_at_ms {
            return;
        }
        for (levels, changes, side) in [
            (&book.bids, bids, TradeSide::Buy),
            (&book.asks, asks, TradeSide::Sell),
        ] {
            if changes.is_empty() {
                continue;
            }
            let mut sizes: HashMap<_, _> = levels.iter().filter_map(level).collect();
            let boundary = levels.last().and_then(level).map(|(p, _)| p);
            for change in changes {
                let (Some(price), Some(after)) = (number(&change.price), number(&change.size))
                else {
                    continue;
                };
                if price <= Decimal::ZERO
                    || after < Decimal::ZERO
                    || price.checked_mul(after).is_none()
                {
                    continue;
                }
                let before = sizes.get(&price).copied().unwrap_or_default();
                let known = sizes.contains_key(&price)
                    || boundary.is_some_and(|last| {
                        if side == TradeSide::Buy {
                            price >= last
                        } else {
                            price <= last
                        }
                    });
                if known {
                    self.change(time_ms, side, price, before, after);
                }
                if after.is_zero() {
                    sizes.remove(&price);
                } else {
                    sizes.insert(price, after);
                }
            }
        }
    }

    pub fn snapshot(&mut self, previous: &Book, next: &Book) {
        if !self.enabled || next.updated_at_ms < previous.updated_at_ms {
            return;
        }
        for (old, new, side) in [
            (&previous.bids, &next.bids, TradeSide::Buy),
            (&previous.asks, &next.asks, TradeSide::Sell),
        ] {
            let (Some((old_last, _)), Some((new_last, _))) =
                (old.last().and_then(level), new.last().and_then(level))
            else {
                continue;
            };
            // Snapshot depth is truncated. Compare only coverage known in both books.
            let boundary = if side == TradeSide::Buy {
                old_last.max(new_last)
            } else {
                old_last.min(new_last)
            };
            let covered = |price: Decimal| {
                if side == TradeSide::Buy {
                    price >= boundary
                } else {
                    price <= boundary
                }
            };
            let old: HashMap<_, _> = old
                .iter()
                .filter_map(level)
                .filter(|(p, _)| covered(*p))
                .collect();
            let new: HashMap<_, _> = new
                .iter()
                .filter_map(level)
                .filter(|(p, _)| covered(*p))
                .collect();
            for (&price, &before) in &old {
                self.change(
                    next.updated_at_ms,
                    side,
                    price,
                    before,
                    new.get(&price).copied().unwrap_or_default(),
                );
            }
            for (&price, &after) in &new {
                if !old.contains_key(&price) {
                    self.change(next.updated_at_ms, side, price, Decimal::ZERO, after);
                }
            }
        }
    }
}

pub fn ui(
    ui: &mut egui::Ui,
    markets: &[Market],
    data: &HashMap<Market, MarketData>,
    catalogs: &HashMap<(terminal_core::Exchange, terminal_core::MarketKind), Vec<SymbolInfo>>,
    settings: &mut Settings,
) {
    let mut unique = Vec::new();
    for market in markets {
        if !unique.contains(&market) {
            unique.push(market);
        }
    }
    let now = crate::utc_now_ms();
    let start = (now.div_euclid(1000) - i64::from(settings.window_secs.clamp(10, 600)) + 1) * 1000;
    let mut buckets = BTreeMap::<i64, [u64; 4]>::new();
    let mut rows = Vec::new();
    for &market in &unique {
        if let Some(data) = data.get(market) {
            for (&second, counts) in data
                .events
                .seconds
                .range(start.div_euclid(1000)..=now.div_euclid(1000))
            {
                let bucket = buckets.entry(second).or_default();
                for i in 0..4 {
                    bucket[i] += counts[i];
                }
            }
            rows.extend(
                data.events
                    .records
                    .iter()
                    .rev()
                    .filter(|e| {
                        (start..=now).contains(&e.time_ms) && settings.types[e.kind.index()]
                    })
                    .take(500)
                    .map(|e| (market, e)),
            );
        }
    }
    rows.sort_by(|a, b| {
        b.1.time_ms
            .cmp(&a.1.time_ms)
            .then_with(|| a.0.symbol.cmp(&b.0.symbol))
    });
    rows.truncate(500);
    ui.label(
        egui::RichText::new(format!(
            "{}s · events / 1s",
            settings.window_secs.clamp(10, 600)
        ))
        .monospace()
        .size(10.0)
        .color(MUTED),
    );
    ui.horizontal_wrapped(|ui| {
        for kind in Kind::ALL {
            let count: u64 = buckets.values().map(|b| b[kind.index()]).sum();
            ui.toggle_value(
                &mut settings.types[kind.index()],
                format!("{} {count}", kind.label()),
            );
        }
    });
    let height = (ui.available_height() * 0.22)
        .clamp(45.0, 90.0)
        .min(ui.available_height());
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let plot = Rect::from_min_max(
        rect.left_top() + Vec2::new(28.0, 5.0),
        rect.right_bottom() - Vec2::new(8.0, 19.0),
    );
    let painter = ui.painter_at(rect);
    if plot.is_positive() {
        let max = buckets
            .values()
            .map(|b| {
                (0..4)
                    .filter(|&i| settings.types[i])
                    .map(|i| b[i])
                    .sum::<u64>()
            })
            .max()
            .unwrap_or(1)
            .max(1);
        for (&second, counts) in &buckets {
            let left = plot.left()
                + ((second * 1000 - start) as f64 / (now - start) as f64) as f32 * plot.width();
            let width = (1000.0 / (now - start) as f32 * plot.width() - 0.4).max(0.3);
            let mut total = 0;
            for kind in Kind::ALL {
                if settings.types[kind.index()] {
                    let value = counts[kind.index()];
                    let top = plot.bottom() - (total + value) as f32 / max as f32 * plot.height();
                    let bottom = plot.bottom() - total as f32 / max as f32 * plot.height();
                    painter.with_clip_rect(plot).rect_filled(
                        Rect::from_min_max(Pos2::new(left, top), Pos2::new(left + width, bottom)),
                        0.0,
                        kind.color().gamma_multiply(0.7),
                    );
                    total += value;
                }
            }
        }
        for (value, y) in [(max, plot.top() + 5.0), (0, plot.bottom())] {
            painter.text(
                Pos2::new(plot.left() - 5.0, y),
                Align2::RIGHT_CENTER,
                value.to_string(),
                FontId::monospace(9.0),
                MUTED,
            );
        }
        for i in 0..=3 {
            let time = start + (now - start) * i / 3;
            painter.text(
                Pos2::new(
                    plot.left() + plot.width() * i as f32 / 3.0,
                    rect.bottom() - 5.0,
                ),
                if i == 0 {
                    Align2::LEFT_CENTER
                } else if i == 3 {
                    Align2::RIGHT_CENTER
                } else {
                    Align2::CENTER_CENTER
                },
                &view::trade_utc(time, true)[3..],
                FontId::monospace(9.0),
                MUTED,
            );
        }
        if let Some(pointer) = response.hover_pos().filter(|p| plot.contains(*p)) {
            let second = (start
                + ((pointer.x - plot.left()) / plot.width() * (now - start) as f32) as i64)
                .div_euclid(1000);
            response.on_hover_ui_at_pointer(|ui| {
                ui.monospace(format!("{} UTC", view::trade_utc(second * 1000, true)));
                let counts = buckets.get(&second).copied().unwrap_or_default();
                for kind in Kind::ALL {
                    if settings.types[kind.index()] {
                        ui.colored_label(
                            kind.color(),
                            format!("{} {}", kind.label(), counts[kind.index()]),
                        );
                    }
                }
            });
        }
    }
    let columns = [0.0, 0.22, 0.34, 0.51, 0.62, 0.81, 1.0];
    let (header, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 21.0), Sense::hover());
    paint_row(
        ui,
        header,
        &columns,
        ["UTC", "VENUE", "EVENT", "SIDE", "PRICE", "Δ SIZE"],
        None,
    );
    if rows.is_empty() {
        ui.weak("Waiting for matching market events…");
    }
    egui::ScrollArea::vertical().id_salt("market-events").auto_shrink([false,false]).show_rows(ui,23.0,rows.len(),|ui,range| {
        for index in range {
            let (market,event)=rows[index];
            let info=orderflow::symbol_info(market,catalogs);
            let (base,multiplier)=info.map_or(("base",1),|i|orderflow::base_units(&i.base,market.kind));
            let multiplier=Decimal::from(multiplier);
            let price=event.price.checked_div(multiplier).unwrap_or(event.price);
            let delta=event.delta().checked_mul(multiplier).unwrap_or(event.delta());
            let time=view::trade_utc(event.time_ms,false);
            let delta_text=format!("{}{}",if event.kind!=Kind::Trade && delta>Decimal::ZERO {"+"}else{""},view::quote_size_text(&delta.normalize().to_string()));
            let (rect,response)=ui.allocate_exact_size(Vec2::new(ui.available_width(),23.0),Sense::hover());
            if index%2==0 || response.hovered() {ui.painter().rect_filled(rect,0.0,if response.hovered(){crate::HOVER}else{SURFACE});}
            paint_row(ui,rect,&columns,[&time[3..],view::venue_code(market.exchange),event.kind.label(),event.side_label(),&price.normalize().to_string(),&delta_text],Some(event));
            response.on_hover_ui_at_pointer(|ui| {
                ui.colored_label(event.kind.color(),format!("{} · {}",event.kind.label(),event.side_label()));
                ui.label(format!("{} {} {}",market.exchange.label(),market.kind.label(),market.symbol));
                ui.monospace(format!("{time} UTC · {}",price.normalize()));
                ui.monospace(format!("Δ {} {base}",delta.normalize()));
                let quote=event.price.checked_mul(event.delta()).map(|n|n.normalize().to_string()).unwrap_or_else(||"—".into());
                ui.monospace(format!("Quote {quote} {}",info.map_or("quote",|i|i.quote.as_str())));
                if event.kind!=Kind::Trade {
                    ui.monospace(format!("{} → {} {base}",event.before.checked_mul(multiplier).map(|v|v.normalize().to_string()).unwrap_or_else(||"—".into()),event.after.checked_mul(multiplier).map(|v|v.normalize().to_string()).unwrap_or_else(||"—".into())));
                }
                if event.kind==Kind::Remove {ui.weak("Observed L2 removal; cancellation or execution cannot be distinguished.");}
            });
        }
    });
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(100));
}

fn paint_row(
    ui: &egui::Ui,
    rect: Rect,
    columns: &[f32; 7],
    labels: [&str; 6],
    event: Option<&Event>,
) {
    for (index, label) in labels.into_iter().enumerate() {
        let cell = Rect::from_min_max(
            Pos2::new(rect.left() + rect.width() * columns[index], rect.top()),
            Pos2::new(
                rect.left() + rect.width() * columns[index + 1],
                rect.bottom(),
            ),
        )
        .shrink2(Vec2::new(3.0, 0.0));
        let color = event.map_or(MUTED, |e| {
            if index == 2 {
                e.kind.color()
            } else if index == 3 {
                view::trade_color(e.side, MUTED)
            } else {
                TEXT
            }
        });
        ui.painter().with_clip_rect(cell).text(
            if index >= 4 {
                cell.right_center()
            } else {
                cell.left_center()
            },
            if index >= 4 {
                Align2::RIGHT_CENTER
            } else {
                Align2::LEFT_CENTER
            },
            label,
            FontId::monospace(10.0),
            color,
        );
    }
    ui.painter().line_segment(
        [rect.left_bottom(), rect.right_bottom()],
        Stroke::new(0.5, BORDER),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(bids: &[(&str, &str)], asks: &[(&str, &str)], time: i64) -> Book {
        let levels = |items: &[(&str, &str)]| {
            items
                .iter()
                .map(|&(p, s)| Level {
                    price: p.into(),
                    size: s.into(),
                    quote_size: String::new(),
                    depth_base: String::new(),
                    depth_quote: String::new(),
                })
                .collect()
        };
        Book {
            bids: levels(bids),
            asks: levels(asks),
            updated_at_ms: time,
        }
    }

    #[test]
    fn snapshot_changes_exclude_truncated_and_newly_discovered_depth() {
        let mut history = History {
            enabled: true,
            ..Default::default()
        };
        let old = book(
            &[("100", "2"), ("99", "3"), ("98", "4")],
            &[("101", "2"), ("102", "3"), ("103", "4")],
            1000,
        );
        let next = book(
            &[("100.5", "5"), ("100", "4"), ("98.5", "1")],
            &[("100.5", "5"), ("101", "4"), ("102.5", "1")],
            1100,
        );
        history.snapshot(&old, &next);
        for (kind, count) in [(Kind::Add, 4), (Kind::Update, 2), (Kind::Remove, 2)] {
            assert_eq!(
                history.records.iter().filter(|e| e.kind == kind).count(),
                count
            );
        }
        assert!(
            !history
                .records
                .iter()
                .any(|e| e.price == Decimal::from(98) || e.price == Decimal::from(103))
        );
        history.records.clear();
        history.seconds.clear();
        let deeper = book(
            &[("100", "2"), ("99", "3"), ("98", "4"), ("97", "9")],
            &[("101", "2"), ("102", "3"), ("103", "4"), ("104", "9")],
            1200,
        );
        history.snapshot(&old, &deeper);
        assert!(
            history.records.is_empty(),
            "discovering depth is not a new order event"
        );
        history.snapshot(&old, &old);
        assert!(
            history.records.is_empty(),
            "same snapshot must not duplicate events"
        );
    }

    #[test]
    fn deltas_use_previous_sizes_and_noops_are_not_events() {
        let mut history = History {
            enabled: true,
            ..Default::default()
        };
        let old = book(&[("100", "2"), ("99", "3")], &[("101", "4")], 1000);
        let changes = [
            ("100.0", "2"),
            ("100", "5"),
            ("99.5", "2"),
            ("99", "0"),
            ("98", "4"),
            ("100", "-1"),
            ("invalid", "1"),
        ]
        .map(|(price, size)| BookChange {
            price: price.into(),
            size: size.into(),
        });
        history.delta(&old, &changes, &[], 1100);
        assert_eq!(history.records.len(), 3);
        assert_eq!(history.records[0].kind, Kind::Update);
        assert_eq!(history.records[0].delta(), Decimal::from(3));
        assert_eq!(history.records[1].kind, Kind::Add);
        assert_eq!(history.records[2].kind, Kind::Remove);
        assert_eq!(history.records[2].delta(), Decimal::from(-3));
        history.delta(&old, &changes, &[], 999);
        assert_eq!(history.records.len(), 3, "stale deltas must be ignored");
    }

    #[test]
    fn counts_survive_tape_cap_and_late_trades_remain_ordered() {
        let mut history = History {
            enabled: true,
            ..Default::default()
        };
        for i in 0..MAX_RECORDS + 5 {
            history.trade(&Trade {
                time_ms: 10_000 + i as i64,
                price: "0.000004".into(),
                size: "1000".into(),
                side: TradeSide::Buy,
            });
        }
        assert_eq!(history.records.len(), MAX_RECORDS);
        assert_eq!(
            history.seconds.values().map(|b| b[3]).sum::<u64>(),
            (MAX_RECORDS + 5) as u64
        );
        history.trade(&Trade {
            time_ms: 11_000,
            price: "1".into(),
            size: "2".into(),
            side: TradeSide::Sell,
        });
        assert!(
            history
                .records
                .iter()
                .zip(history.records.iter().skip(1))
                .all(|(a, b)| a.time_ms <= b.time_ms)
        );
        history.trade(&Trade {
            time_ms: 1_000_000,
            price: "1".into(),
            size: "2".into(),
            side: TradeSide::Sell,
        });
        assert_eq!(history.records.len(), 1);
        assert_eq!(history.seconds.len(), 1);
        history.trade(&Trade {
            time_ms: 1,
            price: "1".into(),
            size: "2".into(),
            side: TradeSide::Sell,
        });
        assert_eq!(
            history.records.len(),
            1,
            "late events outside retention stay excluded"
        );
        history.set_enabled(false);
        assert!(history.records.is_empty() && history.seconds.is_empty());
        history.trade(&Trade {
            time_ms: 1_000_001,
            price: "1".into(),
            size: "2".into(),
            side: TradeSide::Buy,
        });
        assert!(history.records.is_empty());
    }

    #[test]
    fn responsive_ui_deduplicates_sources_and_has_no_permanent_remove_caption() {
        let market = Market::default();
        let mut data = MarketData::default();
        data.events.set_enabled(true);
        data.push_trade(Trade {
            time_ms: crate::utc_now_ms(),
            price: "100".into(),
            size: "2".into(),
            side: TradeSide::Buy,
        });
        let data = HashMap::from([(market.clone(), data)]);
        for width in [220.0, 640.0] {
            let ctx = egui::Context::default();
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
                        &mut Settings::default(),
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
            assert!(
                labels.contains(&"TRADE 1"),
                "duplicate sources must not duplicate counts: {labels:?}"
            );
            assert!(
                !labels.iter().any(
                    |label| label.contains("REMOVE =") || label.contains("Observed L2 removal")
                ),
                "explanations belong in hover only"
            );
        }
    }
}
