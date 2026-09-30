use std::collections::HashMap;

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use terminal_core::{
    Account, Book, Candle, Exchange, Level, Market, OwnOrder, Position, Trade, TradeSide,
};

use crate::{CompareMode, MAX_TAPE_TRADES, MarketData, TradePoint};

const PANEL: Color32 = crate::SURFACE;
const GRID: Color32 = crate::BORDER;
const TEXT: Color32 = crate::TEXT;
const MUTED: Color32 = crate::MUTED;
const GREEN: Color32 = crate::GREEN;
const RED: Color32 = crate::RED;
const TAPE_ROW_HEIGHT: f32 = 21.0;

fn trade_color(side: TradeSide, fallback: Color32) -> Color32 {
    match side {
        TradeSide::Buy => GREEN,
        TradeSide::Sell => RED,
        TradeSide::Unknown => fallback,
    }
}

fn trade_side_label(side: TradeSide) -> &'static str {
    match side {
        TradeSide::Buy => "BUY",
        TradeSide::Sell => "SELL",
        TradeSide::Unknown => "TRADE",
    }
}

fn venue_code(exchange: Exchange) -> &'static str {
    match exchange {
        Exchange::Binance => "BIN",
        Exchange::Okx => "OKX",
        Exchange::Bybit => "BYB",
        Exchange::Hyperliquid => "HYP",
        Exchange::Gate => "GAT",
        Exchange::Lighter => "LTR",
        Exchange::Bitget => "BGT",
        Exchange::Aster => "AST",
        Exchange::Bitunix => "BUX",
    }
}

fn trade_utc(time_ms: i64, compact: bool) -> String {
    let day_ms = time_ms.rem_euclid(86_400_000);
    let hours = day_ms / 3_600_000;
    let minutes = day_ms / 60_000 % 60;
    let seconds = day_ms / 1_000 % 60;
    if compact {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}.{:03}", day_ms % 1_000)
    }
}

pub fn tape_ui(ui: &mut egui::Ui, sources: &[Market], data: &HashMap<Market, MarketData>) {
    let width = ui.available_width().max(1.0);
    let compact = width < 350.0;
    let (price_right, size_right, venue_left) = if compact {
        (width * 0.48, width * 0.72, width * 0.75)
    } else {
        (width * 0.42, width * 0.63, width * 0.66)
    };
    let (header, _) = ui.allocate_exact_size(Vec2::new(width, 27.0), Sense::hover());
    let painter = ui.painter_at(header);
    painter.rect_filled(header, 0.0, PANEL);
    for (x, align, label) in [
        (header.left() + 10.0, Align2::LEFT_CENTER, "UTC"),
        (header.left() + price_right, Align2::RIGHT_CENTER, "PRICE"),
        (header.left() + size_right, Align2::RIGHT_CENTER, "SIZE"),
        (header.left() + venue_left, Align2::LEFT_CENTER, "VENUE"),
    ] {
        painter.text(
            Pos2::new(x, header.center().y),
            align,
            label,
            FontId::monospace(10.0),
            MUTED,
        );
    }
    painter.line_segment(
        [header.left_bottom(), header.right_bottom()],
        Stroke::new(1.0, GRID),
    );

    let mut trades: Vec<(&Market, &Trade)> = sources
        .iter()
        .filter_map(|market| data.get(market).map(|data| (market, data)))
        .flat_map(|(market, data)| data.trades.iter().map(move |trade| (market, trade)))
        .collect();
    trades.sort_by_key(|item| std::cmp::Reverse(item.1.time_ms));
    trades.truncate(MAX_TAPE_TRADES);
    if trades.is_empty() {
        let (body, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
        ui.painter_at(body).text(
            body.center(),
            Align2::CENTER_CENTER,
            "Waiting for live trades…",
            FontId::proportional(13.0),
            MUTED,
        );
        return;
    }

    egui::ScrollArea::vertical()
        .id_salt("trades-tape")
        .auto_shrink([false, false])
        .show_rows(ui, TAPE_ROW_HEIGHT, trades.len(), |ui, rows| {
            for index in rows {
                let (market, trade) = trades[index];
                let (rect, response) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), TAPE_ROW_HEIGHT),
                    Sense::hover(),
                );
                let painter = ui.painter_at(rect);
                if response.hovered() {
                    painter.rect_filled(rect, 0.0, crate::HOVER);
                } else if index % 2 == 0 {
                    painter.rect_filled(rect, 0.0, PANEL);
                }
                let y = rect.center().y;
                let font = FontId::monospace(11.0);
                let side_color = trade_color(trade.side, TEXT);
                painter.text(
                    Pos2::new(rect.left() + 10.0, y),
                    Align2::LEFT_CENTER,
                    trade_utc(trade.time_ms, compact),
                    font.clone(),
                    MUTED,
                );
                painter.text(
                    Pos2::new(rect.left() + price_right, y),
                    Align2::RIGHT_CENTER,
                    &trade.price,
                    font.clone(),
                    side_color,
                );
                painter.text(
                    Pos2::new(rect.left() + size_right, y),
                    Align2::RIGHT_CENTER,
                    &trade.size,
                    font.clone(),
                    side_color,
                );
                let venue = if compact {
                    format!("{} {}", venue_code(market.exchange), market.kind.label())
                } else {
                    format!(
                        "{} {} {}",
                        market.exchange.label(),
                        market.kind.label(),
                        market.symbol
                    )
                };
                painter.text(
                    Pos2::new(rect.left() + venue_left, y),
                    Align2::LEFT_CENTER,
                    venue,
                    font,
                    MUTED,
                );
                response.on_hover_text(format!(
                    "{} · {} {} {} · {} @ {} · {} UTC",
                    trade_side_label(trade.side),
                    market.exchange.label(),
                    market.kind.label(),
                    market.symbol,
                    trade.size,
                    trade.price,
                    trade_utc(trade.time_ms, false),
                ));
            }
        });
}

#[derive(Default)]
pub struct ChartView {
    visible: usize,
    offset: usize,
    drag_remainder: f32,
    y_axis: YAxisView,
}

#[derive(Default)]
pub struct YAxisView {
    manual: Option<(f64, f64)>,
}

impl YAxisView {
    fn pan(&mut self, bounds: (f64, f64), delta_y: f32, height: f32) {
        if delta_y.abs() < 0.01 {
            return;
        }
        let shift = f64::from(delta_y / height) * (bounds.1 - bounds.0);
        self.manual = Some((bounds.0 + shift, bounds.1 + shift));
    }

    fn zoom(&mut self, bounds: (f64, f64), scroll: f32, anchor: f64) {
        let span = bounds.1 - bounds.0;
        let value = bounds.0 + anchor * span;
        let min_span = value.abs().max(1.0) * 1e-10;
        let next_span = (span * (-f64::from(scroll) * 0.005).exp()).max(min_span);
        let next = (
            value - anchor * next_span,
            value + (1.0 - anchor) * next_span,
        );
        if next.0.is_finite() && next.1.is_finite() && next.1 > next.0 {
            self.manual = Some(next);
        }
    }

