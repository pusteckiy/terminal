//! One-shot market changes scoped to a terminal, resolved against venue catalogs.
use crate::*;

pub struct Dialog {
    workspace: usize,
    query: String,
    selected: Option<String>,
    choices: HashMap<Market, Market>,
    exchange: Option<Exchange>,
    kind: Option<MarketKind>,
    focus: bool,
    preview: bool,
}

impl Dialog {
    pub fn new(workspace: usize) -> Self {
        Self {
            workspace,
            query: String::new(),
            selected: None,
            choices: HashMap::new(),
            exchange: None,
            kind: None,
            focus: true,
            preview: false,
        }
    }

    fn choice(&self, from: &Market, options: &[Market]) -> Option<Market> {
        self.choices
            .get(from)
            .filter(|choice| options.contains(choice))
            .or_else(|| {
                options.iter().find(|option| {
                    from.exchange == Exchange::Hyperliquid
                        && from.kind == MarketKind::Perp
                        && option.exchange == from.exchange
                        && option.kind == from.kind
                        && dex(&option.symbol) == dex(&from.symbol)
                })
            })
            .or_else(|| options.first().filter(|_| options.len() == 1))
            .cloned()
    }

    fn target(&self, from: &Market) -> (Exchange, MarketKind) {
        (
            self.exchange.unwrap_or(from.exchange),
            self.kind.unwrap_or(from.kind),
        )
    }

    fn catalog_keys(&self, from: &[Market]) -> HashSet<(Exchange, MarketKind)> {
        from.iter()
            .flat_map(|market| [(market.exchange, market.kind), self.target(market)])
            .filter(|(exchange, kind)| exchange.supports(*kind))
            .collect()
    }

