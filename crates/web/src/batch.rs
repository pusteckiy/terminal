//! One-shot symbol changes scoped to a terminal, resolved against venue catalogs.
use crate::*;

pub struct Dialog {
    workspace: usize,
    query: String,
    selected: Option<String>,
    choices: HashMap<Market, Market>,
    focus: bool,
}

impl Dialog {
    pub fn new(workspace: usize) -> Self {
        Self {
            workspace,
            query: String::new(),
            selected: None,
            choices: HashMap::new(),
            focus: true,
        }
    }
}

struct Previous {
    id: TileId,
    market: Market,
    series: Vec<Market>,
    unavailable: Vec<Market>,
    batch_symbol: Option<String>,
    flow_source: Option<Market>,
}

pub struct Undo {
    workspace: usize,
    symbol: String,
    panes: Vec<Previous>,
    expires: f64,
}

fn restore(app: &mut TerminalApp, undo: Undo) {
    for previous in undo.panes {
        if let Some(Tile::Pane(pane)) = app.workspaces[undo.workspace].tiles.get_mut(previous.id) {
            pane.market = previous.market;
            pane.series = previous.series;
            pane.unavailable_markets = previous.unavailable;
            pane.batch_symbol = previous.batch_symbol;
            pane.orderflow.source = previous.flow_source;
            reset_views(pane);
        }
    }
}

fn sources(pane: &Pane) -> &[Market] {
    match pane.kind {
        WidgetKind::Compare | WidgetKind::Tape => &pane.series,
        WidgetKind::Fills => &[], // An account-wide log keeps its explicit filters.
        _ => std::slice::from_ref(&pane.market),
    }
}

fn markets(tree: &Tree<Pane>) -> Vec<Market> {
    let mut markets: Vec<_> = tree
        .tiles
        .iter()
        .filter_map(|(_, tile)| match tile {
            Tile::Pane(pane) => Some(sources(pane)),
            _ => None,
        })
        .flatten()
        .cloned()
        .collect();
    markets.sort_by(|a, b| {
        (a.exchange.label(), a.kind.label(), &a.symbol).cmp(&(
            b.exchange.label(),
            b.kind.label(),
            &b.symbol,
        ))
    });
    markets.dedup();
    markets
}

fn dex(symbol: &str) -> &str {
    symbol.split_once(':').map_or("", |(dex, _)| dex)
}

fn candidates(from: &Market, base: &str, catalog: &[SymbolInfo]) -> Vec<Market> {
    let quote = catalog
        .iter()
        .find(|info| info.symbol == from.symbol)
        .map(|info| &info.quote);
    catalog
        .iter()
        .filter(|info| {
            info.base.eq_ignore_ascii_case(base)
                && quote.is_some_and(|quote| info.quote.eq_ignore_ascii_case(quote))
                && (from.exchange != Exchange::Hyperliquid
                    || from.kind != MarketKind::Perp
                    || dex(&info.symbol) == dex(&from.symbol))
        })
        .map(|info| Market {
            symbol: info.symbol.clone(),
            ..from.clone()
        })
        .collect()
}

fn reset_views(pane: &mut Pane) {
    pane.chart = view::ChartView::default();
    pane.book_view = view::BookView::default();
    pane.dom_view = dom::View::default();
    pane.position_view = position::View::default();
    pane.compare_view = view::YAxisView::default();
    pane.search.clear();
}

fn coin_list(
    ui: &mut egui::Ui,
    bases: &[String],
) -> egui::scroll_area::ScrollAreaOutput<Option<String>> {
    egui::ScrollArea::vertical()
        .id_salt("batch-coins")
        .auto_shrink([false, true])
        .max_height(160.0)
        .show_rows(ui, 24.0, bases.len(), |ui, range| {
            let mut selected = None;
            for index in range {
                let base = &bases[index];
                if ui.selectable_label(false, base).clicked() {
                    selected = Some(base.clone());
                }
            }
            selected
        })
}