    fn interact(
        &mut self,
        ui: &mut egui::Ui,
        response: &egui::Response,
        rect: Rect,
        plot: Rect,
        auto: (f64, f64),
        wheel_on_plot: bool,
    ) -> (f64, f64) {
        let axis = Rect::from_min_max(
            Pos2::new(plot.right(), plot.top()),
            Pos2::new(rect.right(), plot.bottom()),
        );
        let active = plot.union(axis);
        let bounds = self.manual.unwrap_or(auto);
        if response.dragged_by(egui::PointerButton::Primary)
            && ui.input(|input| {
                input
                    .pointer
                    .press_origin()
                    .is_some_and(|p| active.contains(p))
            })
        {
            self.pan(
                bounds,
                ui.input(|input| input.pointer.delta().y),
                plot.height(),
            );
        }
        if let Some(pointer) = response.hover_pos().filter(|p| active.contains(*p)) {
            let on_axis = axis.contains(pointer);
            let (scroll, shift) = ui.input(|input| {
                let delta = input.smooth_scroll_delta;
                let shift = input.modifiers.shift;
                (
                    if shift && delta.y.abs() < 0.01 {
                        delta.x
                    } else {
                        delta.y
                    },
                    shift,
                )
            });
            if scroll.abs() > 0.01 && (on_axis || wheel_on_plot || shift) {
                let anchor = f64::from((plot.bottom() - pointer.y) / plot.height()).clamp(0.0, 1.0);
                self.zoom(self.manual.unwrap_or(auto), scroll, anchor);
            }
            if on_axis {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                response
                    .clone()
                    .on_hover_text("Drag to move price · Scroll to zoom · Double-click to reset");
                if response.double_clicked() {
                    self.manual = None;
                }
            }
        }
        self.manual.unwrap_or(auto)
    }
}

#[derive(Default)]
pub struct BookView {
    scroll_px: f32,
    inspected: Option<BookSelection>,
}

const BOOK_ROW_HEIGHT: f32 = 18.0;
const BOOK_SPREAD_HALF_HEIGHT: f32 = 12.0;
const BOOK_INSPECTOR_HEIGHT: f32 = 72.0;

fn book_scroll_bounds(body_height: f32, asks: usize, bids: usize) -> (f32, f32) {
    let half_height = body_height / 2.0;
    let min = (half_height - BOOK_SPREAD_HALF_HEIGHT - bids as f32 * BOOK_ROW_HEIGHT).min(0.0);
    let max = (asks as f32 * BOOK_ROW_HEIGHT - half_height + BOOK_SPREAD_HALF_HEIGHT).max(0.0);
    (min, max)
}

fn price_text(price: f64) -> String {
    if price >= 1000.0 {
        format!("{price:.2}")
    } else if price >= 1.0 {
        format!("{price:.4}")
    } else {
        format!("{price:.6}")
    }
}

fn compact_decimal(value: &str) -> &str {
    if value.contains('.') {
        value.trim_end_matches('0').trim_end_matches('.')
    } else {
        value
    }
}

fn spread_percent_text(bid: f64, ask: f64) -> Option<String> {
    let midpoint = (ask + bid) / 2.0;
    if !midpoint.is_finite() || midpoint <= 0.0 {
        return None;
    }
    let percent = (ask - bid) / midpoint * 100.0;
    if percent != 0.0 && percent.abs() < 0.000_001 {
        Some(format!("{percent:.2e}%"))
    } else {
        Some(format!("{}%", compact_decimal(&format!("{percent:.6}"))))
    }
}

fn quote_size_text(value: &str) -> String {
    let Ok(amount) = value.parse::<f64>() else {
        return value.to_owned();
    };
    if !amount.is_finite() {
        return value.to_owned();
    }
    if amount >= 1_000_000_000.0 {
        format!("{:.2}B", amount / 1_000_000_000.0)
    } else if amount >= 1_000_000.0 {
        format!("{:.2}M", amount / 1_000_000.0)
    } else if amount >= 10_000.0 {
        format!("{:.2}K", amount / 1_000.0)
    } else if amount > 0.0 && amount < 0.000_001 {
        format!("{amount:.2e}")
    } else {
        compact_decimal(&price_text(amount)).to_owned()
    }
}