    fn options(
        &self,
        from: &Market,
        base: Option<&str>,
        catalogs: &HashMap<(Exchange, MarketKind), Vec<SymbolInfo>>,
    ) -> Vec<Market> {
        let (exchange, kind) = self.target(from);
        if !exchange.supports(kind) {
            return Vec::new();
        }
        let Some(catalog) = catalogs.get(&(exchange, kind)) else {
            return Vec::new();
        };
        let original = catalogs
            .get(&(from.exchange, from.kind))
            .and_then(|rows| rows.iter().find(|info| info.symbol == from.symbol));
        if (exchange, kind) == (from.exchange, from.kind) {
            return if let Some(base) = base {
                candidates(from, base, catalog)
            } else {
                catalog
                    .iter()
                    .any(|info| info.symbol == from.symbol)
                    .then(|| from.clone())
                    .into_iter()
                    .collect()
            };
        }
        let Some(base) =
            base.or_else(|| original.map(|info| orderflow::base_units(&info.base, from.kind).0))
        else {
            return Vec::new();
        };
        let rows: Vec<_> = catalog
            .iter()
            .filter(|info| {
                orderflow::base_units(&info.base, kind)
                    .0
                    .eq_ignore_ascii_case(base)
            })
            .collect();
        let same_quote = original.is_some_and(|old| {
            rows.iter()
                .any(|info| info.quote.eq_ignore_ascii_case(&old.quote))
        });
        rows.into_iter()
            .filter(|info| {
                !same_quote
                    || original.is_some_and(|old| info.quote.eq_ignore_ascii_case(&old.quote))
            })
            .map(|info| Market {
                exchange,
                kind,
                symbol: info.symbol.clone(),
            })
            .collect()
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
        WidgetKind::Compare
        | WidgetKind::Tape
        | WidgetKind::Dom
        | WidgetKind::Imbalance
        | WidgetKind::Events => &pane.series,
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
            orderflow::base_units(&info.base, from.kind)
                .0
                .eq_ignore_ascii_case(base)
                && quote.is_some_and(|quote| info.quote.eq_ignore_ascii_case(quote))
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
    pane.imbalance_view = crate::imbalance::View::default();
    pane.slippage_view = crate::slippage::View::default();
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

fn apply(app: &mut TerminalApp, dialog: &Dialog, base: Option<&str>, now: f64) -> usize {
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
        pane.batch_symbol = base.map(str::to_owned);
        let next: Vec<_> = old
            .iter()
            .map(|from| {
                let options = dialog.options(from, base, &app.catalogs);
                let resolved = dialog.choice(from, &options);
                resolved.unwrap_or_else(|| {
                    // Retain the original identity for recovery or Undo.
                    // This market is explicitly blocked from rendering/subscription.
                    let missing = from.clone();
                    pane.unavailable_markets.push(missing.clone());
                    missing
                })
            })
            .collect();
        if pane.uses_sources() {
            if let Some(index) = pane
                .orderflow
                .source
                .as_ref()
                .and_then(|source| old.iter().position(|market| market == source))
            {
                pane.orderflow.source = Some(next[index].clone());
            }
            pane.series = next;
            let mut unique = HashSet::new();
            pane.series.retain(|market| unique.insert(market.clone()));
            pane.editing_series = pane.editing_series.min(pane.series.len() - 1);
            if let Some(first) = pane.series.first() {
                pane.market = first.clone();
            }
        } else {
            pane.market = next[0].clone();
        }
        reset_views(pane);
    }
    let count = previous.len();
    app.batch_undo = Some(Undo {
        workspace: dialog.workspace,
        symbol: base.unwrap_or("Markets").to_owned(),
        panes: previous,
        expires: now + 10.0,
    });
    count
}

pub fn ui(app: &mut TerminalApp, ctx: &egui::Context) {
    if let Some(mut dialog) = app.batch_change.take() {
        let from = markets(&app.workspaces[dialog.workspace]);
        let preview_was_shown = dialog.selected.is_some() || dialog.query.trim().is_empty();
        let mut open = true;
        let mut do_apply = false;
        let mut cancel = false;
        let count = app.workspaces[dialog.workspace]
            .tiles
            .iter()
            .filter(|(_, tile)| matches!(tile, Tile::Pane(pane) if !sources(pane).is_empty()))
            .count();
        egui::Window::new("Change markets")
            .id(egui::Id::new("batch-symbol"))
            .title_bar(false).resizable(false)
            .frame(egui::Frame::window(&ctx.global_style()).inner_margin(16))
            .default_width(420.0).anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 72.0))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 10.0;
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Change markets").size(16.0));
                        ui.label(RichText::new(format!("Terminal {} · {count} widgets", dialog.workspace + 1)).size(12.0).color(MUTED));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add_sized([26.0, 26.0], egui::Button::new(RichText::new("×").size(18.0)).frame(false)).on_hover_text("Close").clicked() { open = false; }
                    });
                });
                ui.add_space(2.0);
                let previous=(dialog.exchange,dialog.kind);
                ui.columns(2, |columns| {
                    for column in columns.iter_mut() {
                        column.spacing_mut().interact_size.y = 30.0;
                        column.spacing_mut().item_spacing.y = 6.0;
                    }
                    columns[0].label(RichText::new("Exchange").size(12.0).color(MUTED));
                    egui::ComboBox::from_id_salt("batch-exchange").width(columns[0].available_width()).height(240.0).selected_text(dialog.exchange.map_or("Keep current",Exchange::label)).show_ui(&mut columns[0],|ui| {
                        ui.selectable_value(&mut dialog.exchange,None,"Keep current");
                        for exchange in Exchange::ALL { ui.selectable_value(&mut dialog.exchange,Some(exchange),exchange.label()); }
                    });
                    if previous.0!=dialog.exchange && let Some(exchange)=dialog.exchange
                        && from.iter().any(|market|!exchange.supports(dialog.kind.unwrap_or(market.kind))) {
                        dialog.kind=Some(MarketKind::Spot);
                    }
                    columns[1].label(RichText::new("Market").size(12.0).color(MUTED));
                    egui::ComboBox::from_id_salt("batch-kind").width(columns[1].available_width()).selected_text(dialog.kind.map_or("Keep current",MarketKind::label)).show_ui(&mut columns[1],|ui| {
                        ui.selectable_value(&mut dialog.kind,None,"Keep current");
                        for kind in MarketKind::ALL {
                            ui.add_enabled_ui(dialog.exchange.is_none_or(|exchange|exchange.supports(kind)),|ui|ui.selectable_value(&mut dialog.kind,Some(kind),kind.label()));
                        }
                    });
                });
                if previous!=(dialog.exchange,dialog.kind) { dialog.choices.clear(); }
                ui.label(RichText::new("Symbol").size(12.0).color(MUTED));
                let search = ui.add_sized([ui.available_width(), 30.0], egui::TextEdit::singleline(&mut dialog.query).hint_text("Keep current, or search coin").vertical_align(egui::Align::Center));
                if dialog.focus { search.request_focus(); dialog.focus = false; }
                if search.changed() { dialog.selected = None; dialog.choices.clear(); }
                let search_enter=(search.has_focus() || search.lost_focus()) && ui.input(|input|input.key_pressed(egui::Key::Enter));
                let keys=dialog.catalog_keys(&from);
                app.request_catalogs(keys.clone(), HashSet::new());
                let ready=keys.iter().all(|key|app.catalogs.contains_key(key));
                if !ready {
                    for key in &keys {
                        if let Some(error) = app.catalog_errors.get(key) {
                            ui.label(RichText::new(format!("{} {}: {error}", key.0.label(), key.1.label())).size(12.0).color(RED));
                            if ui.small_button("Retry").clicked() { app.request_catalogs(HashSet::from([*key]), HashSet::from([*key])); }
                        }
                    }
                    ui.label(RichText::new("Loading market catalogs…").size(12.0).color(MUTED));
                }
                let query = dialog.query.trim().to_ascii_uppercase();
                if dialog.selected.is_none() && !query.is_empty() && ready {
                    let mut bases: Vec<_> = from.iter().flat_map(|market| {
                        let key=dialog.target(market);
                        app.catalogs.get(&key).into_iter().flatten().map(move |info|orderflow::base_units(&info.base,key.1).0.to_ascii_uppercase())
                    }).filter(|base|base.contains(&query)).collect();
                    bases.sort_unstable(); bases.dedup();
                    if bases.is_empty() { ui.label(RichText::new("No matching symbols").color(MUTED)); }
                    if search_enter && bases.contains(&query) {
                        dialog.selected = Some(query.clone()); dialog.query = query.clone(); search.request_focus();
                    }
                    if let Some(base) = coin_list(ui, &bases).inner {
                        dialog.selected = Some(base.clone()); dialog.query = base;
                    }
                }
                ui.separator();
                let current = if from.len() == 1 {
                    let market = &from[0];
                    format!("{} {} {}", market.exchange.label(), market.kind.label(), market.symbol)
                } else { format!("{} markets", from.len()) };
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Current").size(12.0).color(MUTED));
                    ui.add(egui::Label::new(RichText::new(current).size(12.0)).truncate());
                });
                let mut enabled = false;
                if ready && (dialog.selected.is_some() || query.is_empty()) {
                    let base=dialog.selected.clone();
                    let options: Vec<_> = from.iter().map(|market| dialog.options(market,base.as_deref(),&app.catalogs)).collect();
                    let unresolved = from.iter().zip(&options).any(|(market, options)| !options.is_empty() && dialog.choice(market, options).is_none());
                    let missing = options.iter().filter(|options| options.is_empty()).count();
                    // Required market choices must stay visible even when the preview is collapsed.
                    if unresolved { dialog.preview = true; }
                    let preview = egui::CollapsingHeader::new(RichText::new(format!("Preview changes · {count} widgets")).size(12.0).color(MUTED))
                        .id_salt("batch-preview-header").open(Some(dialog.preview)).show(ui, |ui| {
                        egui::ScrollArea::vertical().id_salt("batch-preview").auto_shrink([false, true]).max_height(200.0).show(ui, |ui| {
                            for (market, options) in from.iter().zip(&options) {
                                let target=dialog.target(market);
                                ui.add(egui::Label::new(RichText::new(format!("{} {} {}",market.exchange.label(),market.kind.label(),market.symbol)).size(12.0).color(MUTED)).truncate());
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(RichText::new(format!("To {} {}", target.0.label(), target.1.label())).size(12.0).color(MUTED));
                                    if options.is_empty() {
                                        ui.label(RichText::new("Unavailable · paused").size(12.0).color(RED));
                                    } else if options.len() == 1 {
                                        ui.label(RichText::new(&options[0].symbol).monospace().size(12.0));
                                    } else {
                                        let choice = dialog.choice(market, options);
                                        egui::menu::MenuButton::new(choice.as_ref().map_or("Choose market…", |market| &market.symbol)).ui(ui, |ui| {
                                            for option in options { if ui.button(&option.symbol).clicked() { dialog.choices.insert(market.clone(), option.clone()); ui.close(); } }
                                        });
                                    }
                                });
                            }
                        });
                    });
                    if preview.header_response.clicked() { dialog.preview = !dialog.preview; }
                    if missing > 0 { ui.label(RichText::new(format!("{missing} unavailable sources will pause.")).size(12.0).color(RED)); }
                    let changed=dialog.selected.is_some() || dialog.exchange.is_some() || dialog.kind.is_some();
                    enabled = changed && !unresolved && count > 0 && missing < from.len();
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(if enabled { format!("{count} widgets will change") } else { "No changes ready".to_owned() }).size(12.0).color(MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        do_apply = ui.add_enabled(enabled, egui::Button::new(RichText::new("Apply changes").color(BG)).fill(TEXT).min_size(egui::vec2(110.0, 30.0))).clicked()
                            || (enabled && preview_was_shown && search_enter);
                        if ui.add_sized([60.0, 30.0], egui::Button::new("Cancel").frame(false)).clicked() { cancel = true; }
                    });
                });
                if app.workspaces[dialog.workspace].tiles.iter().any(|(_, tile)| matches!(tile, Tile::Pane(pane) if matches!(pane.kind, WidgetKind::Fills))) {
                    ui.label(RichText::new("FILLS keeps its account filters.").size(12.0).color(MUTED));
                }
            });
        if do_apply {
            apply(
                app,
                &dialog,
                dialog.selected.as_deref(),
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
    fn venue_kind_and_symbol_update_all_market_widgets_and_merge_duplicate_sources() {
        let mut app = crate::tests::test_app();
        let binance = Market::default();
        let bybit = market(Exchange::Bybit, "BTCUSDT");
        let chosen = Market {
            exchange: Exchange::Coinbase,
            kind: MarketKind::Spot,
            symbol: "ETH-USD".into(),
        };
        app.catalogs.insert(
            (Exchange::Binance, MarketKind::Spot),
            vec![info("BTCUSDT", "BTC", "USDT")],
        );
        app.catalogs.insert(
            (Exchange::Bybit, MarketKind::Perp),
            vec![info("BTCUSDT", "BTC", "USDT")],
        );
        app.catalogs.insert(
            (Exchange::Coinbase, MarketKind::Spot),
            vec![info("ETH-USD", "ETH", "USD"), info("ETH-EUR", "ETH", "EUR")],
        );
        let kinds = [
            WidgetKind::Chart,
            WidgetKind::Book,
            WidgetKind::Compare,
            WidgetKind::Tape,
            WidgetKind::Dom,
            WidgetKind::Position,
            WidgetKind::Imbalance,
            WidgetKind::Events,
            WidgetKind::Slippage,
            WidgetKind::Fills,
        ];
        let panes = kinds.map(|kind| {
            let mut pane = Pane::new(kind, binance.clone());
            if pane.uses_sources() {
                pane.series = vec![binance.clone(), bybit.clone()];
                pane.orderflow.source = Some(bybit.clone());
                pane.editing_series = 1;
            }
            pane.slippage.show_values = false;
            pane
        });
        app.workspaces[0] = Tree::new_horizontal("all-market-widgets", panes.into());
        let before = serde_json::to_string(&app.workspaces).unwrap();
        let other = serde_json::to_string(&app.workspaces[1]).unwrap();
        let mut dialog = Dialog::new(0);
        dialog.exchange = Some(Exchange::Coinbase);
        dialog.kind = Some(MarketKind::Spot);
        dialog.choices.insert(binance, chosen.clone());
        dialog.choices.insert(bybit, chosen.clone());
        assert_eq!(apply(&mut app, &dialog, Some("ETH"), 0.0), 9);
        for (_, tile) in app.workspaces[0].tiles.iter() {
            let Tile::Pane(pane) = tile else { continue };
            if matches!(pane.kind, WidgetKind::Fills) {
                assert_eq!(pane.market.exchange, Exchange::Binance);
                continue;
            }
            assert_eq!(pane.market, chosen);
            assert!(!pane.slippage.show_values);
            assert!(pane.unavailable_markets.is_empty());
            if pane.uses_sources() {
                assert_eq!(pane.series, [chosen.clone()]);
                assert_eq!(pane.editing_series, 0);
                assert_eq!(pane.orderflow.source, Some(chosen.clone()));
            }
        }
        assert_eq!(serde_json::to_string(&app.workspaces[1]).unwrap(), other);
        app.socket_open = true;
        app.sync_subscriptions();
        assert_eq!(app.subscribed, HashSet::from([chosen]));
        let saved = serde_json::to_string(&app.workspaces[0]).unwrap();
        let restored: Tree<Pane> = serde_json::from_str(&saved).unwrap();
        assert_eq!(
            serde_json::to_value(&restored).unwrap(),
            serde_json::from_str::<serde_json::Value>(&saved).unwrap()
        );
        let undo = app.batch_undo.take().unwrap();
        restore(&mut app, undo);
        assert_eq!(serde_json::to_string(&app.workspaces).unwrap(), before);
    }

    #[test]
    fn changing_only_venue_preserves_each_underlying_coin_and_resolves_new_quote() {
        let btc = Market::default();
        let eth = Market {
            symbol: "ETHUSDT".into(),
            ..btc.clone()
        };
        let mut app = crate::tests::test_app();
        app.catalogs.insert(
            (Exchange::Binance, MarketKind::Spot),
            vec![
                info("BTCUSDT", "BTC", "USDT"),
                info("ETHUSDT", "ETH", "USDT"),
            ],
        );
        app.catalogs.insert(
            (Exchange::Coinbase, MarketKind::Spot),
            vec![info("BTC-USD", "BTC", "USD"), info("ETH-USD", "ETH", "USD")],
        );
        app.workspaces[0] = Tree::new_horizontal(
            "keep-coins",
            vec![
                Pane::new(WidgetKind::Chart, btc),
                Pane::new(WidgetKind::Book, eth),
            ],
        );
        let mut dialog = Dialog::new(0);
        dialog.exchange = Some(Exchange::Coinbase);
        assert_eq!(apply(&mut app, &dialog, None, 0.0), 2);
        let result = markets(&app.workspaces[0]);
        assert!(
            result
                .iter()
                .all(|m| m.exchange == Exchange::Coinbase && m.kind == MarketKind::Spot)
        );
        assert_eq!(
            result.iter().map(|m| m.symbol.as_str()).collect::<Vec<_>>(),
            ["BTC-USD", "ETH-USD"]
        );
    }

    #[test]
    fn market_type_only_preserves_venue_and_resolves_native_contracts() {
        let mut app = crate::tests::test_app();
        let from = Market::default();
        app.catalogs.insert(
            (Exchange::Binance, MarketKind::Spot),
            vec![info("BTCUSDT", "BTC", "USDT")],
        );
        app.catalogs.insert(
            (Exchange::Binance, MarketKind::Perp),
            vec![
                info("BTCUSDT", "BTC", "USDT"),
                info("BTCUSDC", "BTC", "USDC"),
            ],
        );
        let mut dialog = Dialog::new(0);
        dialog.kind = Some(MarketKind::Perp);
        let options = dialog.options(&from, None, &app.catalogs);
        assert_eq!(options, [market(Exchange::Binance, "BTCUSDT")]);
        assert!(
            dialog
                .catalog_keys(&[from.clone()])
                .contains(&(Exchange::Binance, MarketKind::Perp))
        );
        let mut unsupported = Dialog::new(0);
        unsupported.exchange = Some(Exchange::Coinbase);
        unsupported.kind = Some(MarketKind::Perp);
        assert!(
            unsupported
                .options(&from, Some("ETH"), &app.catalogs)
                .is_empty()
        );
        assert!(
            !unsupported
                .catalog_keys(&[from])
                .contains(&(Exchange::Coinbase, MarketKind::Perp))
        );
    }

    #[test]
    fn target_quote_ambiguity_requires_a_choice_and_cannot_reuse_stale_choices() {
        let from = Market::default();
        let mut catalogs = HashMap::new();
        catalogs.insert(
            (Exchange::Binance, MarketKind::Spot),
            vec![info("BTCUSDT", "BTC", "USDT")],
        );
        catalogs.insert(
            (Exchange::Coinbase, MarketKind::Spot),
            vec![info("ETH-USD", "ETH", "USD"), info("ETH-EUR", "ETH", "EUR")],
        );
        let mut dialog = Dialog::new(0);
        dialog.exchange = Some(Exchange::Coinbase);
        let options = dialog.options(&from, Some("ETH"), &catalogs);
        assert_eq!(options.len(), 2);
        assert!(dialog.choice(&from, &options).is_none());
        dialog.choices.insert(from.clone(), options[1].clone());
        assert_eq!(dialog.choice(&from, &options), Some(options[1].clone()));
        dialog.exchange = None;
        assert!(
            dialog
                .choice(&from, &dialog.options(&from, Some("ETH"), &catalogs))
                .is_none()
        );
    }

    #[test]
    fn compact_preview_starts_closed_but_reveals_required_market_choices() {
        let mut app = crate::tests::test_app();
        app.catalogs.insert(
            (Exchange::Binance, MarketKind::Spot),
            vec![info("BTCUSDT", "BTC", "USDT")],
        );
        app.catalogs.insert(
            (Exchange::Coinbase, MarketKind::Spot),
            vec![info("ETH-USD", "ETH", "USD"), info("ETH-EUR", "ETH", "EUR")],
        );
        let mut dialog = Dialog::new(0);
        assert!(!dialog.preview);
        dialog.exchange = Some(Exchange::Coinbase);
        dialog.query = "ETH".into();
        dialog.selected = Some("ETH".into());
        app.batch_change = Some(dialog);
        let ctx = egui::Context::default();
        for frame in 0..2 {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 700.0),
                    )),
                    events: if frame == 0 {
                        vec![]
                    } else {
                        vec![egui::Event::Key {
                            key: egui::Key::Enter,
                            physical_key: None,
                            pressed: true,
                            repeat: false,
                            modifiers: egui::Modifiers::NONE,
                        }]
                    },
                    ..Default::default()
                },
                |ui| super::ui(&mut app, ui.ctx()),
            );
            output.textures_delta.clear();
            assert!(app.batch_change.as_ref().unwrap().preview);
            assert!(
                app.batch_undo.is_none(),
                "an unresolved quote choice must block Apply even with Enter"
            );
        }
    }

    #[test]
    fn enter_previews_then_applies_the_selected_market_without_skipping_confirmation() {
        let mut app = crate::tests::test_app();
        app.catalogs.insert(
            (Exchange::Binance, MarketKind::Spot),
            vec![info("BTCUSDT", "BTC", "USDT")],
        );
        app.catalogs.insert(
            (Exchange::Coinbase, MarketKind::Spot),
            vec![info("ETH-USD", "ETH", "USD")],
        );
        let mut dialog = Dialog::new(0);
        dialog.exchange = Some(Exchange::Coinbase);
        dialog.kind = Some(MarketKind::Spot);
        app.batch_change = Some(dialog);
        let ctx = egui::Context::default();
        let enter = || egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        for (frame, events) in [
            vec![],
            vec![egui::Event::Text("ETH".into())],
            vec![enter()],
            vec![enter()],
        ]
        .into_iter()
        .enumerate()
        {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 700.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| super::ui(&mut app, ui.ctx()),
            );
            output.textures_delta.clear();
            if frame == 2 {
                assert_eq!(
                    app.batch_change.as_ref().unwrap().selected.as_deref(),
                    Some("ETH")
                );
                assert!(
                    markets(&app.workspaces[0])
                        .iter()
                        .all(|m| m.exchange == Exchange::Binance),
                    "first Enter only opens preview"
                );
            }
        }
        assert!(app.batch_change.is_none());
        assert!(
            markets(&app.workspaces[0])
                .iter()
                .all(|m| m.exchange == Exchange::Coinbase && m.symbol == "ETH-USD")
        );
    }

    #[test]
    fn batch_changes_every_dom_source_resolves_bundles_and_undo_restores() {
        let mut app = crate::tests::test_app();
        let binance = market(Exchange::Binance, "BTCUSDT");
        let hl = market(Exchange::Hyperliquid, "BTC");
        app.catalogs.insert(
            (Exchange::Binance, MarketKind::Perp),
            vec![
                info("BTCUSDT", "BTC", "USDT"),
                info("1000PEPEUSDT", "1000PEPE", "USDT"),
            ],
        );
        app.catalogs.insert(
            (Exchange::Hyperliquid, MarketKind::Perp),
            vec![info("BTC", "BTC", "USDC"), info("kPEPE", "kPEPE", "USDC")],
        );
        let mut pane = Pane::new(WidgetKind::Dom, binance.clone());
        pane.series = vec![binance, hl];
        let mut tiles = Tiles::default();
        let id = tiles.insert_pane(pane);
        app.workspaces[0] = Tree::new("multi-dom", id, tiles);
        let before = serde_json::to_string(&app.workspaces).unwrap();
        assert_eq!(apply(&mut app, &Dialog::new(0), Some("PEPE"), 0.0), 1);
        let pane = app.workspaces[0].tiles.get_pane(&id).unwrap();
        assert_eq!(
            pane.series
                .iter()
                .map(|market| market.symbol.as_str())
                .collect::<Vec<_>>(),
            ["1000PEPEUSDT", "kPEPE"]
        );
        assert_eq!(pane.market, pane.series[0]);
        assert!(pane.unavailable_markets.is_empty());
        app.socket_open = true;
        app.sync_subscriptions();
        assert_eq!(app.subscribed.len(), 2);
        let undo = app.batch_undo.take().unwrap();
        restore(&mut app, undo);
        assert_eq!(serde_json::to_string(&app.workspaces).unwrap(), before);
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
    fn reopening_batch_allows_switching_dex_for_the_same_symbol() {
        let mut app = crate::tests::test_app();
        let native = market(Exchange::Hyperliquid, "BTC");
        let xyz = market(Exchange::Hyperliquid, "xyz:UNITREE");
        let para = market(Exchange::Hyperliquid, "para:UNITREE");
        let catalog = vec![
            info("BTC", "BTC", "USDC"),
            info("xyz:UNITREE", "UNITREE", "USDC"),
            info("para:UNITREE", "UNITREE", "USDC"),
        ];
        app.catalogs
            .insert((Exchange::Hyperliquid, MarketKind::Perp), catalog.clone());
        let mut tiles = Tiles::default();
        let pane = tiles.insert_pane(Pane::new(WidgetKind::Dom, native.clone()));
        app.workspaces[0] = Tree::new("repeat-dex", pane, tiles);
        let mut first = Dialog::new(0);
        first.choices.insert(native, xyz.clone());
        apply(&mut app, &first, Some("UNITREE"), 0.0);
        let current = app.workspaces[0]
            .tiles
            .get_pane(&pane)
            .unwrap()
            .market
            .clone();
        assert_eq!(current, xyz);
        let options = candidates(&current, "UNITREE", &catalog);
        assert_eq!(
            options,
            [xyz.clone(), para.clone()],
            "the second preview must still offer both DEXs"
        );
        let mut second = Dialog::new(0);
        assert_eq!(second.choice(&current, &options), Some(xyz.clone()));
        second.choices.insert(current.clone(), para.clone());
        assert_eq!(second.choice(&current, &options), Some(para.clone()));
        apply(&mut app, &second, Some("UNITREE"), 1.0);
        let updated = app.workspaces[0].tiles.get_pane(&pane).unwrap();
        assert_eq!(updated.market, para);
        assert!(updated.unavailable_markets.is_empty());
        let undo = app.batch_undo.take().unwrap();
        restore(&mut app, undo);
        assert_eq!(app.workspaces[0].tiles.get_pane(&pane).unwrap().market, xyz);
    }

    #[test]
    fn hyperliquid_batch_falls_back_to_other_dexes_without_changing_quote() {
        let native = market(Exchange::Hyperliquid, "BTC");
        let xyz = market(Exchange::Hyperliquid, "xyz:UNITREE");
        let para = market(Exchange::Hyperliquid, "para:UNITREE");
        let catalog = vec![
            info("BTC", "BTC", "USDC"),
            info("xyz:UNITREE", "UNITREE", "USDC"),
            info("para:UNITREE", "UNITREE", "USDC"),
            info("cash:UNITREE", "UNITREE", "USDH"),
        ];
        assert_eq!(
            candidates(&native, "UNITREE", &catalog),
            [xyz.clone(), para.clone()]
        );
        assert_eq!(
            candidates(&xyz, "UNITREE", &catalog),
            [xyz.clone(), para.clone()]
        );
        assert_eq!(candidates(&para, "BTC", &catalog), [native]);
        assert!(candidates(&xyz, "MISSING", &catalog).is_empty());
    }

    #[test]
    fn batch_applies_chosen_hip3_market_to_shared_sources_and_undo_restores_native() {
        let mut app = crate::tests::test_app();
        let from = market(Exchange::Hyperliquid, "BTC");
        let chosen = market(Exchange::Hyperliquid, "xyz:UNITREE");
        app.catalogs.insert(
            (Exchange::Hyperliquid, MarketKind::Perp),
            vec![
                info("BTC", "BTC", "USDC"),
                info("xyz:UNITREE", "UNITREE", "USDC"),
                info("para:UNITREE", "UNITREE", "USDC"),
            ],
        );
        let mut tiles = Tiles::default();
        let dom = tiles.insert_pane(Pane::new(WidgetKind::Dom, from.clone()));
        let mut prices = Pane::new(WidgetKind::Compare, from.clone());
        prices.series = vec![from.clone()];
        prices.orderflow.source = Some(from.clone());
        let prices = tiles.insert_pane(prices);
        let root = tiles.insert_horizontal_tile(vec![dom, prices]);
        app.workspaces[0] = Tree::new("hip3-batch", root, tiles);
        let before = serde_json::to_string(&app.workspaces).unwrap();
        let mut dialog = Dialog::new(0);
        dialog.choices.insert(from.clone(), chosen.clone());
        assert_eq!(apply(&mut app, &dialog, Some("UNITREE"), 0.0), 2);
        let dom = app.workspaces[0].tiles.get_pane(&dom).unwrap();
        assert_eq!(dom.market, chosen);
        assert!(dom.unavailable_markets.is_empty());
        let prices = app.workspaces[0].tiles.get_pane(&prices).unwrap();
        assert_eq!(prices.series, [chosen.clone()]);
        assert_eq!(prices.orderflow.source.as_ref(), Some(&chosen));
        assert!(prices.unavailable_markets.is_empty());
        app.socket_open = true;
        app.sync_subscriptions();
        assert!(app.subscribed.contains(&chosen));
        assert!(!app.subscribed.contains(&from));
        let undo = app.batch_undo.take().unwrap();
        restore(&mut app, undo);
        assert_eq!(serde_json::to_string(&app.workspaces).unwrap(), before);
    }

    #[test]
    fn resolution_preserves_quote_defaults_to_current_dex_and_requires_ambiguous_choice() {
        let from = market(Exchange::Hyperliquid, "xyz:NVDA");
        let catalog = vec![
            info("xyz:NVDA", "NVDA", "USDC"),
            info("BTC", "BTC", "USDC"),
            info("flx:BTC", "BTC", "USDC"),
            info("xyz:BTC", "BTC", "USDC"),
        ];
        let options = candidates(&from, "BTC", &catalog);
        assert_eq!(
            options,
            [
                market(Exchange::Hyperliquid, "BTC"),
                market(Exchange::Hyperliquid, "flx:BTC"),
                market(Exchange::Hyperliquid, "xyz:BTC")
            ]
        );
        assert_eq!(
            Dialog::new(0).choice(&from, &options),
            Some(market(Exchange::Hyperliquid, "xyz:BTC"))
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
        assert_eq!(
            Dialog::new(0).choice(&from, &candidates(&from, "BTC", &catalog)),
            None
        );
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
        assert_eq!(apply(&mut app, &Dialog::new(0), Some("BTC"), 0.0), 3);
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
        apply(&mut app, &Dialog::new(0), Some("BTC"), 0.0);
        apply(&mut app, &Dialog::new(0), Some("PONS"), 1.0);
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