fn apply(app: &mut TerminalApp, dialog: &Dialog, base: &str, now: f64) -> usize {
    let tree = &mut app.workspaces[dialog.workspace];
    let mut previous = Vec::new();
    for (id, tile) in tree.tiles.iter_mut() {
        let Tile::Pane(pane) = tile else { continue };
        if sources(pane).is_empty() {
            continue;
        }
        previous.push(Previous {
            id: *id,
            market: pane.market.clone(),
            series: pane.series.clone(),
            unavailable: pane.unavailable_markets.clone(),
            batch_symbol: pane.batch_symbol.clone(),
            flow_source: pane.orderflow.source.clone(),
        });
        let old = sources(pane).to_vec();
        pane.unavailable_markets.clear();
        pane.batch_symbol = Some(base.to_owned());
        let next: Vec<_> = old
            .iter()
            .map(|from| {
                let options = app
                    .catalogs
                    .get(&(from.exchange, from.kind))
                    .map(|catalog| candidates(from, base, catalog))
                    .unwrap_or_default();
                let resolved = dialog
                    .choices
                    .get(from)
                    .filter(|choice| options.contains(choice))
                    .cloned()
                    .or_else(|| (options.len() == 1).then(|| options[0].clone()));
                resolved.unwrap_or_else(|| {
                    // Retain the venue/quote/DEX identity for a later batch change.
                    // This market is explicitly blocked from rendering/subscription.
                    let missing = from.clone();
                    pane.unavailable_markets.push(missing.clone());
                    missing
                })
            })
            .collect();
        if matches!(pane.kind, WidgetKind::Compare | WidgetKind::Tape) {
            if let Some(index) = pane
                .orderflow
                .source
                .as_ref()
                .and_then(|source| old.iter().position(|market| market == source))
            {
                pane.orderflow.source = Some(next[index].clone());
            }
            pane.series = next;
        } else {
            pane.market = next[0].clone();
        }
        reset_views(pane);
    }
    let count = previous.len();
    app.batch_undo = Some(Undo {
        workspace: dialog.workspace,
        symbol: base.to_owned(),
        panes: previous,
        expires: now + 10.0,
    });
    count
}

