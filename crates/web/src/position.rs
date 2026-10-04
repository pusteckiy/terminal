use std::collections::{HashMap, HashSet, VecDeque};

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use terminal_core::{Account, AccountState, Exchange, Market, MarketKind, SymbolInfo, TradeSide};

use crate::{BORDER, GREEN, MUTED, MarketData, RED, fills, utc_now_ms, view};

const MAX_POINTS: usize = 20_000;
const RETAIN_MS: i64 = 602_000;

#[derive(Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum Metric {
    #[default]
    Size,
    Notional,
    Pnl,
}

impl Metric {
    fn label(self) -> &'static str {
        match self {
            Self::Size => "Size",
            Self::Notional => "Notional",
            Self::Pnl => "uPnL",
        }
    }
    fn value(self, values: Values) -> Option<f64> {
        match self {
            Self::Size => Some(values.size),
            Self::Notional => values.notional,
            Self::Pnl => values.pnl,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub metric: Metric,
    pub window_secs: u32,
    accounts: Option<Vec<Account>>,
    sum: bool,
    show_fills: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            metric: Metric::Size,
            window_secs: 600,
            accounts: None,
            sum: false,
            show_fills: true,
        }
    }
}

impl Settings {
    fn accepts(&self, account: &Account, market: &Market, hidden: &HashSet<Account>) -> bool {
        account.exchange == market.exchange
            && !hidden.contains(account)
            && self
                .accounts
                .as_ref()
                .is_none_or(|list| list.contains(account))
    }
}