fn time_text(timestamp: i64) -> String {
    let minutes = timestamp.div_euclid(60).rem_euclid(1440);
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

pub fn chart_ui(
    ui: &mut egui::Ui,
    candles: &[Candle],
    view: &mut ChartView,
    orders: &[(&Account, &OwnOrder)],
    positions: &[(&Account, &Position)],
) {
    let available = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(available.x.max(1.0), available.y.max(1.0)),
        Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, PANEL);
    if !orders.is_empty() || !positions.is_empty() {
        let position = positions.first().map(|(_, position)| {
            format!(
                " · {} {} @ {} · PnL {}",
                if position.size.starts_with('-') {
                    "SHORT"
                } else {
                    "LONG"
                },
                compact_decimal(&position.size),
                compact_decimal(&position.entry_price),
                compact_decimal(&position.unrealized_pnl)
            )
        });
        painter
            .with_clip_rect(Rect::from_min_max(
                Pos2::new(rect.left() + 12.0, rect.top()),
                Pos2::new(rect.right() - 12.0, rect.top() + 26.0),
            ))
            .text(
                Pos2::new(rect.left() + 14.0, rect.top() + 12.0),
                Align2::LEFT_CENTER,
                format!(
                    "OWN  {} orders · {} positions{}",
                    orders.len(),
                    positions.len(),
                    position.unwrap_or_default()
                ),
                FontId::monospace(10.0),
                MUTED,
            );
    }
    if candles.is_empty() {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "Waiting for candle history…",
            FontId::proportional(13.0),
            MUTED,
        );
        return;
    }

    let plot = Rect::from_min_max(
        Pos2::new(
            rect.left() + 14.0,
            rect.top()
                + if orders.is_empty() && positions.is_empty() {
                    12.0
                } else {
                    29.0
                },
        ),
        Pos2::new(rect.right() - 76.0, rect.bottom() - 31.0),
    );
    if plot.width() < 80.0 || plot.height() < 80.0 {
        return;
    }

    if view.visible == 0 {
        view.visible = 90;
    }
    if response.hovered()
        && response.hover_pos().is_some_and(|p| plot.contains(p))
        && !ui.input(|input| input.modifiers.shift)
    {
        let scroll = ui.input(|input| input.smooth_scroll_delta.y);
        if scroll.abs() > 0.5 {
            let change = if scroll > 0.0 { -5 } else { 5 };
            view.visible = (view.visible as isize + change).clamp(20, 220) as usize;
        }
    }
    view.offset = view.offset.min(candles.len().saturating_sub(1));
    let step = plot.width() / view.visible as f32;
    if response.dragged_by(egui::PointerButton::Primary)
        && ui.input(|input| {
            input
                .pointer
                .press_origin()
                .is_some_and(|p| plot.contains(p))
        })
    {
        view.drag_remainder += ui.input(|input| input.pointer.delta().x);
        let moved = (view.drag_remainder / step).trunc() as isize;
        if moved != 0 {
            view.offset = (view.offset as isize + moved)
                .clamp(0, candles.len().saturating_sub(1) as isize)
                as usize;
            view.drag_remainder -= moved as f32 * step;
        }
    } else {
        view.drag_remainder = 0.0;
    }

    let end = candles.len().saturating_sub(view.offset);
    let start = end.saturating_sub(view.visible);
    let visible = &candles[start..end];
    if visible.is_empty() {
        return;
    }
    let high = visible
        .iter()
        .map(|c| c.high)
        .fold(f64::NEG_INFINITY, f64::max);
    let low = visible.iter().map(|c| c.low).fold(f64::INFINITY, f64::min);
    let padding = ((high - low) * 0.07).max(high.abs() * 0.0001);
    let price_area = Rect::from_min_max(
        plot.min,
        Pos2::new(plot.right(), plot.top() + plot.height() * 0.76),
    );
    let volume_area =
        Rect::from_min_max(Pos2::new(plot.left(), price_area.bottom() + 12.0), plot.max);
    let (floor, ceiling) = view.y_axis.interact(
        ui,
        &response,
        rect,
        price_area,
        (low - padding, high + padding),
        false,
    );
    let price_painter = painter.with_clip_rect(price_area);
    let volume_painter = painter.with_clip_rect(volume_area);
    let y_for = |price: f64| {
        price_area.bottom() - ((price - floor) / (ceiling - floor)) as f32 * price_area.height()
    };

    for line in 0..=4 {
        let y = price_area.top() + price_area.height() * line as f32 / 4.0;
        painter.line_segment(
            [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            Stroke::new(1.0, GRID),
        );
        let price = ceiling - (ceiling - floor) * line as f64 / 4.0;
        painter.text(
            Pos2::new(rect.right() - 10.0, y),
            Align2::RIGHT_CENTER,
            price_text(price),
            FontId::monospace(10.0),
            MUTED,
        );
    }
    painter.line_segment(
        [
            Pos2::new(plot.left(), volume_area.top()),
            Pos2::new(plot.right(), volume_area.top()),
        ],
        Stroke::new(1.0, GRID),
    );
    painter.text(
        Pos2::new(plot.left() + 3.0, volume_area.top() + 4.0),
        Align2::LEFT_TOP,
        "VOL",
        FontId::monospace(9.0),
        MUTED,
    );

    let max_volume = visible
        .iter()
        .map(|c| c.volume)
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let bar_width = (step * 0.65).clamp(1.0, 15.0);
    for (index, candle) in visible.iter().enumerate() {
        let x = plot.left() + (index as f32 + 0.5) * step;
        let color = if candle.close >= candle.open {
            GREEN
        } else {
            RED
        };
        price_painter.line_segment(
            [
                Pos2::new(x, y_for(candle.high)),
                Pos2::new(x, y_for(candle.low)),
            ],
            Stroke::new(1.0, color),
        );
        let top = y_for(candle.open.max(candle.close));
        let bottom = y_for(candle.open.min(candle.close)).max(top + 1.5);
        price_painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(x - bar_width / 2.0, top),
                Pos2::new(x + bar_width / 2.0, bottom),
            ),
            0.0,
            color,
        );
        let volume_height = (candle.volume / max_volume) as f32 * (volume_area.height() - 12.0);
        volume_painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(x - bar_width / 2.0, volume_area.bottom() - volume_height),
                Pos2::new(x + bar_width / 2.0, volume_area.bottom()),
            ),
            0.0,
            color.gamma_multiply(0.45),
        );
    }

    let pointer = response.hover_pos().filter(|p| price_area.contains(*p));
    let mut hovered_order: Option<(f32, &Account, &OwnOrder, f32)> = None;
    for (account, order) in orders {
        let Ok(price) = order.price.parse::<f64>() else {
            continue;
        };
        if price < floor || price > ceiling {
            continue;
        }
        let y = y_for(price);
        let color = if order.side == TradeSide::Buy {
            GREEN
        } else {
            RED
        };
        price_painter.line_segment(
            [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            Stroke::new(1.0, color.gamma_multiply(0.35)),
        );
        price_painter.rect_filled(
            Rect::from_center_size(Pos2::new(plot.right() - 2.0, y), Vec2::splat(4.0)),
            1.0,
            color,
        );
        if let Some(pointer) = pointer {
            let distance = (pointer.y - y).abs();
            if distance <= 5.0
                && hovered_order
                    .as_ref()
                    .is_none_or(|(best, _, _, _)| distance < *best)
            {
                hovered_order = Some((distance, account, order, y));
            }
        }
    }
    for (account, position) in positions {
        let Ok(price) = position.entry_price.parse::<f64>() else {
            continue;
        };
        if price < floor || price > ceiling {
            continue;
        }
        let y = y_for(price);
        let color = Color32::from_rgb(94, 193, 255);
        price_painter.line_segment(
            [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            Stroke::new(1.3, color),
        );
        price_painter.text(
            Pos2::new(plot.right() - 4.0, y - 2.0),
            Align2::RIGHT_BOTTOM,
            format!(
                "{} {} · {}",
                if position.size.starts_with('-') {
                    "SHORT"
                } else {
                    "LONG"
                },
                compact_decimal(&position.size),
                &account.address[account.address.len() - 4..]
            ),
            FontId::monospace(10.0),
            color,
        );
    }

    if let Some((_, account, order, y)) = hovered_order {
        let color = trade_color(order.side, MUTED);
        price_painter.line_segment(
            [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            Stroke::new(1.4, color),
        );
        let header = Rect::from_min_max(rect.min, Pos2::new(rect.right(), price_area.top()));
        painter.rect_filled(header, 0.0, PANEL);
        painter.with_clip_rect(header).text(
            Pos2::new(rect.left() + 14.0, rect.top() + 12.0),
            Align2::LEFT_CENTER,
            format!(
                "ORDER {} {} × {} · …{}",
                trade_side_label(order.side),
                order.price,
                order.size,
                &account.address[account.address.len().saturating_sub(4)..]
            ),
            FontId::monospace(10.0),
            color,
        );
    }

    for tick in 0..=4 {
        let index = ((visible.len() - 1) * tick / 4).min(visible.len() - 1);
        let x = plot.left() + (index as f32 + 0.5) * step;
        painter.text(
            Pos2::new(x, plot.bottom() + 11.0),
            Align2::CENTER_CENTER,
            time_text(visible[index].time),
            FontId::monospace(10.0),
            MUTED,
        );
    }

    if let Some(pointer) = response
        .hover_pos()
        .filter(|pointer| price_area.contains(*pointer))
    {
        let index = (((pointer.x - plot.left()) / step).floor() as usize).min(visible.len() - 1);
        let x = plot.left() + (index as f32 + 0.5) * step;
        painter.line_segment(
            [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
            Stroke::new(1.0, MUTED),
        );
        painter.line_segment(
            [
                Pos2::new(plot.left(), pointer.y),
                Pos2::new(plot.right(), pointer.y),
            ],
            Stroke::new(1.0, MUTED),
        );
        let c = &visible[index];
        painter.text(
            Pos2::new(plot.left() + 8.0, plot.top() + 8.0),
            Align2::LEFT_TOP,
            format!(
                "{} UTC     O {}   H {}   L {}   C {}",
                time_text(c.time),
                price_text(c.open),
                price_text(c.high),
                price_text(c.low),
                price_text(c.close)
            ),
            FontId::monospace(10.0),
            TEXT,
        );
    }
}

pub fn book_ui(
    ui: &mut egui::Ui,
    book: Option<&Book>,
    view: &mut BookView,
    orders: &[(&Account, &OwnOrder)],
) {
    let size = ui.available_size();
    let (rect, _) =
        ui.allocate_exact_size(Vec2::new(size.x.max(1.0), size.y.max(1.0)), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, PANEL);
    let Some(book) = book else {
        if view.inspected.take().is_some() {
            view.scroll_px -= BOOK_INSPECTOR_HEIGHT / 2.0;
        }
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            "Waiting for order book…",
            FontId::proportional(13.0),
            MUTED,
        );
        return;
    };
    if view
        .inspected
        .is_some_and(|selection| match selection.side {
            BookSide::Ask => selection.index >= book.asks.len(),
            BookSide::Bid => selection.index >= book.bids.len(),
        })
    {
        view.inspected = None;
        view.scroll_px -= BOOK_INSPECTOR_HEIGHT / 2.0;
    }
    if rect.width() < 120.0 || rect.height() < 140.0 {
        return;
    }

    let left = rect.left() + 12.0;
    let right = rect.right() - 12.0;
    let value_width = book_value_width(right - left);
    let base_right = right - value_width - 6.0;
    painter.text(
        Pos2::new(left, rect.top() + 16.0),
        Align2::LEFT_CENTER,
        "PRICE",
        FontId::monospace(10.0),
        MUTED,
    );
    if !orders.is_empty() {
        painter.text(
            Pos2::new(left + 48.0, rect.top() + 16.0),
            Align2::LEFT_CENTER,
            format!("OWN {}", orders.len()),
            FontId::monospace(9.0),
            Color32::from_rgb(94, 193, 255),
        );
    }
    painter.text(
        Pos2::new(base_right, rect.top() + 16.0),
        Align2::RIGHT_CENTER,
        "BASE SIZE",
        FontId::monospace(10.0),
        MUTED,
    );
    painter.text(
        Pos2::new(right, rect.top() + 16.0),
        Align2::RIGHT_CENTER,
        "QUOTE SIZE",
        FontId::monospace(10.0),
        MUTED,
    );

    let pointer = ui.input(|input| input.pointer.hover_pos());
    if view.inspected.is_some() && !pointer.is_some_and(|pointer| rect.contains(pointer)) {
        view.inspected = None;
        view.scroll_px -= BOOK_INSPECTOR_HEIGHT / 2.0;
    }
    let footer_height = if view.inspected.is_some() {
        BOOK_INSPECTOR_HEIGHT
    } else {
        0.0
    };
    let body = Rect::from_min_max(
        Pos2::new(left, rect.top() + 26.0),
        Pos2::new(right, rect.bottom() - footer_height),
    );
    let footer = Rect::from_min_max(Pos2::new(rect.left(), body.bottom()), rect.right_bottom());
    if body.height() < 35.0 {
        return;
    }

    let (min_scroll, max_scroll) =
        book_scroll_bounds(body.height(), book.asks.len(), book.bids.len());
    view.scroll_px = view.scroll_px.clamp(min_scroll, max_scroll);
    if let Some(pointer) = pointer
        && body.contains(pointer)
    {
        let scroll = ui.input(|input| input.smooth_scroll_delta.y);
        view.scroll_px = (view.scroll_px + scroll).clamp(min_scroll, max_scroll);
    }
    let center_y = body.center().y + view.scroll_px;
    let best_ask_y = center_y - BOOK_SPREAD_HALF_HEIGHT - BOOK_ROW_HEIGHT / 2.0;
    let best_bid_y = center_y + BOOK_SPREAD_HALF_HEIGHT + BOOK_ROW_HEIGHT / 2.0;
    let hovered =
        pointer.and_then(|pointer| hovered_book_level(book, body, pointer, best_ask_y, best_bid_y));
    if let Some(selection) = hovered {
        if view.inspected.is_none() {
            view.scroll_px += BOOK_INSPECTOR_HEIGHT / 2.0;
            ui.ctx().request_repaint();
        }
        view.inspected = Some(selection);
    }
    let selected = view.inspected;

    let visible = |y: f32| {
        y - BOOK_ROW_HEIGHT / 2.0 >= body.top() && y + BOOK_ROW_HEIGHT / 2.0 <= body.bottom()
    };
    let mut max_size = 0.000_001_f64;
    for (index, level) in book.asks.iter().enumerate() {
        let y = best_ask_y - index as f32 * BOOK_ROW_HEIGHT;
        if visible(y) {
            max_size = max_size.max(level.size.parse::<f64>().unwrap_or_default());
        }
    }
    for (index, level) in book.bids.iter().enumerate() {
        let y = best_bid_y + index as f32 * BOOK_ROW_HEIGHT;
        if visible(y) {
            max_size = max_size.max(level.size.parse::<f64>().unwrap_or_default());
        }
    }

    let book_painter = painter.with_clip_rect(body);
    for (index, level) in book.asks.iter().enumerate().rev() {
        let y = best_ask_y - index as f32 * BOOK_ROW_HEIGHT;
        if !visible(y) {
            continue;
        }
        let selection = selected.filter(|selection| selection.side == BookSide::Ask);
        book_row(
            &book_painter,
            Rect::from_min_max(
                Pos2::new(left, y - BOOK_ROW_HEIGHT / 2.0),
                Pos2::new(right, y + BOOK_ROW_HEIGHT / 2.0),
            ),
            level,
            max_size,
            RED,
            BookRowState {
                in_depth: selection.is_some_and(|selection| index <= selection.index),
                hovered: selection.is_some_and(|selection| index == selection.index),
                own_count: own_orders_at(orders, level, TradeSide::Sell),
            },
        );
    }

    let best_ask = book
        .asks
        .first()
        .and_then(|level| level.price.parse::<f64>().ok());
    let best_bid = book
        .bids
        .first()
        .and_then(|level| level.price.parse::<f64>().ok());
    book_painter.line_segment(
        [
            Pos2::new(left, center_y - BOOK_SPREAD_HALF_HEIGHT),
            Pos2::new(right, center_y - BOOK_SPREAD_HALF_HEIGHT),
        ],
        Stroke::new(1.0, GRID),
    );
    if let (Some(ask), Some(bid)) = (best_ask, best_bid) {
        if let Some(percent) = spread_percent_text(bid, ask) {
            let width = right - left;
            if width >= 160.0 {
                book_painter.text(
                    Pos2::new(left, center_y),
                    Align2::LEFT_CENTER,
                    price_text((ask + bid) / 2.0),
                    FontId::monospace(15.0),
                    TEXT,
                );
            }
            let absolute = compact_decimal(&price_text(ask - bid)).to_owned();
            let label = if width >= 300.0 {
                format!("Spread {absolute} · {percent}")
            } else if width >= 210.0 {
                format!("{absolute} · {percent}")
            } else {
                percent
            };
            book_painter.text(
                Pos2::new(
                    if width >= 160.0 {
                        right
                    } else {
                        (left + right) / 2.0
                    },
                    center_y,
                ),
                if width >= 160.0 {
                    Align2::RIGHT_CENTER
                } else {
                    Align2::CENTER_CENTER
                },
                label,
                FontId::monospace(11.0),
                MUTED,
            );
        }
    }
    book_painter.line_segment(
        [
            Pos2::new(left, center_y + BOOK_SPREAD_HALF_HEIGHT),
            Pos2::new(right, center_y + BOOK_SPREAD_HALF_HEIGHT),
        ],
        Stroke::new(1.0, GRID),
    );
    for (index, level) in book.bids.iter().enumerate() {
        let y = best_bid_y + index as f32 * BOOK_ROW_HEIGHT;
        if !visible(y) {
            continue;
        }
        let selection = selected.filter(|selection| selection.side == BookSide::Bid);
        book_row(
            &book_painter,
            Rect::from_min_max(
                Pos2::new(left, y - BOOK_ROW_HEIGHT / 2.0),
                Pos2::new(right, y + BOOK_ROW_HEIGHT / 2.0),
            ),
            level,
            max_size,
            GREEN,
            BookRowState {
                in_depth: selection.is_some_and(|selection| index <= selection.index),
                hovered: selection.is_some_and(|selection| index == selection.index),
                own_count: own_orders_at(orders, level, TradeSide::Buy),
            },
        );
    }
    if footer_height > 0.0
        && let Some(selection) = selected
    {
        book_inspector(&painter, footer, book, selection, orders);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BookSide {
    Ask,
    Bid,
}

#[derive(Clone, Copy)]
struct BookSelection {
    side: BookSide,
    index: usize,
}

fn hovered_book_level(
    book: &Book,
    body: Rect,
    pointer: Pos2,
    best_ask_y: f32,
    best_bid_y: f32,
) -> Option<BookSelection> {
    if !body.contains(pointer) {
        return None;
    }
    for index in 0..book.asks.len() {
        let y = best_ask_y - index as f32 * BOOK_ROW_HEIGHT;
        if y - BOOK_ROW_HEIGHT / 2.0 >= body.top()
            && y + BOOK_ROW_HEIGHT / 2.0 <= body.bottom()
            && (pointer.y - y).abs() < BOOK_ROW_HEIGHT / 2.0
        {
            return Some(BookSelection {
                side: BookSide::Ask,
                index,
            });
        }
    }
    for index in 0..book.bids.len() {
        let y = best_bid_y + index as f32 * BOOK_ROW_HEIGHT;
        if y - BOOK_ROW_HEIGHT / 2.0 >= body.top()
            && y + BOOK_ROW_HEIGHT / 2.0 <= body.bottom()
            && (pointer.y - y).abs() < BOOK_ROW_HEIGHT / 2.0
        {
            return Some(BookSelection {
                side: BookSide::Bid,
                index,
            });
        }
    }
    None
}

fn book_value_width(total_width: f32) -> f32 {
    ((total_width - 92.0) / 2.0).clamp(60.0, 110.0)
}

struct BookRowState {
    in_depth: bool,
    hovered: bool,
    own_count: usize,
}

fn book_row(
    painter: &egui::Painter,
    row: Rect,
    level: &Level,
    max_size: f64,
    color: Color32,
    state: BookRowState,
) {
    let left = row.left();
    let right = row.right();
    let y = row.center().y;
    if state.in_depth {
        painter.rect_filled(
            row,
            0.0,
            color.gamma_multiply(if state.hovered { 0.22 } else { 0.09 }),
        );
    }
    if state.own_count > 0 {
        painter.rect_filled(
            row,
            0.0,
            Color32::from_rgb(94, 193, 255).gamma_multiply(0.10),
        );
        painter.circle_filled(
            Pos2::new(left + 4.0, y),
            2.6,
            Color32::from_rgb(94, 193, 255),
        );
    }
    let size = level.size.parse::<f64>().unwrap_or_default();
    let fraction = (size / max_size).clamp(0.0, 1.0) as f32;
    painter.rect_filled(
        Rect::from_min_max(
            Pos2::new(right - (right - left) * fraction, y - 9.0),
            Pos2::new(right, y + 9.0),
        ),
        0.0,
        color.gamma_multiply(0.13),
    );
    let value_width = book_value_width(row.width());
    let base_right = right - value_width - 6.0;
    let price_right = base_right - value_width - 6.0;
    let row_top = row.top();
    let row_bottom = row.bottom();
    painter
        .with_clip_rect(Rect::from_min_max(
            Pos2::new(left, row_top),
            Pos2::new(price_right - 3.0, row_bottom),
        ))
        .text(
            Pos2::new(left + if state.own_count > 0 { 11.0 } else { 2.0 }, y),
            Align2::LEFT_CENTER,
            compact_decimal(&level.price),
            FontId::monospace(11.0),
            color,
        );
    painter
        .with_clip_rect(Rect::from_min_max(
            Pos2::new(price_right + 6.0, row_top),
            Pos2::new(base_right, row_bottom),
        ))
        .text(
            Pos2::new(base_right - 3.0, y),
            Align2::RIGHT_CENTER,
            compact_decimal(&level.size),
            FontId::monospace(11.0),
            TEXT,
        );
    let quote_size = if level.quote_size.is_empty() {
        "—"
    } else {
        compact_decimal(&level.quote_size)
    };
    painter
        .with_clip_rect(Rect::from_min_max(
            Pos2::new(base_right + 6.0, row_top),
            Pos2::new(right, row_bottom),
        ))
        .text(
            Pos2::new(right - 3.0, y),
            Align2::RIGHT_CENTER,
            quote_size_text(quote_size),
            FontId::monospace(11.0),
            TEXT,
        );
}

fn own_orders_at(orders: &[(&Account, &OwnOrder)], level: &Level, side: TradeSide) -> usize {
    let Ok(price) = level.price.parse::<rust_decimal::Decimal>() else {
        return 0;
    };
    orders
        .iter()
        .filter(|(_, order)| {
            order.side == side && order.price.parse::<rust_decimal::Decimal>().ok() == Some(price)
        })
        .count()
}

fn execution_metrics(level: &Level, best_price: f64) -> Option<(f64, f64)> {
    let price = level.price.parse::<f64>().ok()?;
    let depth_base = level.depth_base.parse::<f64>().ok()?;
    let depth_quote = level.depth_quote.parse::<f64>().ok()?;
    if best_price <= 0.0 || depth_base <= 0.0 {
        return None;
    }
    Some(((price / best_price - 1.0) * 100.0, depth_quote / depth_base))
}

fn execution_price_text(price: f64) -> String {
    if price.abs() < 0.000_000_01 {
        return format!("{price:.4e}");
    }
    let text = format!("{price:.8}");
    compact_decimal(&text).to_owned()
}

fn book_inspector(
    painter: &egui::Painter,
    footer: Rect,
    book: &Book,
    selection: BookSelection,
    orders: &[(&Account, &OwnOrder)],
) {
    painter.rect_filled(footer, 0.0, crate::RAISED);
    painter.line_segment(
        [footer.left_top(), footer.right_top()],
        Stroke::new(1.0, GRID),
    );
    let content = painter.with_clip_rect(footer.shrink2(Vec2::new(8.0, 2.0)));
    let left = footer.left() + 12.0;
    let lines = [
        footer.top() + 10.0,
        footer.top() + 23.0,
        footer.top() + 36.0,
        footer.top() + 49.0,
        footer.top() + 62.0,
    ];
    let (side, level, best_price, color) = match selection.side {
        BookSide::Ask => ("ASK", &book.asks[selection.index], &book.asks[0], RED),
        BookSide::Bid => ("BID", &book.bids[selection.index], &book.bids[0], GREEN),
    };
    let best_price = best_price.price.parse::<f64>().unwrap_or_default();
    let (percent, weighted_price) = execution_metrics(level, best_price).unwrap_or((0.0, 0.0));
    let own_count = own_orders_at(
        orders,
        level,
        if selection.side == BookSide::Ask {
            TradeSide::Sell
        } else {
            TradeSide::Buy
        },
    );
    let total = match selection.side {
        BookSide::Ask => book.asks.len(),
        BookSide::Bid => book.bids.len(),
    };
    content.text(
        Pos2::new(left, lines[0]),
        Align2::LEFT_CENTER,
        format!(
            "{side} {}/{}  ·  {}",
            selection.index + 1,
            total,
            compact_decimal(&level.price)
        ),
        FontId::monospace(11.0),
        color,
    );
    if own_count > 0 {
        content.text(
            Pos2::new(footer.right() - 12.0, lines[0]),
            Align2::RIGHT_CENTER,
            format!("{own_count} OWN"),
            FontId::monospace(10.0),
            Color32::from_rgb(94, 193, 255),
        );
    }
    content.text(
        Pos2::new(left, lines[1]),
        Align2::LEFT_CENTER,
        format!("DISTANCE  {percent:+.5}% from best"),
        FontId::monospace(10.0),
        MUTED,
    );
    content.text(
        Pos2::new(left, lines[2]),
        Align2::LEFT_CENTER,
        format!(
            "LEVEL  {} base  ·  {} quote",
            compact_decimal(&level.size),
            quote_size_text(&level.quote_size)
        ),
        FontId::monospace(10.0),
        TEXT,
    );
    content.text(
        Pos2::new(left, lines[3]),
        Align2::LEFT_CENTER,
        format!(
            "DEPTH  {} base  ·  {} quote",
            compact_decimal(&level.depth_base),
            quote_size_text(&level.depth_quote)
        ),
        FontId::monospace(10.0),
        TEXT,
    );
    content.text(
        Pos2::new(left, lines[4]),
        Align2::LEFT_CENTER,
        format!(
            "WEIGHTED EXECUTION  {}",
            execution_price_text(weighted_price)
        ),
        FontId::monospace(11.0),
        TEXT,
    );
}

const SERIES_COLORS: [Color32; 6] = [
    Color32::from_rgb(94, 193, 255),
    Color32::from_rgb(255, 190, 94),
    Color32::from_rgb(171, 147, 255),
    Color32::from_rgb(93, 213, 172),
    Color32::from_rgb(239, 111, 123),
    Color32::from_rgb(255, 137, 198),
];

// Keep the first, last, high, and low observation in each horizontal pixel bucket.
// Long windows remain cheap to draw without hiding brief price moves.
fn simplify_points(
    points: &[(i64, f64)],
    start_ms: i64,
    end_ms: i64,
    max_buckets: usize,
) -> Vec<(i64, f64)> {
    let max_buckets = max_buckets.max(1);
    if points.len() <= max_buckets * 4 {
        return points.to_vec();
    }
    let span = (end_ms - start_ms).max(1);
    let bucket_for = |time_ms: i64| {
        ((time_ms - start_ms).clamp(0, span) * max_buckets as i64 / span)
            .min(max_buckets as i64 - 1)
    };
    let mut reduced = Vec::with_capacity(max_buckets * 4);
    let mut index = 0;
    while index < points.len() {
        let first = index;
        let bucket = bucket_for(points[index].0);
        let (mut low, mut high) = (index, index);
        index += 1;
        while index < points.len() && bucket_for(points[index].0) == bucket {
            if points[index].1 < points[low].1 {
                low = index;
            }
            if points[index].1 > points[high].1 {
                high = index;
            }
            index += 1;
        }
        let mut selected = [first, low, high, index - 1];
        selected.sort_unstable();
        let mut previous = None;
        for sample in selected {
            if previous != Some(sample) {
                reduced.push(points[sample]);
                previous = Some(sample);
            }
        }
    }
    reduced
}

fn compare_sources_table(
    ui: &mut egui::Ui,
    markets: &[Market],
    data: &HashMap<Market, MarketData>,
    mode: CompareMode,
    window_secs: u32,
) {
    const ROW_HEIGHT: f32 = 15.0;
    const CHART_SPACE: f32 = 150.0;
    let (header, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
    let painter = ui.painter_at(header);
    let left = header.left() + 8.0;
    let right = header.right() - 8.0;
    let bid_right = right - 70.0;
    let font = FontId::monospace(9.0);
    let title = match mode {
        CompareMode::Trades => "LIVE TRADES",
        CompareMode::BestBidAsk => "BEST BID / ASK",
    };
    painter.text(
        Pos2::new(left, header.center().y),
        Align2::LEFT_CENTER,
        format!("{title} · {window_secs}s"),
        font.clone(),
        MUTED,
    );
    if mode == CompareMode::BestBidAsk {
        painter.text(
            Pos2::new(bid_right, header.center().y),
            Align2::RIGHT_CENTER,
            "BID",
            font.clone(),
            MUTED,
        );
    }
    painter.text(
        Pos2::new(right, header.center().y),
        Align2::RIGHT_CENTER,
        if mode == CompareMode::Trades {
            "LAST"
        } else {
            "ASK"
        },
        font.clone(),
        MUTED,
    );

    // Keep a useful plot visible even when many markets share a small tile.
    let rows_height = (ui.available_height() - CHART_SPACE - 6.0)
        .max(ROW_HEIGHT)
        .min(ROW_HEIGHT * 6.0)
        .min(ROW_HEIGHT * markets.len() as f32);
    egui::ScrollArea::vertical()
        .id_salt("compare_sources")
        .max_height(rows_height)
        .show_rows(ui, ROW_HEIGHT, markets.len(), |ui, rows| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for index in rows {
                let market = &markets[index];
                let color = SERIES_COLORS[index % SERIES_COLORS.len()];
                let (rect, response) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), ROW_HEIGHT),
                    Sense::hover(),
                );
                let painter = ui.painter_at(rect);
                let left = rect.left() + 8.0;
                let right = rect.right() - 8.0;
                let bid_right = right - 70.0;
                let source_right = if mode == CompareMode::BestBidAsk {
                    bid_right - 70.0
                } else {
                    right - 78.0
                };
                let center = rect.center().y;
                painter.rect_filled(
                    Rect::from_center_size(Pos2::new(left + 3.0, center), Vec2::splat(6.0)),
                    1.0,
                    color,
                );
                let name = format!(
                    "{} {} {}",
                    market.exchange.label(),
                    market.kind.label(),
                    market.symbol
                );
                painter
                    .with_clip_rect(Rect::from_min_max(
                        Pos2::new(left + 12.0, rect.top()),
                        Pos2::new(source_right - 5.0, rect.bottom()),
                    ))
                    .text(
                        Pos2::new(left + 13.0, center),
                        Align2::LEFT_CENTER,
                        &name,
                        font.clone(),
                        MUTED,
                    );
                let market_data = data.get(market);
                let (bid, last) = match mode {
                    CompareMode::Trades => (
                        None,
                        market_data
                            .and_then(|data| data.last_price.as_ref())
                            .map(|price| compact_decimal(&price.price)),
                    ),
                    CompareMode::BestBidAsk => {
                        let quote = market_data.and_then(|data| data.best_bid_ask.as_ref());
                        (
                            quote.map(|quote| compact_decimal(&quote.bid)),
                            quote.map(|quote| compact_decimal(&quote.ask)),
                        )
                    }
                };
                if let Some(bid) = bid {
                    painter
                        .with_clip_rect(Rect::from_min_max(
                            Pos2::new(source_right, rect.top()),
                            Pos2::new(bid_right, rect.bottom()),
                        ))
                        .text(
                            Pos2::new(bid_right, center),
                            Align2::RIGHT_CENTER,
                            bid,
                            font.clone(),
                            color.gamma_multiply(0.65),
                        );
                }
                painter
                    .with_clip_rect(Rect::from_min_max(
                        Pos2::new(
                            if bid.is_some() {
                                bid_right
                            } else {
                                source_right
                            },
                            rect.top(),
                        ),
                        Pos2::new(right, rect.bottom()),
                    ))
                    .text(
                        Pos2::new(right, center),
                        Align2::RIGHT_CENTER,
                        last.unwrap_or("—"),
                        font.clone(),
                        color,
                    );
                let details = match mode {
                    CompareMode::Trades => format!("{name} · Last {}", last.unwrap_or("—")),
                    CompareMode::BestBidAsk => format!(
                        "{name} · Bid {} · Ask {}",
                        bid.unwrap_or("—"),
                        last.unwrap_or("—")
                    ),
                };
                response.on_hover_text(details);
            }
        });
}

pub fn compare_ui(
    ui: &mut egui::Ui,
    markets: &[Market],
    data: &HashMap<Market, MarketData>,
    mode: CompareMode,
    percent: bool,
    window_secs: u32,
    orders: &[(usize, &Account, &OwnOrder)],
    view: &mut YAxisView,
) {
    compare_sources_table(ui, markets, data, mode, window_secs);
    ui.add_space(4.0);

    let size = ui.available_size();
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(size.x.max(1.0), size.y.max(1.0)),
        Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, PANEL);
    let plot = Rect::from_min_max(
        Pos2::new(rect.left() + 14.0, rect.top() + 14.0),
        Pos2::new(rect.right() - 78.0, rect.bottom() - 30.0),
    );
    if plot.width() < 80.0 || plot.height() < 80.0 {
        return;
    }

    let end_ms = markets
        .iter()
        .filter_map(|market| data.get(market))
        .filter_map(|market_data| match mode {
            CompareMode::Trades => market_data
                .last_price
                .as_ref()
                .filter(|_| !market_data.price_trades.is_empty())
                .map(|price| price.time_ms),
            CompareMode::BestBidAsk => market_data.quotes.back().map(|quote| quote.time_ms),
        })
        .max();
    let Some(end_ms) = end_ms else {
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            match mode {
                CompareMode::Trades => "Waiting for live trades…",
                CompareMode::BestBidAsk => "Waiting for order books…",
            },
            FontId::proportional(13.0),
            MUTED,
        );
        return;
    };
    let cutoff_ms = end_ms - i64::from(window_secs.clamp(10, 600)) * 1_000;
    let mut lines = Vec::with_capacity(markets.len() * 2);
    for (index, market) in markets.iter().enumerate() {
        let color = SERIES_COLORS[index % SERIES_COLORS.len()];
        let Some(market_data) = data.get(market) else {
            continue;
        };
        match mode {
            CompareMode::Trades => {
                let mut points: Vec<_> = market_data
                    .price_trades
                    .iter()
                    .filter(|trade| trade.time_ms >= cutoff_ms)
                    .map(|trade| (trade.time_ms, trade.price))
                    .collect();
                if percent && let Some(base) = points.first().map(|(_, price)| *price) {
                    for (_, price) in &mut points {
                        *price = (*price / base - 1.0) * 100.0;
                    }
                }
                lines.push((points, color, 1.7));
            }
            CompareMode::BestBidAsk => {
                for (bid, line_color, width) in
                    [(true, color.gamma_multiply(0.65), 1.4), (false, color, 1.8)]
                {
                    let points = market_data
                        .quotes
                        .iter()
                        .filter(|quote| quote.time_ms >= cutoff_ms)
                        .map(|quote| (quote.time_ms, if bid { quote.bid } else { quote.ask }))
                        .filter(|(_, price)| price.is_finite() && *price > 0.0)
                        .collect();
                    lines.push((points, line_color, width));
                }
            }
        }
    }
    let Some(first_ms) = lines
        .iter()
        .flat_map(|(points, _, _)| points.iter().map(|(time, _)| *time))
        .min()
    else {
        return;
    };
    // Fill the plot with the history collected so far, then roll at the selected limit.
    let start_ms = cutoff_ms.max(first_ms - 2_000).min(end_ms - 10_000);
    let low = lines
        .iter()
        .flat_map(|(points, _, _)| points.iter().map(|(_, price)| *price))
        .fold(f64::INFINITY, f64::min);
    let high = lines
        .iter()
        .flat_map(|(points, _, _)| points.iter().map(|(_, price)| *price))
        .fold(f64::NEG_INFINITY, f64::max);
    if !low.is_finite() || !high.is_finite() {
        return;
    }
    let percent = percent && mode == CompareMode::Trades;
    let pad = ((high - low) * 0.08).max(if percent { 0.01 } else { high.abs() * 0.0001 });
    let (floor, ceiling) = view.interact(ui, &response, rect, plot, (low - pad, high + pad), true);
    let plot_painter = painter.with_clip_rect(plot);
    let x_for = |time: i64| {
        plot.left() + (time - start_ms) as f32 / (end_ms - start_ms) as f32 * plot.width()
    };
    let y_for =
        |price: f64| plot.bottom() - ((price - floor) / (ceiling - floor)) as f32 * plot.height();
    for tick in 0..=4 {
        let y = plot.top() + plot.height() * tick as f32 / 4.0;
        painter.line_segment(
            [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            Stroke::new(1.0, GRID),
        );
        let value = ceiling - (ceiling - floor) * tick as f64 / 4.0;
        let label = if percent {
            format!("{value:+.2}%")
        } else {
            price_text(value)
        };
        painter.text(
            Pos2::new(rect.right() - 9.0, y),
            Align2::RIGHT_CENTER,
            label,
            FontId::monospace(10.0),
            MUTED,
        );
        if tick % 2 == 0 {
            let time = start_ms + (end_ms - start_ms) * tick / 4;
            let seconds = time.div_euclid(1000);
            let label = if end_ms - start_ms >= 300_000 {
                format!(
                    "{:02}:{:02}",
                    seconds.div_euclid(3_600).rem_euclid(24),
                    seconds.div_euclid(60).rem_euclid(60)
                )
            } else {
                format!(
                    "{:02}:{:02}",
                    seconds.div_euclid(60).rem_euclid(60),
                    seconds.rem_euclid(60)
                )
            };
            painter.text(
                Pos2::new(x_for(time), plot.bottom() + 12.0),
                Align2::CENTER_CENTER,
                label,
                FontId::monospace(10.0),
                MUTED,
            );
        }
    }
    for (points, color, width) in &lines {
        let simplified = (points.len() > plot.width() as usize * 4)
            .then(|| simplify_points(points, start_ms, end_ms, plot.width().ceil() as usize));
        let points = simplified.as_deref().unwrap_or(points);
        for pair in points.windows(2) {
            plot_painter.line_segment(
                [
                    Pos2::new(x_for(pair[0].0), y_for(pair[0].1)),
                    Pos2::new(x_for(pair[1].0), y_for(pair[1].1)),
                ],
                Stroke::new(*width, *color),
            );
        }
        if mode == CompareMode::BestBidAsk
            && let Some((time, price)) = points.last()
        {
            plot_painter.circle_filled(Pos2::new(x_for(*time), y_for(*price)), 2.5, *color);
        }
    }
    let order_levels: Vec<_> = orders
        .iter()
        .filter_map(|(index, account, order)| {
            let market = markets.get(*index)?;
            let base = if percent {
                data.get(market)?
                    .price_trades
                    .iter()
                    .find(|trade| trade.time_ms >= cutoff_ms)
                    .map(|trade| trade.price)?
            } else {
                1.0
            };
            let value = order_plot_value(&order.price, base, percent)?;
            (floor..=ceiling)
                .contains(&value)
                .then_some((*index, *account, *order, y_for(value)))
        })
        .collect();
    for (_, _, order, y) in &order_levels {
        let color = trade_color(order.side, MUTED);
        plot_painter.line_segment(
            [Pos2::new(plot.left(), *y), Pos2::new(plot.right(), *y)],
            Stroke::new(1.0, color.gamma_multiply(0.35)),
        );
        plot_painter.rect_filled(
            Rect::from_center_size(Pos2::new(plot.right(), *y), Vec2::splat(5.0)),
            1.0,
            color,
        );
    }
    let hovered_order = response
        .hover_pos()
        .filter(|pointer| plot.contains(*pointer))
        .and_then(|pointer| {
            order_levels
                .iter()
                .filter(|(_, _, _, y)| (pointer.y - *y).abs() <= 5.0)
                .min_by(|a, b| (pointer.y - a.3).abs().total_cmp(&(pointer.y - b.3).abs()))
        });
    if mode == CompareMode::Trades {
        for (index, market) in markets.iter().enumerate() {
            let Some(market_data) = data.get(market) else {
                continue;
            };
            let mut trades = market_data
                .price_trades
                .iter()
                .filter(|trade| trade.time_ms >= cutoff_ms);
            let Some(first) = trades.next() else {
                continue;
            };
            let base = first.price;
            let fallback = SERIES_COLORS[index % SERIES_COLORS.len()];
            let position = |trade: &TradePoint| {
                let price = if percent {
                    (trade.price / base - 1.0) * 100.0
                } else {
                    trade.price
                };
                Pos2::new(x_for(trade.time_ms), y_for(price))
            };
            let visible_count = 1 + trades.clone().count();
            if visible_count <= plot.width() as usize * 2 {
                for trade in std::iter::once(first).chain(trades) {
                    plot_painter.circle_filled(
                        position(trade),
                        1.8,
                        trade_color(trade.side, fallback),
                    );
                }
            } else {
                // One dot per horizontal pixel keeps dense tape streams responsive.
                let mut pending = first;
                let mut column = position(first).x as i32;
                for trade in trades {
                    let next_column = position(trade).x as i32;
                    if next_column != column {
                        plot_painter.circle_filled(
                            position(pending),
                            1.8,
                            trade_color(pending.side, fallback),
                        );
                        column = next_column;
                    }
                    pending = trade;
                }
                plot_painter.circle_filled(
                    position(pending),
                    1.8,
                    trade_color(pending.side, fallback),
                );
            }
            if let Some(latest) = market_data.price_trades.back() {
                plot_painter.circle_filled(
                    position(latest),
                    2.8,
                    trade_color(latest.side, fallback),
                );
            }
        }
    }
    if let Some(pointer) = response.hover_pos().filter(|pos| plot.contains(*pos)) {
        painter.line_segment(
            [
                Pos2::new(pointer.x, plot.top()),
                Pos2::new(pointer.x, plot.bottom()),
            ],
            Stroke::new(1.0, MUTED),
        );
        if mode == CompareMode::Trades && hovered_order.is_none() {
            let mut nearest: Option<(f32, &Market, &TradePoint, Pos2, usize)> = None;
            for (index, market) in markets.iter().enumerate() {
                let Some(market_data) = data.get(market) else {
                    continue;
                };
                let base = market_data
                    .price_trades
                    .iter()
                    .find(|trade| trade.time_ms >= cutoff_ms)
                    .map(|trade| trade.price)
                    .unwrap_or(1.0);
                for trade in market_data
                    .price_trades
                    .iter()
                    .filter(|trade| trade.time_ms >= cutoff_ms)
                {
                    let value = if percent {
                        (trade.price / base - 1.0) * 100.0
                    } else {
                        trade.price
                    };
                    let position = Pos2::new(x_for(trade.time_ms), y_for(value));
                    if !plot.contains(position) {
                        continue;
                    }
                    let distance = position.distance_sq(pointer);
                    if nearest
                        .as_ref()
                        .is_none_or(|(best, _, _, _, _)| distance < *best)
                    {
                        nearest = Some((distance, market, trade, position, index));
                    }
                }
            }
            if let Some((_, market, trade, position, index)) = nearest {
                plot_painter.circle_stroke(position, 5.0, Stroke::new(1.5, TEXT));
                let color = trade_color(trade.side, SERIES_COLORS[index % SERIES_COLORS.len()]);
                plot_painter.circle_filled(position, 2.5, color);
                painter
                    .with_clip_rect(Rect::from_min_max(
                        Pos2::new(plot.left(), rect.top()),
                        Pos2::new(plot.right(), plot.top()),
                    ))
                    .text(
                        Pos2::new(plot.left() + 3.0, rect.top() + 7.0),
                        Align2::LEFT_CENTER,
                        format!(
                            "{} · {} {} {} · {} UTC · {} × {}",
                            trade_side_label(trade.side),
                            venue_code(market.exchange),
                            market.kind.label(),
                            market.symbol,
                            trade_utc(trade.time_ms, false),
                            trade.price,
                            trade.size,
                        ),
                        FontId::monospace(9.0),
                        TEXT,
                    );
            }
        }
    }
    if let Some((index, account, order, y)) = hovered_order {
        let market = &markets[*index];
        let color = trade_color(order.side, MUTED);
        painter.line_segment(
            [Pos2::new(plot.left(), *y), Pos2::new(plot.right(), *y)],
            Stroke::new(1.4, color),
        );
        painter
            .with_clip_rect(Rect::from_min_max(
                Pos2::new(plot.left(), rect.top()),
                Pos2::new(plot.right(), plot.top()),
            ))
            .text(
                Pos2::new(plot.left() + 3.0, rect.top() + 7.0),
                Align2::LEFT_CENTER,
                format!(
                    "ORDER {} · {} {} {} · {} × {} · …{}",
                    trade_side_label(order.side),
                    venue_code(market.exchange),
                    market.kind.label(),
                    market.symbol,
                    order.price,
                    order.size,
                    &account.address[account.address.len().saturating_sub(4)..],
                ),
                FontId::monospace(9.0),
                color,
            );
    }
}