pub fn ui(app: &mut TerminalApp, ctx: &egui::Context) {
    if let Some(mut dialog) = app.batch_change.take() {
        let from = markets(&app.workspaces[dialog.workspace]);
        let keys: HashSet<_> = from
            .iter()
            .map(|market| (market.exchange, market.kind))
            .collect();
        app.request_catalogs(keys.clone(), HashSet::new());
        let ready = keys.iter().all(|key| app.catalogs.contains_key(key));
        let preview_was_shown = dialog.selected.is_some();
        let mut open = true;
        let mut do_apply = false;
        let mut cancel = false;
        egui::Window::new(format!("Change symbol · Terminal {}", dialog.workspace + 1))
            .id(egui::Id::new("batch-symbol"))
            .open(&mut open).collapsible(false).resizable(false)
            .default_width(380.0).anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                ui.label(RichText::new("Keep each widget’s exchange and market type.").size(11.0).color(MUTED));
                let search = ui.add(egui::TextEdit::singleline(&mut dialog.query).hint_text("Search coin, e.g. BTC").desired_width(f32::INFINITY));
                if dialog.focus { search.request_focus(); dialog.focus = false; }
                if search.changed() { dialog.selected = None; dialog.choices.clear(); }
                if !ready {
                    for key in &keys {
                        if let Some(error) = app.catalog_errors.get(key) {
                            ui.label(RichText::new(format!("{} {}: {error}", key.0.label(), key.1.label())).small().color(RED));
                            if ui.small_button("Retry").clicked() { app.request_catalogs(HashSet::from([*key]), HashSet::from([*key])); }
                        }
                    }
                    ui.label(RichText::new("Loading market catalogs…").small().color(MUTED));
                }
                let query = dialog.query.trim().to_ascii_uppercase();
                if dialog.selected.is_none() && ready {
                    let mut bases: Vec<_> = from.iter().flat_map(|market| app.catalogs.get(&(market.exchange, market.kind)).into_iter().flatten())
                        .map(|info| info.base.to_ascii_uppercase()).filter(|base| query.is_empty() || base.contains(&query)).collect();
                    bases.sort_unstable(); bases.dedup();
                    if bases.is_empty() { ui.label(RichText::new("No matching symbols").color(MUTED)); }
                    let enter = ui.input(|input| input.key_pressed(egui::Key::Enter));
                    if enter && bases.contains(&query) {
                        dialog.selected = Some(query.clone()); dialog.query = query.clone();
                    }
                    if let Some(base) = coin_list(ui, &bases).inner {
                        dialog.selected = Some(base.clone()); dialog.query = base;
                    }
                }
                if let Some(base) = dialog.selected.clone() {
                    ui.separator();
                    let mut unresolved = false;
                    let mut missing = 0;
                    egui::ScrollArea::vertical().id_salt("batch-preview").max_height(220.0).show(ui, |ui| {
                        for market in &from {
                            let options = app.catalogs.get(&(market.exchange, market.kind)).map(|catalog| candidates(market, &base, catalog)).unwrap_or_default();
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("{} {}", market.exchange.label(), market.kind.label())).size(12.0).color(MUTED));
                                if options.is_empty() {
                                    missing += 1;
                                    ui.label(RichText::new("Unavailable · paused").size(12.0).color(RED));
                                } else if options.len() == 1 {
                                    ui.label(RichText::new(&options[0].symbol).monospace().size(12.0));
                                } else {
                                    let choice = dialog.choices.get(market);
                                    unresolved |= choice.is_none_or(|choice| !options.contains(choice));
                                    egui::menu::MenuButton::new(choice.map_or("Choose market…", |market| &market.symbol)).ui(ui, |ui| {
                                        for option in options { if ui.button(&option.symbol).clicked() { dialog.choices.insert(market.clone(), option); ui.close(); } }
                                    });
                                }
                            });
                        }
                    });
                    if missing > 0 { ui.label(RichText::new("Unavailable sources pause; no old symbol is shown.").small().color(MUTED)); }
                    let count = app.workspaces[dialog.workspace].tiles.iter().filter(|(_, tile)| matches!(tile, Tile::Pane(pane) if !sources(pane).is_empty())).count();
                    let enabled = ready && !unresolved && count > 0 && missing < from.len();
                    ui.separator();
                    ui.horizontal(|ui| {
                        do_apply = ui.add_enabled(enabled, egui::Button::new(RichText::new(format!("Apply to {count} widgets")).color(BG)).fill(TEXT)).clicked()
                            || (enabled && preview_was_shown && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                        if ui.button("Cancel").clicked() { cancel = true; }
                    });
                    if app.workspaces[dialog.workspace].tiles.iter().any(|(_, tile)| matches!(tile, Tile::Pane(pane) if matches!(pane.kind, WidgetKind::Fills))) {
                        ui.label(RichText::new("FILLS keeps its account filters.").small().color(MUTED));
                    }
                }
            });
        if do_apply {
            apply(
                app,
                &dialog,
                dialog.selected.as_ref().unwrap(),
                ctx.input(|input| input.time),
            );
        } else if open && !cancel && !ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            app.batch_change = Some(dialog);
        }
    }
    if let Some(undo) = app.batch_undo.take() {
        let now = ctx.input(|input| input.time);
        if now < undo.expires {
            let mut restore = false;
            let mut dismiss = false;
            egui::Area::new(egui::Id::new("batch-undo"))
                .order(egui::Order::Foreground)
                .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -12.0))
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(format!(
                                "{} · Terminal {} updated",
                                undo.symbol,
                                undo.workspace + 1
                            ));
                            restore = ui.button("Undo").clicked();
                            dismiss = ui.small_button("×").clicked();
                        });
                    });
                });
            if restore {
                self::restore(app, undo);
            } else if !dismiss {
                ctx.request_repaint_after(Duration::from_secs_f64(undo.expires - now));
                app.batch_undo = Some(undo);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(symbol: &str, base: &str, quote: &str) -> SymbolInfo {
        serde_json::from_value(serde_json::json!({"symbol":symbol,"base":base,"quote":quote}))
            .unwrap()
    }

    fn market(exchange: Exchange, symbol: &str) -> Market {
        Market {
            exchange,
            kind: MarketKind::Perp,
            symbol: symbol.into(),
        }
    }

    #[test]
    fn coin_scrollbar_stays_at_the_right_edge_as_visible_symbol_lengths_change() {
        let ctx = egui::Context::default();
        let bases: Vec<_> = (0..40)
            .map(|index| {
                if index < 20 {
                    "A".to_owned()
                } else {
                    "LONGER_SYMBOL_NAME".to_owned()
                }
            })
            .collect();
        let mut edges = Vec::new();
        for frame in 0..2 {
            let mut frame_output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(800.0, 600.0),
                    )),
                    ..Default::default()
                },
                |ui| {
                    ui.set_max_width(380.0);
                    let right = ui.max_rect().right();
                    let mut output = coin_list(ui, &bases);
                    edges.push((ui.min_rect().right(), right));
                    if frame == 0 {
                        output.state.offset.y = 600.0;
                        output.state.store(ui.ctx(), output.id);
                    }
                },
            );
            frame_output.textures_delta.clear();
        }
        assert!(
            edges
                .iter()
                .all(|(actual, expected)| (actual - expected).abs() < 1.0),
            "the scrollbar must fill the list width instead of following visible text: {edges:?}"
        );
    }

    #[test]
    fn resolution_preserves_quote_and_hip3_dex_and_requires_ambiguous_choice() {
        let from = market(Exchange::Hyperliquid, "xyz:NVDA");
        let catalog = vec![
            info("xyz:NVDA", "NVDA", "USDC"),
            info("BTC", "BTC", "USDC"),
            info("flx:BTC", "BTC", "USDC"),
            info("xyz:BTC", "BTC", "USDC"),
        ];
        assert_eq!(
            candidates(&from, "BTC", &catalog),
            [market(Exchange::Hyperliquid, "xyz:BTC")]
        );
        let from = market(Exchange::Binance, "PONSUSDC");
        let catalog = vec![
            info("PONSUSDC", "PONS", "USDC"),
            info("BTCUSDT", "BTC", "USDT"),
            info("BTCUSDC", "BTC", "USDC"),
        ];
        assert_eq!(
            candidates(&from, "BTC", &catalog),
            [market(Exchange::Binance, "BTCUSDC")]
        );
        let catalog = vec![
            info("PONSUSDC", "PONS", "USDC"),
            info("BTC1", "BTC", "USDC"),
            info("BTC2", "BTC", "USDC"),
        ];
        assert_eq!(candidates(&from, "BTC", &catalog).len(), 2);
        assert!(candidates(&from, "BT", &catalog).is_empty());
    }

    #[test]
    fn batch_changes_every_source_in_only_one_terminal_pauses_missing_and_undo_restores() {
        let mut app = crate::tests::test_app();
        let binance = market(Exchange::Binance, "PONSUSDT");
        let gate = market(Exchange::Gate, "PONS_USDT");
        let hl = market(Exchange::Hyperliquid, "xyz:PONS");
        app.catalogs.insert(
            (Exchange::Binance, MarketKind::Perp),
            vec![
                info("PONSUSDT", "PONS", "USDT"),
                info("BTCUSDT", "BTC", "USDT"),
            ],
        );
        app.catalogs.insert(
            (Exchange::Gate, MarketKind::Perp),
            vec![info("PONS_USDT", "PONS", "USDT")],
        );
        app.catalogs.insert(
            (Exchange::Hyperliquid, MarketKind::Perp),
            vec![
                info("xyz:PONS", "PONS", "USDC"),
                info("xyz:BTC", "BTC", "USDC"),
            ],
        );
        let mut tiles = Tiles::default();
        let dom = tiles.insert_pane(Pane::new(WidgetKind::Dom, binance.clone()));
        let mut prices = Pane::new(WidgetKind::Compare, binance.clone());
        prices.series = vec![binance, hl.clone()];
        prices.orderflow.source = Some(hl.clone());
        prices.compare_window_secs = 600;
        let prices = tiles.insert_pane(prices);
        let book = tiles.insert_pane(Pane::new(WidgetKind::Book, gate.clone()));
        let fills = tiles.insert_pane(Pane::new(WidgetKind::Fills, hl));
        let root = tiles.insert_horizontal_tile(vec![dom, prices, book, fills]);
        app.workspaces[0] = Tree::new("batch-test", root, tiles);
        let before = serde_json::to_string(&app.workspaces).unwrap();
        let other = serde_json::to_string(&app.workspaces[1]).unwrap();
        assert_eq!(apply(&mut app, &Dialog::new(0), "BTC", 0.0), 3);
        assert_eq!(
            app.workspaces[0]
                .tiles
                .get_pane(&dom)
                .unwrap()
                .market
                .symbol,
            "BTCUSDT"
        );
        let pane = app.workspaces[0].tiles.get_pane(&prices).unwrap();
        assert_eq!(
            pane.series
                .iter()
                .map(|m| m.symbol.as_str())
                .collect::<Vec<_>>(),
            ["BTCUSDT", "xyz:BTC"]
        );
        assert_eq!(pane.compare_window_secs, 600);
        assert_eq!(pane.orderflow.source.as_ref().unwrap().symbol, "xyz:BTC");
        assert_eq!(serde_json::to_string(&app.workspaces[1]).unwrap(), other);
        let blocked = app.workspaces[0].tiles.get_pane(&book).unwrap();
        assert_eq!(blocked.unavailable_markets, [gate.clone()]);
        assert_eq!(blocked.batch_symbol.as_deref(), Some("BTC"));
        let saved: Tree<Pane> =
            serde_json::from_str(&serde_json::to_string(&app.workspaces[0]).unwrap()).unwrap();
        assert_eq!(
            saved.tiles.get_pane(&book).unwrap().unavailable_markets,
            [gate.clone()]
        );
        app.socket_open = true;
        app.sync_subscriptions();
        assert!(!app.subscribed.contains(&gate));
        let undo = app.batch_undo.take().unwrap();
        restore(&mut app, undo);
        assert_eq!(serde_json::to_string(&app.workspaces).unwrap(), before);
        apply(&mut app, &Dialog::new(0), "BTC", 0.0);
        apply(&mut app, &Dialog::new(0), "PONS", 1.0);
        assert!(
            app.workspaces[0]
                .tiles
                .get_pane(&book)
                .unwrap()
                .unavailable_markets
                .is_empty()
        );
    }
}