pub fn is_live(
    settings: &Settings,
    market: &Market,
    accounts: &[Account],
    hidden: &HashSet<Account>,
    states: &HashMap<Account, AccountState>,
) -> bool {
    let selected: Vec<_> = accounts
        .iter()
        .filter(|account| settings.accepts(account, market, hidden))
        .collect();
    !selected.is_empty()
        && selected.iter().all(|account| {
            states.get(*account).is_some_and(|state| {
                state.error.is_none()
                    && if market.kind == MarketKind::Spot {
                        state.spot_updated_at_ms > 0
                    } else {
                        state.positions_updated_at_ms > 0
                    }
            })
        })
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Values {
    size: f64,
    notional: Option<f64>,
    pnl: Option<f64>,
    entry: Option<f64>,
}

#[derive(Clone, Copy)]
struct Point {
    time_ms: i64,
    values: Option<Values>,
}

#[derive(Default)]
struct History {
    points: VecDeque<Point>,
}

impl History {
    fn push(&mut self, time_ms: i64, values: Option<Values>) {
        if self
            .points
            .back()
            .is_none_or(|point| point.values != values)
        {
            // Local observation time is monotonic within a stream; several events
            // in the same millisecond remain in order instead of being downsampled.
            let time_ms = self
                .points
                .back()
                .map_or(time_ms, |p| time_ms.max(p.time_ms));
            self.points.push_back(Point { time_ms, values });
        }
        while self.points.len() > MAX_POINTS
            || (self.points.len() > 1 && self.points[1].time_ms < time_ms - RETAIN_MS)
        {
            self.points.pop_front();
        }
    }

    fn at(&self, time_ms: i64) -> Option<Values> {
        let index = self
            .points
            .partition_point(|point| point.time_ms <= time_ms);
        self.points.get(index.checked_sub(1)?)?.values
    }
}

#[derive(Default)]
pub struct View {
    market: Option<Market>,
    histories: HashMap<Account, History>,
    metric: Option<Metric>,
    y_axis: view::YAxisView,
}

fn number(value: &str) -> Option<f64> {
    value.parse::<f64>().ok().filter(|value| value.is_finite())
}

fn values(
    market: &Market,
    state: &AccountState,
    info: Option<&SymbolInfo>,
    data: Option<&MarketData>,
) -> Option<Values> {
    if state.error.is_some() {
        return None;
    }
    if market.kind == MarketKind::Perp {
        if state.positions_updated_at_ms <= 0 {
            return None;
        }
        let mut current =
            if let Some(position) = state.positions.iter().find(|p| p.coin == market.symbol) {
                let size = number(&position.size)?;
                Values {
                    size,
                    notional: number(&position.notional_value).map(|v| v.abs() * size.signum()),
                    pnl: number(&position.unrealized_pnl),
                    entry: number(&position.entry_price),
                }
            } else {
                // A complete, valid snapshot without this position confirms it is flat.
                Values {
                    size: 0.0,
                    notional: Some(0.0),
                    pnl: Some(0.0),
                    entry: None,
                }
            };
        if let Some(live) = state.live_positions.iter().find(|p| {
            p.coin == market.symbol && p.time_ms > state.position_snapshot_time(&market.symbol)
        }) {
            let size = number(&live.size)?;
            if size != current.size {
                // The fill confirms Size. Valuation/entry for that new size remain
                // unavailable until account state catches up; don't invent uPnL.
                current.notional = (size == 0.0).then_some(0.0);
                current.pnl = (size == 0.0).then_some(0.0);
                current.entry = None;
            }
            current.size = size;
        }
        return Some(current);
    }
    if state.spot_updated_at_ms <= 0 {
        return None;
    }
    let token = info?.base_token_id?;
    let size = state
        .spot_balances
        .iter()
        .find(|balance| balance.token == token)
        .map_or(Some(0.0), |balance| number(&balance.total))?;
    let mid = data
        .filter(|data| data.connected)
        .and_then(|data| data.best_bid_ask.as_ref())
        .and_then(|quote| number(&quote.bid).zip(number(&quote.ask)))
        .filter(|(bid, ask)| *bid > 0.0 && ask >= bid)
        .map(|(bid, ask)| (bid + ask) / 2.0);
    Some(Values {
        size,
        notional: if size == 0.0 {
            Some(0.0)
        } else {
            mid.map(|mid| size * mid)
        },
        pnl: None,
        entry: None,
    })
}

impl View {
    pub fn observe(
        &mut self,
        market: &Market,
        accounts: &[Account],
        states: &HashMap<Account, AccountState>,
        info: Option<&SymbolInfo>,
        data: Option<&MarketData>,
        now_ms: i64,
    ) {
        if self.market.as_ref() != Some(market) {
            self.histories.clear();
            self.market = Some(market.clone());
            self.metric = None;
            self.y_axis = view::YAxisView::default();
        }
        self.histories
            .retain(|account, _| accounts.contains(account));
        for account in accounts
            .iter()
            .filter(|account| account.exchange == market.exchange)
        {
            let current = states
                .get(account)
                .and_then(|state| values(market, state, info, data));
            let time = states.get(account).map_or(now_ms, |state| {
                if market.kind != MarketKind::Perp || current.is_none() {
                    return now_ms;
                }
                let time = state.position_snapshot_time(&market.symbol).max(
                    state
                        .live_positions
                        .iter()
                        .filter(|p| p.coin == market.symbol)
                        .map(|p| p.time_ms)
                        .max()
                        .unwrap_or(0),
                );
                if time > 0 { time } else { now_ms }
            });
            self.histories
                .entry(account.clone())
                .or_default()
                .push(time, current);
        }
    }
}

pub fn config_ui(
    ui: &mut egui::Ui,
    settings: &mut Settings,
    market: &mut Market,
    search: &mut String,
    accounts: &[Account],
    hidden: &HashSet<Account>,
    states: &HashMap<Account, AccountState>,
    catalogs: &HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
    errors: &HashMap<(Exchange, MarketKind), String>,
    retry: &mut HashSet<(Exchange, MarketKind)>,
) {
    ui.weak("Hyperliquid");
    ui.horizontal(|ui| {
        for kind in MarketKind::ALL {
            if ui
                .selectable_label(market.kind == kind, kind.label())
                .clicked()
                && market.kind != kind
            {
                *market = Market::for_exchange_kind(Exchange::Hyperliquid, kind);
                search.clear();
                if kind == MarketKind::Spot && settings.metric == Metric::Pnl {
                    settings.metric = Metric::Size;
                }
            }
        }
    });
    ui.horizontal(|ui| {
        for metric in [Metric::Size, Metric::Notional, Metric::Pnl] {
            let supported = metric != Metric::Pnl || market.kind == MarketKind::Perp;
            if ui
                .add_enabled(
                    supported,
                    egui::Button::new(metric.label()).selected(settings.metric == metric),
                )
                .on_hover_text(if metric == Metric::Notional {
                    "Signed exposure. Perp: exchange valuation; Spot: current mid-price."
                } else if metric == Metric::Pnl {
                    "Exchange-reported unrealized PnL, available for Perp."
                } else {
                    "Signed Perp position or total Spot balance, including held funds."
                })
                .clicked()
            {
                settings.metric = metric;
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label("Window");
        ui.add(
            egui::DragValue::new(&mut settings.window_secs)
                .range(10..=600)
                .suffix(" s"),
        );
    });
    ui.checkbox(&mut settings.show_fills, "Show fills");
    ui.checkbox(&mut settings.sum, "Sum selected accounts");
    ui.separator();
    ui.label(egui::RichText::new("ACCOUNTS").small().color(MUTED));
    let mut all = settings.accounts.is_none();
    if ui.checkbox(&mut all, "All visible accounts").changed() {
        settings.accounts = if all { None } else { Some(Vec::new()) };
    }
    for account in accounts.iter().filter(|a| a.exchange == market.exchange) {
        let mut selected = settings
            .accounts
            .as_ref()
            .is_none_or(|list| list.contains(account));
        if ui
            .add_enabled(
                !hidden.contains(account),
                egui::Checkbox::new(&mut selected, wallet(account)),
            )
            .on_hover_text(&account.address)
            .changed()
        {
            let list = settings.accounts.get_or_insert_with(|| {
                accounts
                    .iter()
                    .filter(|a| a.exchange == market.exchange && !hidden.contains(*a))
                    .cloned()
                    .collect()
            });
            list.retain(|a| a != account);
            if selected {
                list.push(account.clone());
            }
        }
    }
    ui.separator();
    ui.label(egui::RichText::new("SYMBOL").small().color(MUTED));
    ui.add(
        egui::TextEdit::singleline(search)
            .hint_text("Search symbol")
            .desired_width(260.0),
    );
    let info = catalogs.get(&(market.exchange, market.kind));
    let mut symbols: Vec<_> = info
        .into_iter()
        .flatten()
        .map(|info| {
            (
                info.symbol.clone(),
                format!("{} / {} · {}", info.base, info.quote, info.symbol),
            )
        })
        .collect();
    if market.kind == MarketKind::Perp {
        for position in states.values().flat_map(|state| &state.positions) {
            if !symbols.iter().any(|(coin, _)| coin == &position.coin) {
                symbols.push((position.coin.clone(), position.coin.clone()));
            }
        }
    }
    symbols.sort_by(|a, b| a.0.cmp(&b.0));
    symbols.dedup_by(|a, b| a.0 == b.0);
    let query = search.trim().to_ascii_uppercase();
    symbols.retain(|(_, label)| label.to_ascii_uppercase().contains(&query));
    egui::ScrollArea::vertical()
        .id_salt("position_symbols")
        .max_height(120.0)
        .show_rows(ui, 22.0, symbols.len(), |ui, range| {
            for index in range {
                let (coin, label) = &symbols[index];
                if ui.selectable_label(market.symbol == *coin, label).clicked() {
                    market.symbol.clone_from(coin);
                }
            }
        });
    if info.is_none() {
        if let Some(error) = errors.get(&(market.exchange, market.kind)) {
            ui.colored_label(RED, error);
            if ui.button("Retry symbols").clicked() {
                retry.insert((market.exchange, market.kind));
            }
        } else {
            ui.weak("Loading symbols…");
        }
    }
}

fn wallet(account: &Account) -> String {
    format!(
        "{}…{}",
        &account.address[..account.address.len().min(6)],
        &account.address[account.address.len().saturating_sub(4)..]
    )
}

fn text(value: f64) -> String {
    if value != 0.0 && value.abs() < 1e-8 {
        return format!("{value:.2e}");
    }
    if value.abs() >= 1_000.0 {
        let amount = view::quote_size_text(&value.abs().to_string());
        return if value < 0.0 {
            format!("−{amount}")
        } else {
            amount
        };
    }
    format!("{value:.8}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

fn axis_text(value: f64, tick_step: f64) -> String {
    if value.abs() >= 1_000.0 || (value != 0.0 && value.abs() < 1e-6) {
        return text(value);
    }
    let decimals = (1.0 - tick_step.abs().log10().floor()).clamp(0.0, 8.0) as usize;
    format!("{value:.decimals$}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

fn auto_bounds(values: impl Iterator<Item = f64>) -> (f64, f64) {
    let bounds =
        values
            .filter(|value| value.is_finite())
            .fold(None::<(f64, f64)>, |bounds, value| {
                Some(bounds.map_or((value, value), |(min, max)| {
                    (min.min(value), max.max(value))
                }))
            });
    let (min, max) = bounds.unwrap_or((0.0, 0.0));
    let magnitude = min.abs().max(max.abs());
    let padding = if max > min {
        // Scale to the visible variation, even when the position is far from zero.
        ((max - min) * 0.08).max(magnitude * f64::EPSILON * 16.0)
    } else if magnitude > 0.0 {
        magnitude * 0.001
    } else {
        1.0
    }
    .max(1e-8);
    (min - padding, max + padding)
}

struct Line<'a> {
    accounts: Vec<&'a Account>,
    points: Vec<(i64, Option<f64>)>,
    color: Option<Color32>,
}

fn total_at(view: &View, accounts: &[&Account], metric: Metric, time_ms: i64) -> Option<f64> {
    accounts.iter().try_fold(0.0, |sum, account| {
        Some(sum + metric.value(view.histories.get(*account)?.at(time_ms)?)?)
    })
}

fn fill_size_after(fill: &terminal_core::OwnFill) -> Option<f64> {
    use rust_decimal::{Decimal, prelude::ToPrimitive};
    let start = fill.start_position.as_deref()?.parse::<Decimal>().ok()?;
    let size = fill.size.parse::<Decimal>().ok()?;
    let end = match fill.side {
        TradeSide::Buy => start.checked_add(size)?,
        TradeSide::Sell => start.checked_sub(size)?,
        _ => return None,
    };
    end.to_f64()
}

pub fn ui(
    ui: &mut egui::Ui,
    market: &Market,
    settings: &Settings,
    view: &mut View,
    accounts: &[Account],
    hidden: &HashSet<Account>,
    states: &HashMap<Account, AccountState>,
    data: Option<&MarketData>,
    info: Option<&SymbolInfo>,
    fills: &fills::Store,
) {
    let now = utc_now_ms();
    view.observe(market, accounts, states, info, data, now);
    let selected: Vec<_> = accounts
        .iter()
        .filter(|account| settings.accepts(account, market, hidden))
        .collect();
    if selected.is_empty() {
        ui.centered_and_justified(|ui| {
            ui.weak(if accounts.is_empty() {
                "Connect an account to see your position"
            } else {
                "No visible accounts selected"
            });
        });
        return;
    }
    let metric = if market.kind == MarketKind::Spot && settings.metric == Metric::Pnl {
        Metric::Size
    } else {
        settings.metric
    };
    let unit = if metric == Metric::Size {
        info.map_or(market.symbol.as_str(), |i| i.base.as_str())
    } else if market.kind == MarketKind::Perp {
        "USD"
    } else {
        info.map_or("quote", |i| i.quote.as_str())
    };
    // Initially show the history we have actually observed, then roll at the
    // configured window length. Avoid ten minutes of empty space on startup.
    let first_observation = selected
        .iter()
        .filter_map(|account| view.histories.get(*account))
        .flat_map(|history| {
            history
                .points
                .iter()
                .find(|point| point.values.is_some())
                .map(|point| point.time_ms)
        })
        .min()
        .unwrap_or(now);
    let start = (now - i64::from(settings.window_secs.clamp(10, 600)) * 1_000)
        .max(first_observation.min(now - 10_000));
    let groups: Vec<Vec<&Account>> = if settings.sum {
        vec![selected.clone()]
    } else {
        selected.iter().map(|account| vec![*account]).collect()
    };
    let lines: Vec<_> = groups
        .into_iter()
        .enumerate()
        .map(|(index, group)| {
            let points = if group.len() == 1 {
                let mut points = vec![(start, total_at(view, &group, metric, start))];
                if let Some(history) = view.histories.get(group[0]) {
                    points.extend(
                        history
                            .points
                            .iter()
                            .filter(|point| point.time_ms > start && point.time_ms <= now)
                            .map(|point| {
                                (
                                    point.time_ms,
                                    point.values.and_then(|values| metric.value(values)),
                                )
                            }),
                    );
                }
                points.push((now, total_at(view, &group, metric, now)));
                points
            } else {
                let mut times: Vec<_> = group
                    .iter()
                    .flat_map(|account| {
                        view.histories
                            .get(*account)
                            .into_iter()
                            .flat_map(|history| history.points.iter().map(|p| p.time_ms))
                    })
                    .filter(|time| *time > start && *time <= now)
                    .collect();
                times.extend([start, now]);
                times.sort_unstable();
                times.dedup();
                times
                    .into_iter()
                    .map(|time| (time, total_at(view, &group, metric, time)))
                    .collect()
            };
            Line {
                accounts: group,
                points,
                color: if selected.len() > 1 && !settings.sum {
                    Some(view::SERIES_COLORS[index % view::SERIES_COLORS.len()])
                } else {
                    None
                },
            }
        })
        .collect();
    ui.horizontal_wrapped(|ui| {
        ui.weak(format!("{} · {unit}", metric.label().to_ascii_uppercase()));
        for line in &lines {
            let current = line.points.last().and_then(|(_, v)| *v);
            let color = line
                .color
                .unwrap_or_else(|| sign_color(current.unwrap_or(0.0)));
            ui.colored_label(
                color,
                egui::RichText::new(format!(
                    "{}  {}",
                    if settings.sum {
                        "Σ".into()
                    } else {
                        wallet(line.accounts[0])
                    },
                    current.map_or("—".into(), text)
                ))
                .monospace()
                .size(10.0),
            );
        }
    });
    if view.metric != Some(metric) {
        view.metric = Some(metric);
        view.y_axis = view::YAxisView::default();
    }
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
    let plot = Rect::from_min_max(
        rect.left_top() + Vec2::new(12.0, 12.0),
        rect.right_bottom() - Vec2::new(78.0, 28.0),
    );
    if plot.width() < 40.0 || plot.height() < 30.0 {
        return;
    }
    if lines
        .iter()
        .all(|line| line.points.iter().all(|(_, value)| value.is_none()))
    {
        ui.painter().text(
            plot.center(),
            Align2::CENTER_CENTER,
            if metric == Metric::Notional && market.kind == MarketKind::Spot {
                "Waiting for balance and live price…"
            } else {
                "Waiting for account state…"
            },
            FontId::proportional(12.0),
            MUTED,
        );
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
        return;
    }
    let (min, max) = auto_bounds(
        lines
            .iter()
            .flat_map(|line| line.points.iter().filter_map(|(_, v)| *v)),
    );
    let (min, max) = view
        .y_axis
        .interact(ui, &response, rect, plot, (min, max), false);
    let pos = |time: i64, value: f64| {
        Pos2::new(
            plot.left() + ((time - start) as f64 / (now - start) as f64) as f32 * plot.width(),
            plot.bottom() - ((value - min) / (max - min)) as f32 * plot.height(),
        )
    };
    let painter = ui.painter_at(rect);
    for i in 0..=4 {
        let value = max - (max - min) * i as f64 / 4.0;
        let y = pos(start, value).y;
        painter.line_segment(
            [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
            Stroke::new(0.5, BORDER),
        );
        if (y - pos(start, 0.0).y).abs() >= 12.0 {
            painter.text(
                Pos2::new(rect.right() - 6.0, y),
                Align2::RIGHT_CENTER,
                axis_text(value, (max - min) / 4.0),
                FontId::monospace(10.0),
                MUTED,
            );
        }
    }
    if min <= 0.0 && max >= 0.0 {
        painter.line_segment([pos(start, 0.0), pos(now, 0.0)], Stroke::new(1.0, MUTED));
        painter.text(
            Pos2::new(rect.right() - 6.0, pos(start, 0.0).y),
            Align2::RIGHT_CENTER,
            "0",
            FontId::monospace(10.0),
            MUTED,
        );
    }
    let ticks = ((plot.width() / 95.0) as usize).clamp(2, 10);
    for i in 0..=ticks {
        let time = start + (now - start) * i as i64 / ticks as i64;
        painter.text(
            Pos2::new(pos(time, 0.0).x, plot.bottom() + 15.0),
            if i == 0 {
                Align2::LEFT_CENTER
            } else if i == ticks {
                Align2::RIGHT_CENTER
            } else {
                Align2::CENTER_CENTER
            },
            &fills::utc(time)[..8],
            FontId::monospace(9.0),
            MUTED,
        );
    }
    let clipped = painter.with_clip_rect(plot);
    for line in &lines {
        for pair in line.points.windows(2) {
            if let Some(left) = pair[0].1 {
                let color = line.color.unwrap_or_else(|| sign_color(left));
                clipped.line_segment(
                    [pos(pair[0].0, left), pos(pair[1].0, left)],
                    Stroke::new(1.4, color),
                );
                if let Some(right) = pair[1].1 {
                    clipped.line_segment(
                        [pos(pair[1].0, left), pos(pair[1].0, right)],
                        Stroke::new(1.4, line.color.unwrap_or_else(|| sign_color(right))),
                    );
                }
            }
        }
        if let Some(value) = line.points.last().and_then(|(_, v)| *v) {
            clipped.circle_filled(
                pos(now, value),
                2.5,
                line.color.unwrap_or_else(|| sign_color(value)),
            );
        }
    }
    let mut hovered_fill = None;
    if settings.show_fills {
        for line in &lines {
            for account in &line.accounts {
                for fill in fills
                    .histories
                    .get(*account)
                    .into_iter()
                    .flatten()
                    .filter(|fill| {
                        fill.market == *market && fill.time_ms >= start && fill.time_ms <= now
                    })
                {
                    if let Some(value) = total_at(view, &line.accounts, metric, fill.time_ms) {
                        let value = if metric == Metric::Size
                            && market.kind == MarketKind::Perp
                            && line.accounts.len() == 1
                        {
                            // Fills in the same millisecond can have different post-fill sizes.
                            fill_size_after(fill).unwrap_or(value)
                        } else {
                            value
                        };
                        let point = pos(fill.time_ms, value);
                        clipped.circle_filled(
                            point,
                            2.6,
                            if fill.side == TradeSide::Buy {
                                GREEN
                            } else {
                                RED
                            },
                        );
                        if response
                            .hover_pos()
                            .is_some_and(|cursor| cursor.distance(point) < 7.0)
                        {
                            hovered_fill = Some((*account, fill));
                        }
                    }
                }
            }
        }
    }
    if let Some(cursor) = response.hover_pos().filter(|cursor| plot.contains(*cursor)) {
        let time = start
            + (((cursor.x - plot.left()) / plot.width()) as f64 * (now - start) as f64) as i64;
        painter.line_segment(
            [
                Pos2::new(cursor.x, plot.top()),
                Pos2::new(cursor.x, plot.bottom()),
            ],
            Stroke::new(0.5, MUTED),
        );
        response.on_hover_ui_at_pointer(|ui| {
            ui.monospace(format!("{} UTC · {}", fills::utc(time), metric.label()));
            for line in &lines {
                ui.label(format!(
                    "{}: {} {unit}",
                    if settings.sum {
                        "Σ".into()
                    } else {
                        wallet(line.accounts[0])
                    },
                    total_at(view, &line.accounts, metric, time).map_or("—".into(), text)
                ));
                for account in &line.accounts {
                    if let Some(value) = view
                        .histories
                        .get(*account)
                        .and_then(|history| history.at(time))
                    {
                        ui.weak(format!(
                            "{} · size {} · notional {} · uPnL {} · entry {}",
                            wallet(account),
                            text(value.size),
                            value.notional.map_or("—".into(), text),
                            value.pnl.map_or("—".into(), text),
                            value.entry.map_or("—".into(), text)
                        ));
                    }
                }
            }
            if let Some((account, fill)) = hovered_fill {
                ui.separator();
                ui.colored_label(
                    if fill.side == TradeSide::Buy {
                        GREEN
                    } else {
                        RED
                    },
                    format!(
                        "{} {} × {} · {} UTC",
                        if fill.side == TradeSide::Buy {
                            "BUY"
                        } else {
                            "SELL"
                        },
                        fill.price,
                        fill.size,
                        fills::utc(fill.time_ms)
                    ),
                );
                ui.monospace(&account.address);
            }
            ui.weak("Perp Size updates from confirmed fills; valuation from account state. Gaps indicate unavailable data.");
        });
    }
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(100));
}

fn sign_color(value: f64) -> Color32 {
    if value > 0.0 {
        GREEN
    } else if value < 0.0 {
        RED
    } else {
        MUTED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_scale_fills_the_plot_with_visible_position_variation() {
        for values in [[-188.002, -188.001, -188.0], [188.0, 188.001, 188.002]] {
            let (min, max) = auto_bounds(values.into_iter());
            assert!(max < 0.0 || min > 0.0, "zero must not be forced into view");
            assert!(
                0.002 / (max - min) > 0.8,
                "small changes must fill the plot"
            );
            assert!(values.iter().all(|v| *v > min && *v < max));
            assert_ne!(
                axis_text(min, (max - min) / 4.0),
                axis_text(max, (max - min) / 4.0)
            );
        }
        let (min, max) = auto_bounds([-1.0, 1.0].into_iter());
        assert!(min < 0.0 && max > 0.0);
        for value in [-188.0, 0.0, 188.0] {
            let (min, max) = auto_bounds([value, value].into_iter());
            assert!(min.is_finite() && max.is_finite() && min < value && max > value);
        }
    }

    fn account() -> Account {
        Account {
            exchange: Exchange::Hyperliquid,
            address: "0x0000000000000000000000000000000000000001".into(),
        }
    }
    fn state(size: &str) -> AccountState {
        AccountState {
            connected: true,
            positions_updated_at_ms: 1_000,
            positions: vec![terminal_core::Position {
                coin: "BTC".into(),
                size: size.into(),
                entry_price: "100".into(),
                notional_value: "200".into(),
                unrealized_pnl: "-2".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn position_closure_is_zero_but_disconnect_is_a_gap() {
        let account = account();
        let market = Market::for_exchange(Exchange::Hyperliquid);
        let mut view = View::default();
        let mut states = HashMap::from([(account.clone(), state("-2"))]);
        view.observe(&market, &[account.clone()], &states, None, None, 1_000);
        assert_eq!(
            view.histories[&account].at(1_000).unwrap().notional,
            Some(-200.0)
        );
        states.get_mut(&account).unwrap().positions.clear();
        view.observe(&market, &[account.clone()], &states, None, None, 2_000);
        assert_eq!(view.histories[&account].at(2_000).unwrap().size, 0.0);
        states.clear();
        view.observe(&market, &[account.clone()], &states, None, None, 3_000);
        assert!(view.histories[&account].at(3_000).is_none());
        states.insert(account.clone(), state("3"));
        view.observe(&market, &[account.clone()], &states, None, None, 4_000);
        assert!(view.histories[&account].at(3_500).is_none());
        assert_eq!(view.histories[&account].at(4_000).unwrap().size, 3.0);
        assert!(view.histories[&account].at(999).is_none());
    }

    #[test]
    fn all_events_are_kept_and_history_is_bounded_with_boundary_value() {
        let mut history = History::default();
        for i in 0..25_000 {
            history.push(
                i,
                Some(Values {
                    size: i as f64,
                    notional: None,
                    pnl: None,
                    entry: None,
                }),
            );
        }
        assert_eq!(history.points.len(), MAX_POINTS);
        history.push(
            700_000,
            Some(Values {
                size: 1.0,
                notional: None,
                pnl: None,
                entry: None,
            }),
        );
        assert_eq!(history.points.len(), 2);
        history.push(
            700_000,
            Some(Values {
                size: 2.0,
                notional: None,
                pnl: None,
                entry: None,
            }),
        );
        assert_eq!(history.points.len(), 3);
        assert_eq!(history.at(700_000).unwrap().size, 2.0);
    }

    #[test]
    fn spot_uses_token_identity_and_includes_held_balance() {
        let market = Market::for_exchange_kind(Exchange::Hyperliquid, MarketKind::Spot);
        let info = SymbolInfo {
            symbol: market.symbol.clone(),
            base: "PURR".into(),
            quote: "USDC".into(),
            base_token_id: Some(1),
            market_id: None,
            size_multiplier: None,
            price_step: None,
        };
        let state = AccountState {
            spot_updated_at_ms: 1_000,
            spot_balances: vec![
                terminal_core::SpotBalance {
                    coin: "PURR".into(),
                    token: 99,
                    total: "500".into(),
                    hold: "0".into(),
                },
                terminal_core::SpotBalance {
                    coin: "PURR".into(),
                    token: 1,
                    total: "10".into(),
                    hold: "4".into(),
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            values(&market, &state, Some(&info), None).unwrap().size,
            10.0
        );
        assert_eq!(
            values(&market, &state, Some(&info), None).unwrap().notional,
            None
        );
        assert!(values(&market, &state, None, None).is_none());
    }

    #[test]
    fn sum_does_not_hide_missing_accounts_and_filters_respect_eye() {
        let account = account();
        let mut other = account.clone();
        other.address.replace_range(41.., "2");
        let market = Market::for_exchange(Exchange::Hyperliquid);
        let mut view = View::default();
        let states = HashMap::from([(account.clone(), state("2"))]);
        view.observe(
            &market,
            &[account.clone(), other.clone()],
            &states,
            None,
            None,
            1_000,
        );
        assert_eq!(total_at(&view, &[&account], Metric::Size, 1_000), Some(2.0));
        assert_eq!(
            total_at(&view, &[&account, &other], Metric::Size, 1_000),
            None
        );
        assert!(!Settings::default().accepts(&account, &market, &HashSet::from([account.clone()])));
    }

    #[test]
    fn widget_settings_survive_save_and_market_change_clears_history() {
        let mut pane = crate::Pane::new(crate::WidgetKind::Position, Market::default());
        pane.position.metric = Metric::Notional;
        pane.position.sum = true;
        pane.position.window_secs = 120;
        let saved = serde_json::to_string(&pane).unwrap();
        let loaded: crate::Pane = serde_json::from_str(&saved).unwrap();
        assert_eq!(loaded.market.exchange, Exchange::Hyperliquid);
        assert!(loaded.position.metric == Metric::Notional && loaded.position.sum);
        assert_eq!(loaded.position.window_secs, 120);
        let account = account();
        let mut view = View::default();
        let states = HashMap::from([(account.clone(), state("2"))]);
        view.observe(
            &loaded.market,
            &[account.clone()],
            &states,
            None,
            None,
            1_000,
        );
        let mut other = loaded.market;
        other.symbol = "ETH".into();
        view.observe(&other, &[account.clone()], &states, None, None, 2_000);
        assert!(view.histories[&account].at(1_000).is_none());
        assert_eq!(view.histories[&account].at(2_000).unwrap().size, 0.0);
    }
    #[test]
    fn live_size_is_recorded_at_fill_time_and_snapshot_ack_does_not_add_a_delayed_step() {
        let account = account();
        let market = Market::for_exchange(Exchange::Hyperliquid);
        let mut snapshot = state("-188");
        snapshot.position_snapshot_times.insert("".into(), 1_000);
        let mut states = HashMap::from([(account.clone(), snapshot)]);
        let mut view = View::default();
        view.observe(&market, &[account.clone()], &states, None, None, 5_000);
        let update = terminal_core::LivePosition {
            coin: "BTC".into(),
            size: "-187.99".into(),
            time_ms: 1_001,
        };
        states
            .get_mut(&account)
            .unwrap()
            .apply_live_position(update);
        view.observe(&market, &[account.clone()], &states, None, None, 5_010);
        assert_eq!(view.histories[&account].at(1_000).unwrap().size, -188.0);
        assert_eq!(view.histories[&account].at(1_001).unwrap().size, -187.99);
        assert_eq!(view.histories[&account].at(1_001).unwrap().pnl, None);
        let snapshot = states.get_mut(&account).unwrap();
        snapshot.live_positions.clear();
        snapshot.positions[0].size = "-187.99".into();
        snapshot.position_snapshot_times.insert("".into(), 2_000);
        view.observe(&market, &[account.clone()], &states, None, None, 9_000);
        let points = &view.histories[&account].points;
        assert_eq!(points.len(), 3);
        assert_eq!(points[2].time_ms, 2_000);
        assert_eq!(
            points[1].values.unwrap().size,
            points[2].values.unwrap().size
        );
        states.insert(account.clone(), AccountState::default());
        view.observe(&market, &[account.clone()], &states, None, None, 10_000);
        assert!(view.histories[&account].at(10_000).is_none());
    }
}
