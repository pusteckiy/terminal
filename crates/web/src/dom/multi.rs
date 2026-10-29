use super::*;
use crate::{orderflow, view::venue_color};
use terminal_core::{Exchange, Market, MarketKind};

struct Source<'a> {
    index: usize,
    market: &'a Market,
    info: &'a SymbolInfo,
    data: Option<&'a MarketData>,
    multiplier: Decimal,
}

impl Source<'_> {
    fn book(&self) -> Option<&Book> {
        self.data.filter(|data| data.connected)?.book.as_ref()
    }

    fn rows(
        &self,
        orders: &[(usize, &Account, &OwnOrder)],
        step: Decimal,
        low: Decimal,
        high: Decimal,
    ) -> BTreeMap<Decimal, Row> {
        let Some(book) = self.book() else {
            return BTreeMap::new();
        };
        let orders: Vec<_> = orders
            .iter()
            .filter(|(index, _, _)| *index == self.index)
            .map(|(_, account, order)| (*account, *order))
            .collect();
        book_rows(
            book,
            &orders,
            step * self.multiplier,
            low * self.multiplier,
            high * self.multiplier,
        )
        .into_iter()
        .map(|(price, mut row)| {
            let multiplier = self.multiplier.to_f64().unwrap_or(1.0);
            for volume in [
                &mut row.bid,
                &mut row.ask,
                &mut row.own.buy,
                &mut row.own.sell,
            ] {
                volume.base *= multiplier;
            }
            (price / self.multiplier, row)
        })
        .collect()
    }

    fn covered(&self, price: Decimal, step: Decimal, bid: bool) -> bool {
        let Some(book) = self.book() else {
            return false;
        };
        let levels = if bid { &book.bids } else { &book.asks };
        levels
            .last()
            .and_then(|level| positive(&level.price))
            .is_some_and(|last| {
                let last = group(last / self.multiplier, step);
                if bid { price >= last } else { price <= last }
            })
    }
}