fn order_plot_value(price: &str, base: f64, percent: bool) -> Option<f64> {
    let price = price.parse::<f64>().ok()?;
    if !price.is_finite() || price <= 0.0 || !base.is_finite() || base <= 0.0 {
        return None;
    }
    Some(if percent {
        (price / base - 1.0) * 100.0
    } else {
        price
    })
}

#[cfg(test)]
mod tests {
    use super::{
        YAxisView, book_scroll_bounds, execution_metrics, order_plot_value, own_orders_at,
        quote_size_text, simplify_points, trade_utc,
    };
    use terminal_core::{Account, Exchange, Level, OwnOrder, TradeSide};

    #[test]
    fn own_book_marker_matches_exact_price_and_side() {
        let account = Account {
            exchange: Exchange::Hyperliquid,
            address: "0x0000000000000000000000000000000000000000".into(),
        };
        let order = OwnOrder {
            coin: "BTC".into(),
            order_id: 1,
            side: TradeSide::Buy,
            price: "100.000".into(),
            size: "1".into(),
        };
        let level = Level {
            price: "100".into(),
            size: "2".into(),
            quote_size: "200".into(),
            depth_base: "2".into(),
            depth_quote: "200".into(),
        };
        assert_eq!(
            own_orders_at(&[(&account, &order)], &level, TradeSide::Buy),
            1
        );
        assert_eq!(
            own_orders_at(&[(&account, &order)], &level, TradeSide::Sell),
            0
        );
    }