fn same_market(sources: &[Source<'_>]) -> bool {
    sources.first().is_some_and(|first| {
        sources.iter().all(|source| {
            source.market.kind == first.market.kind
                && source.info.quote.eq_ignore_ascii_case(&first.info.quote)
        })
    })
}

fn total_row(rows: &[BTreeMap<Decimal, Row>], price: Decimal) -> Row {
    let mut total = Row::default();
    for row in rows.iter().filter_map(|rows| rows.get(&price)) {
        total.bid.add(row.bid);
        total.ask.add(row.ask);
        total.own.buy.add(row.own.buy);
        total.own.sell.add(row.own.sell);
    }
    total
}

fn paint_size(
    painter: &egui::Painter,
    cell: Rect,
    value: f64,
    known: bool,
    color: Color32,
    maximum: f64,
    font: &FontId,
) {
    if !known {
        painter.text(
            cell.center(),
            Align2::CENTER_CENTER,
            "—",
            font.clone(),
            MUTED,
        );
    } else if value > 0.0 {
        painter.rect_filled(
            Rect::from_min_size(
                cell.left_top(),
                Vec2::new(
                    cell.width() * (value / maximum).clamp(0.0, 1.0) as f32,
                    cell.height(),
                ),
            ),
            0.0,
            color.gamma_multiply(0.22),
        );
        painter.with_clip_rect(cell).text(
            Pos2::new(cell.right() - 5.0, cell.center().y),
            Align2::RIGHT_CENTER,
            amount(value),
            font.clone(),
            color,
        );
    }
}

pub fn ui(
    ui: &mut egui::Ui,
    markets: &[Market],
    data: &HashMap<Market, MarketData>,
    catalogs: &HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
    settings: &Settings,
    view: &mut View,
    orders: &[(usize, &Account, &OwnOrder)],
    own_fills: &[(usize, &Account, &OwnFill)],
) {
    view.hidden_sources
        .retain(|market| markets.contains(market));
    let mut highlighted = None;
    ui.horizontal_wrapped(|ui| {
        for (index, market) in markets.iter().enumerate() {
            let hidden = view.hidden_sources.contains(market);
            let color = venue_color(market.exchange);
            let button = ui.selectable_label(
                !hidden,
                egui::RichText::new(market.exchange.label())
                    .size(10.0)
                    .color(if hidden { MUTED } else { color }),
            );
            if button.hovered() && !hidden {
                highlighted = Some(index);
            }
            if button.clicked() {
                if hidden {
                    view.hidden_sources.remove(market);
                } else {
                    view.hidden_sources.insert(market.clone());
                }
            }
            button.on_hover_text(format!(
                "{} {} {}\nClick to hide/show this source in the chart, ladder and total.",
                market.exchange.label(),
                market.kind.label(),
                market.symbol
            ));
        }
    });
    let mut sources: Vec<Source<'_>> = Vec::new();
    for (index, market) in markets
        .iter()
        .enumerate()
        .filter(|(_, market)| !view.hidden_sources.contains(*market))
    {
        if sources.iter().any(|source| source.market == market) {
            continue;
        }
        let Some(info) = orderflow::symbol_info(market, catalogs) else {
            ui.weak(format!(
                "Loading {} instrument details…",
                market.exchange.label()
            ));
            return;
        };
        sources.push(Source {
            index,
            market,
            info,
            data: data.get(market),
            multiplier: orderflow::base_units(&info.base, market.kind).1.into(),
        });
    }
    let Some(first) = sources.first() else {
        ui.weak("Select a source above to show its DOM.");
        return;
    };
    let base = orderflow::base_units(&first.info.base, first.market.kind).0;
    if sources.iter().any(|source| {
        !orderflow::base_units(&source.info.base, source.market.kind)
            .0
            .eq_ignore_ascii_case(base)
    }) {
        ui.weak("Choose the same underlying coin for every DOM source in config.");
        return;
    }
    let best = |bid: bool| {
        sources
            .iter()
            .filter_map(|source| {
                let book = source.book()?;
                let levels = if bid { &book.bids } else { &book.asks };
                Some(positive(&levels.first()?.price)? / source.multiplier)
            })
            .reduce(|a, b| if bid { a.max(b) } else { a.min(b) })
    };
    let bid = best(true);
    let ask = best(false);
    let Some(reference) = bid
        .zip(ask)
        .map(|(bid, ask)| (bid + ask) / Decimal::TWO)
        .or(bid)
        .or(ask)
    else {
        ui.weak("Waiting for live order books…");
        return;
    };
    let Some(book_source) = sources.iter().find(|source| source.book().is_some()) else {
        return;
    };
    // A shared ladder uses the coarsest available normalized tick.
    let tick = sources
        .iter()
        .filter_map(|source| {
            Some(native_step(source.book()?, Some(source.info)) / source.multiplier)
        })
        .max()
        .unwrap_or(Decimal::ONE);
    let tick_info = SymbolInfo {
        price_step: Some(tick.to_string()),
        ..book_source.info.clone()
    };
    let step = display_step(
        book_source.book().unwrap(),
        Some(&tick_info),
        settings,
        reference,
    );
    if view.step != Some(step) || view.anchor.is_none() {
        view.step = Some(step);
        view.following = true;
        view.scroll_remainder = 0.0;
    }
    if view.following {
        view.anchor = Some(group(reference, step));
    }
    let now_ms = crate::utc_now_ms();
    let window = settings.window_secs.clamp(1, 600);
    let compatible = same_market(&sources);
    let spread = if compatible {
        bid.and_then(|bid| bid.to_f64())
            .zip(ask.and_then(|ask| ask.to_f64()))
            .and_then(|(bid, ask)| crate::view::spread_percent_text(bid, ask))
            .unwrap_or_else(|| "—".into())
    } else {
        "—".into()
    };
    let (rect, response) = ui.allocate_exact_size(
        ui.available_size().max(Vec2::splat(1.0)),
        Sense::click_and_drag(),
    );
    if rect.height() < 100.0 || rect.width() < 160.0 {
        return;
    }
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, SURFACE);
    let flow_width = if settings.trade_stream {
        (rect.width() * 0.48)
            .max(80.0)
            .min((rect.width() - 100.0).max(0.0))
    } else {
        0.0
    };
    let ladder = Rect::from_min_max(
        Pos2::new(rect.left() + flow_width, rect.top()),
        rect.right_bottom(),
    );
    let top = rect.top() + if settings.compact { 46.0 } else { 62.0 };
    let body = Rect::from_min_max(Pos2::new(rect.left(), top), rect.right_bottom());
    let plot = Rect::from_min_max(
        Pos2::new(rect.left() + 8.0, top),
        Pos2::new(ladder.left() - 8.0, (rect.bottom() - 54.0).max(top + 1.0)),
    );
    if settings.trade_stream {
        painter.text(
            Pos2::new(plot.left(), rect.top() + 13.0),
            Align2::LEFT_CENTER,
            format!("TRADES · {window}s"),
            FontId::monospace(10.0),
            MUTED,
        );
    }
    let unit = if settings.units == Units::Base {
        base
    } else if compatible {
        &first.info.quote
    } else {
        "native quote"
    };
    let mut metrics_rect = ladder;
    if !view.following && !settings.trade_stream {
        metrics_rect.max.x -= 52.0;
    }
    painter.with_clip_rect(metrics_rect).text(
        Pos2::new(ladder.left() + 6.0, rect.top() + 12.0),
        Align2::LEFT_CENTER,
        format!("{unit} · step {} · SPR {spread}", step.normalize()),
        FontId::monospace(9.0),
        MUTED,
    );
    if !view.following
        && ui
            .put(
                Rect::from_min_size(
                    if settings.trade_stream {
                        Pos2::new(rect.left() + 8.0, rect.top() + 22.0)
                    } else {
                        Pos2::new(ladder.right() - 50.0, rect.top() + 2.0)
                    },
                    Vec2::new(46.0, 18.0),
                ),
                egui::Button::new(egui::RichText::new("Center").size(10.0)),
            )
            .clicked()
    {
        view.following = true;
        view.anchor = Some(group(reference, step));
    }
    if response.double_clicked() {
        view.following = true;
        view.anchor = Some(group(reference, step));
    }
    if ui.rect_contains_pointer(rect) {
        view.scroll_remainder += ui.input(|input| input.smooth_scroll_delta.y)
            + if response.dragged() {
                ui.input(|input| input.pointer.delta().y)
            } else {
                0.0
            };
        let rows = (view.scroll_remainder / ROW_HEIGHT).trunc() as i64;
        if rows != 0 {
            view.following = false;
            view.anchor = view
                .anchor
                .map(|price| (price + step * Decimal::from(rows)).max(step));
            view.scroll_remainder -= rows as f32 * ROW_HEIGHT;
        }
    }
    if !view.following {
        let mut low = reference;
        let mut high = reference;
        for source in &sources {
            if let (Some(book), Some(data)) = (source.book(), source.data) {
                let own: Vec<_> = orders
                    .iter()
                    .filter(|(index, _, _)| *index == source.index)
                    .map(|(_, account, order)| (*account, *order))
                    .collect();
                let bounds = anchor_bounds(
                    book,
                    &data.dom,
                    &own,
                    now_ms,
                    window,
                    step * source.multiplier,
                    reference * source.multiplier,
                );
                low = low.min(bounds.0 / source.multiplier);
                high = high.max(bounds.1 / source.multiplier);
            }
        }
        if settings.trade_stream && settings.show_fills {
            for (index, _, fill) in own_fills {
                if (0..i64::from(window) * 1000).contains(&(now_ms - fill.time_ms))
                    && let Some(source) = sources.iter().find(|source| source.index == *index)
                    && let Some(price) = positive(&fill.price)
                {
                    let price = group(price / source.multiplier, step);
                    low = low.min(price);
                    high = high.max(price);
                }
            }
        }
        view.anchor = view.anchor.map(|price| price.clamp(low, high));
    }
    let count = (body.height() / ROW_HEIGHT).ceil() as i64;
    let center_rows = if settings.trade_stream {
        (plot.height() / ROW_HEIGHT).ceil() as i64
    } else {
        count
    };
    let top_price = view.anchor.unwrap_or(reference) + step * Decimal::from(center_rows / 2);
    let low = (top_price - step * Decimal::from(count - 1)).max(Decimal::ZERO);
    let rows: Vec<_> = sources
        .iter()
        .map(|source| source.rows(orders, step, low, top_price + step))
        .collect();
    let maximum = rows
        .iter()
        .flat_map(|rows| rows.values())
        .flat_map(|row| [row.bid.value(settings.units), row.ask.value(settings.units)])
        .fold(0.0_f64, f64::max)
        .max(1e-20);
    let pointer = response.hover_pos();
    let start_ms = (now_ms.div_euclid(1_000) - i64::from(window) + 1) * 1_000;
    let x_for = |time| {
        plot.left() + (time - start_ms) as f32 / (now_ms - start_ms).max(1) as f32 * plot.width()
    };
    let nearest = pointer
        .filter(|pointer| settings.trade_stream && plot.contains(*pointer))
        .and_then(|pointer| {
            sources
                .iter()
                .flat_map(|source| {
                    source.data.into_iter().flat_map(move |data| {
                        data.price_trades.iter().map(move |trade| (source, trade))
                    })
                })
                .filter(|(_, trade)| (start_ms..=now_ms).contains(&trade.time_ms))
                .filter_map(|(source, trade)| {
                    let pos = Pos2::new(
                        x_for(trade.time_ms),
                        level_y(
                            trade.exact_price? / source.multiplier,
                            top_price,
                            step,
                            plot,
                        ),
                    );
                    (plot.contains(pos) && pos.distance(pointer) <= 12.0).then_some((
                        source.index,
                        trade,
                        pos,
                    ))
                })
                .min_by(|a, b| {
                    a.2.distance_sq(pointer)
                        .total_cmp(&b.2.distance_sq(pointer))
                })
        });
    if let Some((index, _, _)) = nearest {
        highlighted = Some(index);
    }
    let mut inspected_row = None;
    let price_width = (reference.normalize().to_string().len() as f32 * 6.0 + 18.0)
        .clamp(72.0, 150.0)
        .min((ladder.width() - 40.0).max(36.0));
    let columns = sources.len() + usize::from(settings.total_liquidity);
    let source_width = ((ladder.width() - price_width) / columns as f32).max(if settings.compact {
        80.0
    } else {
        128.0
    });
    let width = source_width * columns as f32;
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_max(
        Pos2::new(ladder.left() + price_width, rect.top() + 24.0),
        ladder.right_bottom(),
    )));
    child.style_mut().always_scroll_the_only_direction = false;
    egui::ScrollArea::horizontal().id_salt("dom-venues").auto_shrink([false, false]).show(&mut child, |ui| {
        let (content, _) = ui.allocate_exact_size(Vec2::new(width.max(ladder.width() - price_width), (rect.height() - 24.0).max(1.0)), Sense::hover());
        let painter = ui.painter_at(content);
        let font = FontId::monospace(10.0);
        let price_x = content.left();
        let column = |index| Rect::from_min_max(Pos2::new(price_x + index as f32 * source_width, content.top()),
            Pos2::new(price_x + (index + 1) as f32 * source_width, content.bottom()));
        for index in 0..(sources.len() + usize::from(settings.total_liquidity)) {
            let col = column(index);
            let total = index == sources.len();
            if !total && ui.rect_contains_pointer(col.intersect(ui.clip_rect())) {
                highlighted = Some(sources[index].index);
            }
            if !total && highlighted == Some(sources[index].index) {
                painter.rect_filled(col, 0.0, Color32::from_white_alpha(7));
            }
            let label = if total { "Σ" } else { sources[index].market.exchange.label() };
            let color = if total { TEXT } else { venue_color(sources[index].market.exchange) };
            let header = Rect::from_min_max(col.left_top(), Pos2::new(col.right(), top));
            let hover = ui.interact(header, ui.id().with(("venue", index)), Sense::hover());
            if hover.hovered() && !total { highlighted = Some(sources[index].index); }
            hover.on_hover_text(if total {
                "Visible sources only. Totals require the same market type and quote currency, live books, and coverage at this price. — means unavailable, not zero.".to_owned()
            } else { format!("{} {} {} · {} / {}", label, sources[index].market.kind.label(), sources[index].market.symbol,
                base, sources[index].info.quote) });
            painter.with_clip_rect(header).text(Pos2::new(col.center().x, content.top() + 10.0), Align2::CENTER_CENTER,
                if total && !compatible { "Σ —" } else { label }, font.clone(), color);
            if !settings.compact {
                for (x, text, color) in [(col.left() + source_width / 4.0, "BID", GREEN), (col.right() - source_width / 4.0, "ASK", RED)] {
                    painter.text(Pos2::new(x, top - 9.0), Align2::CENTER_CENTER, text, FontId::monospace(9.0), color);
                }
            }
            for row_index in 0..count {
                let price = top_price - step * Decimal::from(row_index);
                if price <= Decimal::ZERO { continue }
                let y = top + row_index as f32 * ROW_HEIGHT;
                let cell = Rect::from_min_max(Pos2::new(col.left(), y), Pos2::new(col.right(), y + ROW_HEIGHT));
                let row = if total { total_row(&rows, price) } else { rows[index].get(&price).copied().unwrap_or_default() };
                let known = |bid| if total { compatible && sources.iter().all(|source| source.covered(price, step, bid)) }
                    else { sources[index].covered(price, step, bid) };
                let hovered = ui.rect_contains_pointer(cell.intersect(ui.clip_rect()));
                if hovered {
                    if !total { highlighted = Some(sources[index].index); }
                    inspected_row = Some((index, price, row, known(true), known(false)));
                    painter.rect_filled(cell, 0.0, Color32::from_white_alpha(10));
                }
                let buy = row.bid.value(settings.units); let sell = row.ask.value(settings.units);
                for (bid, value, color) in [(true, buy, GREEN), (false, sell, RED)] {
                    let mut area = cell;
                    if settings.compact {
                        if (buy > 0.0 && sell > 0.0) || !known(true) || !known(false) {
                            if bid { area.max.y = area.center().y; } else { area.min.y = area.center().y; }
                        } else if value == 0.0 { continue }
                    } else if bid { area.max.x = area.center().x; } else { area.min.x = area.center().x; }
                    paint_size(&painter, area.shrink2(Vec2::new(1.0, 0.5)), value, known(bid), color,
                        if total { maximum * sources.len() as f64 } else { maximum }, &font);
                }
                if !total && (row.own.buy.base > 0.0 || row.own.sell.base > 0.0) {
                    painter.rect_stroke(cell.shrink(1.0), 0.0, Stroke::new(1.0, TEXT.gamma_multiply(0.6)), egui::StrokeKind::Inside);
                }
                painter.line_segment([cell.left_bottom(), cell.right_bottom()], Stroke::new(0.5, BORDER));
            }
            painter.line_segment([col.left_top(), col.left_bottom()], Stroke::new(0.5, BORDER));
        }
    });
    let price_area = Rect::from_min_max(
        Pos2::new(ladder.left(), rect.top() + 24.0),
        Pos2::new(ladder.left() + price_width, rect.bottom()),
    );
    let price_painter = painter.with_clip_rect(price_area);
    price_painter.rect_filled(price_area, 0.0, SURFACE);
    price_painter.text(
        Pos2::new(price_area.center().x, price_area.top() + 10.0),
        Align2::CENTER_CENTER,
        "PRICE",
        FontId::monospace(10.0),
        MUTED,
    );
    let price_x = price_area.left();
    for row in 0..count {
        let price = top_price - step * Decimal::from(row);
        if price <= Decimal::ZERO {
            continue;
        }
        let cell = Rect::from_min_max(
            Pos2::new(price_x, top + row as f32 * ROW_HEIGHT),
            Pos2::new(price_x + price_width, top + (row + 1) as f32 * ROW_HEIGHT),
        );
        for (best, color) in [(bid, GREEN), (ask, RED)] {
            if best.is_some_and(|best| group(best, step) == price) {
                price_painter.rect_filled(cell, 0.0, color.gamma_multiply(0.18));
            }
        }
        price_painter.text(
            cell.center(),
            Align2::CENTER_CENTER,
            price.normalize().to_string(),
            FontId::monospace(10.0),
            TEXT,
        );
        price_painter.line_segment(
            [cell.left_bottom(), cell.right_bottom()],
            Stroke::new(0.5, BORDER),
        );
    }

    if settings.trade_stream {
        for row_index in 0..count {
            let price = top_price - step * Decimal::from(row_index);
            let own = total_row(&rows, price).own;
            let y = top + (row_index as f32 + 0.5) * ROW_HEIGHT;
            for (size, color, buy) in [(own.buy.base, GREEN, true), (own.sell.base, RED, false)] {
                if size > 0.0 {
                    let both = own.buy.base > 0.0 && own.sell.base > 0.0;
                    painter.with_clip_rect(plot).line_segment(
                        [
                            Pos2::new(
                                if both && !buy {
                                    plot.center().x
                                } else {
                                    plot.left()
                                },
                                y,
                            ),
                            Pos2::new(
                                if both && buy {
                                    plot.center().x
                                } else {
                                    plot.right()
                                },
                                y,
                            ),
                        ],
                        Stroke::new(1.0, color.gamma_multiply(0.35)),
                    );
                }
            }
        }
        for (index, source) in sources.iter().enumerate() {
            if let Some(data) = source.data {
                trade_flow(
                    &painter,
                    plot,
                    data,
                    top_price,
                    step,
                    now_ms,
                    window,
                    if highlighted == Some(source.index) {
                        pointer
                    } else {
                        None
                    },
                    source.multiplier,
                    venue_color(source.market.exchange),
                    index == 0,
                    highlighted.is_some_and(|index| index != source.index),
                );
            }
        }
        let footer = painter.with_clip_rect(Rect::from_min_max(
            Pos2::new(plot.left(), plot.bottom() + 22.0),
            Pos2::new(ladder.left() - 6.0, rect.bottom()),
        ));
        if let Some((index, trade, _)) = nearest {
            let source = sources.iter().find(|source| source.index == index).unwrap();
            let price = trade.exact_price.unwrap() / source.multiplier;
            let size = trade.size * source.multiplier.to_f64().unwrap_or(1.0);
            footer.text(
                Pos2::new(plot.left(), plot.bottom() + 30.0),
                Align2::LEFT_CENTER,
                format!(
                    "{} · {} UTC · {}",
                    source.market.exchange.label(),
                    trade_utc(trade.time_ms, false),
                    if trade.side == TradeSide::Buy {
                        "BUY"
                    } else if trade.side == TradeSide::Sell {
                        "SELL"
                    } else {
                        "TRADE"
                    }
                ),
                FontId::monospace(9.0),
                trade_color(trade.side, MUTED),
            );
            footer.text(
                Pos2::new(plot.left(), plot.bottom() + 44.0),
                Align2::LEFT_CENTER,
                format!(
                    "{} × {} {base} · {} {}",
                    price.normalize(),
                    amount(size),
                    amount(trade.price * trade.size),
                    source.info.quote
                ),
                FontId::monospace(9.0),
                TEXT,
            );
        } else if let Some((index, price, row, bid_known, ask_known)) = inspected_row {
            let name = sources
                .get(index)
                .map_or("Σ", |source| source.market.exchange.label());
            footer.text(
                Pos2::new(plot.left(), plot.bottom() + 30.0),
                Align2::LEFT_CENTER,
                format!(
                    "{name} · {} ≤ price < {}",
                    price.normalize(),
                    (price + step).normalize()
                ),
                FontId::monospace(9.0),
                MUTED,
            );
            let own = if row.own.buy.base > 0.0 || row.own.sell.base > 0.0 {
                format!(
                    " · OWN B {} / S {}",
                    amount(row.own.buy.value(settings.units)),
                    amount(row.own.sell.value(settings.units))
                )
            } else {
                String::new()
            };
            footer.text(
                Pos2::new(plot.left(), plot.bottom() + 44.0),
                Align2::LEFT_CENTER,
                format!(
                    "BID {} · ASK {}{own} {unit}",
                    if bid_known {
                        amount(row.bid.value(settings.units))
                    } else {
                        "—".into()
                    },
                    if ask_known {
                        amount(row.ask.value(settings.units))
                    } else {
                        "—".into()
                    }
                ),
                FontId::monospace(9.0),
                TEXT,
            );
        }
    }
    let mut hovered_fill = false;
    if settings.trade_stream && settings.show_fills {
        hovered_fill =
            crate::fills::overlay(ui, &response, plot, own_fills, catalogs, |index, fill| {
                let source = sources.iter().find(|source| source.index == index)?;
                if !(start_ms..=now_ms).contains(&fill.time_ms) {
                    return None;
                }
                let price = positive(&fill.price)? / source.multiplier;
                Some((
                    Pos2::new(x_for(fill.time_ms), level_y(price, top_price, step, plot)),
                    source.multiplier.to_u64()?,
                ))
            });
    }
    if settings.trade_stream && !hovered_fill {
        order_hover(
            ui,
            &response,
            plot,
            orders.iter().filter_map(|(index, account, order)| {
                let source = sources
                    .iter()
                    .find(|source| source.index == *index && source.book().is_some())?;
                let price = positive(&order.price)? / source.multiplier;
                positive(&order.size)?;
                Some((
                    source.market,
                    *account,
                    *order,
                    source.multiplier.to_u64()?,
                    level_y(price, top_price, step, plot),
                ))
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use terminal_core::{Level, Trade};

    fn level(price: &str, size: &str) -> Level {
        Level {
            price: price.into(),
            size: size.into(),
            quote_size: String::new(),
            depth_base: String::new(),
            depth_quote: String::new(),
        }
    }

    fn fixtures() -> (
        Vec<Market>,
        HashMap<Market, MarketData>,
        HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
    ) {
        let mut markets = Vec::new();
        let mut data = HashMap::new();
        let mut catalogs = HashMap::new();
        let now = crate::utc_now_ms();
        for (exchange, base, price, ask, size) in [
            (Exchange::Binance, "1000PEPE", "0.004308", "0.004310", "2"),
            (Exchange::Bybit, "1000PEPE", "0.004308", "0.004310", "2"),
            (Exchange::Okx, "PEPE", "0.000004308", "0.000004310", "2000"),
            (Exchange::Hyperliquid, "kPEPE", "0.004308", "0.004310", "2"),
        ] {
            let market = Market {
                exchange,
                kind: MarketKind::Perp,
                symbol: base.into(),
            };
            catalogs.insert(
                (exchange, market.kind),
                vec![SymbolInfo {
                    symbol: base.into(),
                    base: base.into(),
                    quote: "USDT".into(),
                    base_token_id: None,
                    market_id: None,
                    size_multiplier: None,
                    price_step: Some(
                        if base == "PEPE" {
                            "0.000000001"
                        } else {
                            "0.000001"
                        }
                        .into(),
                    ),
                }],
            );
            let mut feed = MarketData {
                connected: true,
                ..Default::default()
            };
            feed.book = Some(Book {
                bids: vec![level(price, size)],
                asks: vec![level(ask, size)],
                updated_at_ms: now,
            });
            for time_ms in [now - 1000, now - 100] {
                feed.push_trade(Trade {
                    time_ms,
                    price: price.into(),
                    size: size.into(),
                    side: TradeSide::Buy,
                });
            }
            data.insert(market.clone(), feed);
            markets.push(market);
        }
        (markets, data, catalogs)
    }

    #[test]
    fn normalized_rows_preserve_quote_size_and_unknown_depth() {
        let (markets, mut data, catalogs) = fixtures();
        let step: Decimal = "0.000000001".parse().unwrap();
        let price: Decimal = "0.000004308".parse().unwrap();
        let account = Account {
            exchange: Exchange::Hyperliquid,
            address: "0x1234".into(),
        };
        let order = OwnOrder {
            kind: None,
            external_id: None,
            coin: "kPEPE".into(),
            order_id: 1,
            side: TradeSide::Buy,
            price: "0.004308".into(),
            size: "3".into(),
        };
        let rows: Vec<_> = markets
            .iter()
            .enumerate()
            .map(|(index, market)| {
                let info = orderflow::symbol_info(market, &catalogs).unwrap();
                let source = Source {
                    index,
                    market,
                    info,
                    data: data.get(market),
                    multiplier: orderflow::base_units(&info.base, market.kind).1.into(),
                };
                assert!(source.covered(price, step, true));
                assert!(
                    !source.covered(price - step, step, true),
                    "missing depth must not become zero liquidity"
                );
                let rows = source.rows(
                    &[(3, &account, &order)],
                    step,
                    price - step,
                    price + step * Decimal::TEN,
                );
                assert_eq!(rows[&price].bid.base, 2000.0);
                assert!((rows[&price].bid.quote - 0.008616).abs() < 1e-12);
                rows
            })
            .collect();
        assert_eq!(total_row(&rows, price).bid.base, 8000.0);
        assert_eq!(rows[3][&price].own.buy.base, 3000.0);
        data.get_mut(&markets[0]).unwrap().connected = false;
        let source = Source {
            index: 0,
            market: &markets[0],
            info: &catalogs[&(Exchange::Binance, MarketKind::Perp)][0],
            data: data.get(&markets[0]),
            multiplier: Decimal::from(1000),
        };
        assert!(!source.covered(price, step, true));
        assert!(source.rows(&[], step, price, price + step).is_empty());
    }

    #[test]
    fn multi_dom_renders_aligned_trades_and_hides_sources_without_mixing_quotes() {
        let (markets, data, mut catalogs) = fixtures();
        let mut settings = Settings {
            grouping: Grouping::Tick,
            trade_stream: true,
            total_liquidity: true,
            ..Default::default()
        };
        for compact in [false, true] {
            settings.compact = compact;
            for hidden in [false, true] {
                let ctx = egui::Context::default();
                let mut view = View::default();
                if hidden {
                    view.hidden_sources.insert(markets[1].clone());
                }
                let mut render = || {
                    let mut output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(Rect::from_min_size(
                                Pos2::ZERO,
                                Vec2::new(1200.0, 600.0),
                            )),
                            ..Default::default()
                        },
                        |ui| {
                            super::ui(
                                ui,
                                &markets,
                                &data,
                                &catalogs,
                                &settings,
                                &mut view,
                                &[],
                                &[],
                            );
                        },
                    );
                    output.textures_delta.clear();
                    output
                };
                render();
                let output = render();
                let texts: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|shape| {
                        if let egui::Shape::Text(text) = &shape.shape {
                            Some(text)
                        } else {
                            None
                        }
                    })
                    .collect();
                assert!(texts.iter().any(|text| text.galley.job.text == "Σ"));
                assert_eq!(
                    texts
                        .iter()
                        .filter(|text| text.galley.job.text == "Bybit")
                        .count(),
                    if hidden { 1 } else { 2 }
                );
                assert_eq!(
                    texts.iter().any(|text| text.galley.job.text == "BID"),
                    !compact
                );
                let price_y = texts
                    .iter()
                    .find(|text| text.galley.job.text == "0.000004308")
                    .map(|text| text.pos.y + text.galley.size().y / 2.0)
                    .unwrap();
                let dots: Vec<_> = output
                    .shapes
                    .iter()
                    .filter_map(|shape| {
                        if let egui::Shape::Circle(dot) = &shape.shape {
                            Some(dot)
                        } else {
                            None
                        }
                    })
                    .collect();
                assert_eq!(dots.len(), if hidden { 9 } else { 12 });
                assert!(dots.iter().all(|dot| (dot.center.y - price_y).abs() < 0.01));
            }
        }
        catalogs
            .get_mut(&(Exchange::Hyperliquid, MarketKind::Perp))
            .unwrap()[0]
            .quote = "USDC".into();
        let sources: Vec<_> = markets
            .iter()
            .enumerate()
            .map(|(index, market)| Source {
                index,
                market,
                info: orderflow::symbol_info(market, &catalogs).unwrap(),
                data: data.get(market),
                multiplier: Decimal::ONE,
            })
            .collect();
        assert!(!same_market(&sources));
        let mut output = egui::Context::default().run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 600.0))),
                ..Default::default()
            },
            |ui| {
                super::ui(
                    ui,
                    &markets,
                    &data,
                    &catalogs,
                    &settings,
                    &mut View::default(),
                    &[],
                    &[],
                );
            },
        );
        output.textures_delta.clear();
        assert!(output.shapes.iter().any(
            |shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == "Σ —")
        ));
    }

    #[test]
    fn horizontal_venue_scroll_keeps_the_shared_price_ruler_fixed() {
        let (markets, data, catalogs) = fixtures();
        let settings = Settings {
            trade_stream: true,
            total_liquidity: true,
            ..Default::default()
        };
        let mut view = View::default();
        let ctx = egui::Context::default();
        let mut render = |frame: usize, wheel: bool| {
            let mut events = vec![egui::Event::PointerMoved(Pos2::new(650.0, 250.0))];
            if wheel {
                events.push(egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: Vec2::new(-180.0, 0.0),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::default(),
                });
            }
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 500.0))),
                    time: Some(frame as f64 / 60.0),
                    events,
                    ..Default::default()
                },
                |ui| {
                    super::ui(
                        ui,
                        &markets,
                        &data,
                        &catalogs,
                        &settings,
                        &mut view,
                        &[],
                        &[],
                    )
                },
            );
            output.textures_delta.clear();
            output
        };
        render(0, false);
        let before = render(1, false);
        let header = |output: &egui::FullOutput, name: &str| {
            let price_y = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) if text.galley.job.text == "PRICE" => Some(text.pos.y),
                    _ => None,
                })
                .unwrap();
            output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    egui::Shape::Text(text)
                        if text.galley.job.text == name && (text.pos.y - price_y).abs() < 0.1 =>
                    {
                        Some(text.pos.x)
                    }
                    _ => None,
                })
                .unwrap()
        };
        let price_x = header(&before, "PRICE");
        let venue_x = header(&before, "Bybit");
        render(2, true);
        for frame in 3..20 {
            render(frame, false);
        }
        let after = render(20, false);
        assert_eq!(header(&after, "PRICE"), price_x);
        assert!(
            header(&after, "Bybit") < venue_x - 50.0,
            "venue columns must actually scroll"
        );
    }

    #[test]
    fn duplicate_sources_are_drawn_once_and_different_assets_are_rejected() {
        let (mut markets, data, mut catalogs) = fixtures();
        markets.push(markets[0].clone());
        let settings = Settings {
            trade_stream: true,
            compact: true,
            ..Default::default()
        };
        let render = |catalogs: &HashMap<_, _>| {
            let mut output = egui::Context::default().run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 600.0))),
                    ..Default::default()
                },
                |ui| {
                    super::ui(
                        ui,
                        &markets,
                        &data,
                        catalogs,
                        &settings,
                        &mut View::default(),
                        &[],
                        &[],
                    )
                },
            );
            output.textures_delta.clear();
            output
        };
        let output = render(&catalogs);
        assert_eq!(
            output
                .shapes
                .iter()
                .filter(|shape| matches!(shape.shape, egui::Shape::Circle(_)))
                .count(),
            12
        );
        catalogs
            .get_mut(&(Exchange::Bybit, MarketKind::Perp))
            .unwrap()[0]
            .base = "ETH".into();
        let output = render(&catalogs);
        assert!(
            !output
                .shapes
                .iter()
                .any(|shape| matches!(shape.shape, egui::Shape::Circle(_)))
        );
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape,
            egui::Shape::Text(text) if text.galley.job.text.contains("same underlying coin"))));
    }
}