    #[test]
    fn own_order_uses_source_baseline_in_percent_comparison() {
        assert!((order_plot_value("105", 100.0, true).unwrap() - 5.0).abs() < 1e-10);
        assert_eq!(order_plot_value("105", 100.0, false), Some(105.0));
        assert_eq!(order_plot_value("0", 100.0, true), None);
    }

    #[test]
    fn vertical_pan_reveals_lower_orders_and_keeps_manual_bounds() {
        let mut view = YAxisView::default();
        view.pan((90.0, 110.0), -100.0, 200.0);
        assert_eq!(view.manual, Some((80.0, 100.0)));
        assert!((80.0..=100.0).contains(&85.0));
        // A live repaint or a horizontal-only drag must not alter the manual range.
        view.pan((95.0, 115.0), 0.0, 200.0);
        assert_eq!(view.manual, Some((80.0, 100.0)));
    }

    #[test]
    fn vertical_zoom_keeps_pointer_price_fixed_and_can_reveal_distant_orders() {
        let mut view = YAxisView::default();
        view.zoom((90.0, 110.0), -300.0, 0.25);
        let (floor, ceiling) = view.manual.unwrap();
        assert!((floor + 0.25 * (ceiling - floor) - 95.0).abs() < 1e-10);
        assert!((floor..=ceiling).contains(&80.0));
        assert!(ceiling - floor > 20.0);
        view.zoom((floor, ceiling), 300.0, 0.25);
        let (floor, ceiling) = view.manual.unwrap();
        assert!((floor - 90.0).abs() < 1e-10);
        assert!((ceiling - 110.0).abs() < 1e-10);
    }

    #[test]
    fn trade_times_display_in_utc_with_milliseconds() {
        let timestamp = 86_400_000 + 3_600_000 + 2 * 60_000 + 3_000 + 4;
        assert_eq!(trade_utc(timestamp, false), "01:02:03.004");
        assert_eq!(trade_utc(timestamp, true), "01:02:03");
    }

    #[test]
    fn book_scroll_reaches_the_last_level_on_either_side() {
        let (min, max) = book_scroll_bounds(300.0, 100, 100);
        assert_eq!((min, max), (-1662.0, 1662.0));
        assert_eq!(book_scroll_bounds(300.0, 1, 1), (0.0, 0.0));
    }

    #[test]
    fn execution_price_uses_cumulative_size_through_hovered_level() {
        let level = Level {
            price: "99".to_owned(),
            size: "4".to_owned(),
            quote_size: "396".to_owned(),
            depth_base: "5".to_owned(),
            depth_quote: "496".to_owned(),
        };
        let (percent, average) = execution_metrics(&level, 100.0).unwrap();
        assert!((percent + 1.0).abs() < 1e-9);
        assert!((average - 99.2).abs() < 1e-9);
        let ask = Level {
            price: "102".to_owned(),
            depth_base: "4".to_owned(),
            depth_quote: "406".to_owned(),
            ..level
        };
        let (percent, average) = execution_metrics(&ask, 100.0).unwrap();
        assert!((percent - 2.0).abs() < 1e-9);
        assert!((average - 101.5).abs() < 1e-9);
    }

    #[test]
    fn quote_size_labels_are_compact_without_turning_tiny_values_into_zero() {
        assert_eq!(quote_size_text("116594.089066"), "116.59K");
        assert_eq!(quote_size_text("15.115374"), "15.1154");
        assert_eq!(quote_size_text("0.00000012345"), "1.23e-7");
    }

    #[test]
    fn long_window_simplification_keeps_short_price_spikes() {
        let mut points = (0..1_000).map(|time| (time, 100.0)).collect::<Vec<_>>();
        points[431].1 = 150.0;
        points[756].1 = 50.0;
        let reduced = simplify_points(&points, 0, 1_000, 20);
        assert!(reduced.len() <= 80);
        assert!(reduced.contains(&(431, 150.0)));
        assert!(reduced.contains(&(756, 50.0)));
        assert_eq!(reduced.first(), points.first());
        assert_eq!(reduced.last(), points.last());
    }
}
