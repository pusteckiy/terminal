//! Compact account navigation; credentials and live state stay out of the account list.
use super::*;

#[derive(Clone, Default)]
enum Page {
    #[default]
    List,
    Choose,
    Connect(Exchange),
    Detail(Account),
    Security,
}
#[derive(Default)]
pub struct Menu {
    page: Page,
    confirm_remove: bool,
}

pub(super) fn primary(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add_sized(
        [ui.available_width(), 28.0],
        egui::Button::new(RichText::new(text).color(BG)).fill(TEXT),
    )
}
fn badge(ui: &mut egui::Ui, exchange: Exchange) {
    let text = match exchange {
        Exchange::Hyperliquid => "HL",
        Exchange::Binance => "BN",
        Exchange::Bybit => "BY",
        Exchange::Okx => "OK",
        _ => "?",
    };
    let (rect, _) = ui.allocate_exact_size(egui::vec2(28.0, 28.0), Sense::hover());
    ui.painter().rect(
        rect,
        5.0,
        BG,
        egui::Stroke::new(1.0, BORDER),
        egui::StrokeKind::Inside,
    );
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(10.0),
        MUTED,
    );
}

impl TerminalApp {
    fn account_page(&mut self, page: Page) {
        self.account_ui.page = page;
        self.account_ui.confirm_remove = false;
        self.account_error = None;
        self.account_address.clear();
        self.private.vault.close_ui();
    }
    pub(super) fn account_menu(&mut self, ui: &mut egui::Ui) {
        let button = egui::Button::new("")
            .fill(RAISED)
            .stroke(egui::Stroke::new(1.0, BORDER))
            .corner_radius(egui::CornerRadius::same(6))
            .min_size(egui::vec2(29.0, 25.0));
        let (response, menu) = egui::menu::MenuButton::from_button(button)
            .config(
                egui::menu::MenuConfig::new()
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
            )
            .ui(ui, |ui| self.account_content(ui));
        if menu.is_none() {
            self.account_page(Page::List);
        }
        let painter = ui.painter();
        let center = response.rect.center();
        painter.circle_stroke(
            center + egui::vec2(0.0, -3.0),
            2.6,
            egui::Stroke::new(1.35, TEXT),
        );
        painter.add(egui::epaint::PathShape::line(
            vec![
                center + egui::vec2(-5.3, 5.0),
                center + egui::vec2(-4.0, 2.0),
                center + egui::vec2(-1.7, 0.9),
                center + egui::vec2(1.7, 0.9),
                center + egui::vec2(4.0, 2.0),
                center + egui::vec2(5.3, 5.0),
            ],
            egui::Stroke::new(1.35, TEXT),
        ));
        response.on_hover_text("Accounts");
    }

    fn account_content(&mut self, ui: &mut egui::Ui) {
        ui.set_width((ui.ctx().viewport_rect().width() - 32.0).clamp(240.0, 340.0));
        // Popups remember the previous screen's size; allow a longer form to grow.
        ui.set_max_height((ui.ctx().viewport_rect().height() - 80.0).max(120.0));
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
        if self.private.vault.take_connected() {
            self.account_page(Page::List);
        }
        if let Page::Detail(account) = &self.account_ui.page {
            if !self.accounts.contains(account) {
                self.account_page(Page::List);
            }
        }
        ui.horizontal(|ui| {
            if matches!(self.account_ui.page, Page::List) {
                ui.label(RichText::new("Accounts").size(13.0));
                ui.label(
                    RichText::new(self.accounts.len().to_string())
                        .size(11.0)
                        .color(MUTED),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new("⚙").frame(false))
                        .on_hover_text("API storage settings")
                        .clicked()
                    {
                        self.account_page(Page::Security);
                    }
                });
            } else if ui
                .add(egui::Button::new("‹ Accounts").frame(false))
                .clicked()
            {
                self.account_page(Page::List);
            }
        });
        ui.separator();
        let page = self.account_ui.page.clone();
        // Keep the footer reachable in short windows and make each screen's scroll independent.
        let body_height = (ui.ctx().viewport_rect().height() - 140.0).clamp(100.0, 520.0);
        match page {
            Page::List => {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .id_salt("account-list")
                    .max_height(body_height)
                    .show(ui, |ui| self.account_list(ui));
                ui.separator();
                if primary(ui, "+ Connect account").clicked() {
                    self.account_page(Page::Choose);
                }
            }
            Page::Choose => {
                ui.label("Connect account");
                for exchange in [
                    Exchange::Hyperliquid,
                    Exchange::Binance,
                    Exchange::Bybit,
                    Exchange::Okx,
                ] {
                    let response = egui::Frame::new()
                        .inner_margin(egui::Margin::symmetric(4, 6))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                badge(ui, exchange);
                                ui.add_sized(
                                    [ui.available_width(), 32.0],
                                    egui::Button::new(exchange.label())
                                        .right_text(if exchange == Exchange::Hyperliquid {
                                            "Wallet address  ›"
                                        } else {
                                            "API key  ›"
                                        })
                                        .frame(false),
                                )
                            })
                            .inner
                        })
                        .inner;
                    if response.clicked() {
                        self.account_page(Page::Connect(exchange));
                        self.private.vault.select_exchange(exchange);
                    }
                }
            }
            Page::Connect(exchange) => {
                ui.label(RichText::new(exchange.label()).size(13.0));
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .id_salt(("account-connect", exchange))
                    .max_height(body_height)
                    .show(ui, |ui| {
                        if exchange == Exchange::Hyperliquid {
                            self.wallet_form(ui);
                        } else {
                            self.private.ui(ui, Some(exchange));
                        }
                    });
            }
            Page::Security => {
                ui.label(RichText::new("API key storage").size(13.0));
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .id_salt("account-security")
                    .max_height(body_height)
                    .show(ui, |ui| {
                        if self.private.ui(ui, None) {
                            self.account_data.retain(|a, _| !private::is_private(a));
                            self.fills.histories.retain(|a, _| !private::is_private(a));
                        }
                    });
            }
            Page::Detail(account) => {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .id_salt(("account-details", &account))
                    .max_height(body_height)
                    .show(ui, |ui| self.account_details(ui, &account, body_height));
            }
        }
    }
    fn account_list(&mut self, ui: &mut egui::Ui) {
        if self.accounts.is_empty() {
            ui.add_space(12.0);
            ui.label(RichText::new("No accounts connected").color(MUTED));
            ui.add_space(12.0);
        }
        let mut selected = None;
        for account in &self.accounts {
            ui.push_id(account, |ui| {
                egui::Frame::new()
                    .inner_margin(egui::Margin::symmetric(4, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            badge(ui, account.exchange);
                            let width = (ui.available_width() - 58.0).max(80.0);
                            let state = self.account_data.get(account);
                            let connected = state.is_some_and(|s| s.connected);
                            let text = ui
                                .allocate_ui_with_layout(
                                    egui::vec2(width, 50.0),
                                    egui::Layout::top_down(egui::Align::Min),
                                    |ui| {
                                        ui.set_min_width(width);
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(account.exchange.label()).size(12.0),
                                            )
                                            .truncate(),
                                        );
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new(self.private.label(account))
                                                    .monospace()
                                                    .size(10.0)
                                                    .color(MUTED),
                                            )
                                            .truncate(),
                                        );
                                        let description = if private::is_private(account)
                                            && !self.private.vault.unlocked()
                                        {
                                            "Locked".to_owned()
                                        } else if let Some(state) = state {
                                            if connected {
                                                format!(
                                                    "{} orders · {} positions",
                                                    state.orders.len(),
                                                    state.positions.len()
                                                )
                                            } else if state.error.is_some() {
                                                "Connection error · Open details".to_owned()
                                            } else {
                                                "Connecting…".to_owned()
                                            }
                                        } else {
                                            "Connecting…".to_owned()
                                        };
                                        ui.horizontal(|ui| {
                                            let (dot, _) = ui.allocate_exact_size(
                                                egui::vec2(6.0, 10.0),
                                                Sense::hover(),
                                            );
                                            ui.painter().circle_filled(
                                                dot.center(),
                                                2.4,
                                                if connected { GREEN } else { MUTED },
                                            );
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(description)
                                                        .size(10.0)
                                                        .color(MUTED),
                                                )
                                                .truncate(),
                                            );
                                        });
                                    },
                                )
                                .response
                                .interact(Sense::click());
                            text.widget_info(|| {
                                egui::WidgetInfo::labeled(
                                    egui::WidgetType::Button,
                                    true,
                                    format!(
                                        "Open {} {}",
                                        account.exchange.label(),
                                        self.private.label(account)
                                    ),
                                )
                            });
                            if text.on_hover_text(&account.address).clicked() {
                                selected = Some(account.clone());
                            }
                            let visible = !self.hidden_widget_accounts.contains(account);
                            if account_visibility_button(ui, visible) {
                                if visible {
                                    self.hidden_widget_accounts.insert(account.clone());
                                } else {
                                    self.hidden_widget_accounts.remove(account);
                                }
                            }
                            if ui
                                .add_sized(
                                    [20.0, 23.0],
                                    egui::Button::new(RichText::new("›").size(16.0)).frame(false),
                                )
                                .on_hover_text("Account details")
                                .clicked()
                            {
                                selected = Some(account.clone());
                            }
                        });
                    });
            });
        }
        if let Some(account) = selected {
            self.account_page(Page::Detail(account));
        }
    }
    fn wallet_form(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Wallet address").size(11.0).color(MUTED));
        ui.add(
            egui::TextEdit::singleline(&mut self.account_address)
                .hint_text("0x…")
                .desired_width(f32::INFINITY),
        );
        ui.label(
            RichText::new("Read-only. No wallet signature needed.")
                .size(10.0)
                .color(MUTED),
        );
        if let Some(error) = &self.account_error {
            ui.label(RichText::new(error).size(11.0).color(RED));
        }
        ui.add_space(6.0);
        if primary(ui, "Connect account").clicked() {
            let account = Account {
                exchange: Exchange::Hyperliquid,
                address: self.account_address.trim().to_ascii_lowercase(),
            };
            if !account.valid() {
                self.account_error = Some("Enter a valid 0x wallet address".into());
            } else if self.accounts.contains(&account) {
                self.account_error = Some("This account is already connected".into());
            } else {
                self.hidden_widget_accounts.remove(&account);
                self.accounts.push(account);
                self.account_page(Page::List);
            }
        }
    }
    fn account_details(&mut self, ui: &mut egui::Ui, account: &Account, height: f32) {
        ui.label(
            RichText::new(format!(
                "{} · {}",
                account.exchange.label(),
                self.private.label(account)
            ))
            .size(12.0),
        );
        ui.add(
            egui::Label::new(
                RichText::new(&account.address)
                    .monospace()
                    .size(10.0)
                    .color(MUTED),
            )
            .wrap(),
        );
        let state = self.account_data.get(account);
        egui::Grid::new("account-details")
            .num_columns(2)
            .spacing([18.0, 8.0])
            .show(ui, |ui| {
                ui.label(RichText::new("Connection").color(MUTED));
                ui.label(
                    if private::is_private(account) && !self.private.vault.unlocked() {
                        "Locked"
                    } else if state.is_some_and(|s| s.connected) {
                        "Live"
                    } else {
                        "Disconnected"
                    },
                );
                ui.end_row();
                ui.label(RichText::new("Orders").color(MUTED));
                ui.label(state.map_or_else(|| "—".into(), |s| s.orders.len().to_string()));
                ui.end_row();
                ui.label(RichText::new("Positions").color(MUTED));
                ui.label(state.map_or_else(|| "—".into(), |s| s.positions.len().to_string()));
                ui.end_row();
                ui.label(RichText::new("Show on widgets").color(MUTED));
                let visible = !self.hidden_widget_accounts.contains(account);
                if account_visibility_button(ui, visible) {
                    if visible {
                        self.hidden_widget_accounts.insert(account.clone());
                    } else {
                        self.hidden_widget_accounts.remove(account);
                    }
                }
                ui.end_row();
            });
        if let Some(error) = state.and_then(|s| s.error.as_deref()) {
            ui.label(RichText::new(error).size(10.0).color(RED));
        }
        if let Some(warning) = self.private.warning(account) {
            ui.label(RichText::new(warning).size(10.0).color(MUTED));
        }
        if let Some(state) = state {
            egui::CollapsingHeader::new("View live state")
                .id_salt(account)
                .show(ui, |ui| {
                    let row_height =
                        ui.text_style_height(&egui::TextStyle::Body) + ui.spacing().item_spacing.y;
                    let positions = state.positions.len();
                    let orders = state.orders.len();
                    let balances: Vec<_> = state
                        .spot_balances
                        .iter()
                        .filter(|b| b.total != "0")
                        .collect();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, true])
                        .id_salt(("account-live-state", account))
                        .max_height((height - 240.0).clamp(80.0, 240.0))
                        .show_rows(
                            ui,
                            row_height,
                            positions + orders + balances.len(),
                            |ui, range| {
                                for index in range {
                                    let (text, color) = if index < positions {
                                        let position = &state.positions[index];
                                        (
                                            format!(
                                                "{} {} {} @ {} · PnL {}",
                                                position.coin,
                                                if position.size.starts_with('-') {
                                                    "SHORT"
                                                } else {
                                                    "LONG"
                                                },
                                                position.size,
                                                position.entry_price,
                                                position.unrealized_pnl
                                            ),
                                            if position.size.starts_with('-') {
                                                RED
                                            } else {
                                                GREEN
                                            },
                                        )
                                    } else if index < positions + orders {
                                        let order = &state.orders[index - positions];
                                        (
                                            format!(
                                                "{} {} {} @ {}",
                                                order.coin,
                                                if order.side == TradeSide::Buy {
                                                    "BUY"
                                                } else {
                                                    "SELL"
                                                },
                                                order.size,
                                                order.price
                                            ),
                                            if order.side == TradeSide::Buy {
                                                GREEN
                                            } else {
                                                RED
                                            },
                                        )
                                    } else {
                                        let balance = balances[index - positions - orders];
                                        (
                                            format!(
                                                "{} {} · held {}",
                                                balance.coin, balance.total, balance.hold
                                            ),
                                            TEXT,
                                        )
                                    };
                                    ui.add(
                                        egui::Label::new(RichText::new(&text).color(color))
                                            .truncate(),
                                    )
                                    .on_hover_text(text);
                                }
                            },
                        );
                });
        }
        ui.separator();
        if self.account_ui.confirm_remove {
            ui.label("Remove this account connection?");
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    self.account_ui.confirm_remove = false;
                }
                let can_remove = !private::is_private(account) || self.private.vault.can_edit();
                if ui
                    .add_enabled(
                        can_remove,
                        egui::Button::new(RichText::new("Remove account").color(RED)),
                    )
                    .clicked()
                {
                    if private::is_private(account) {
                        self.private.vault.remove(account, ui.ctx());
                    }
                    self.accounts.retain(|a| a != account);
                    self.hidden_widget_accounts.remove(account);
                    self.account_data.remove(account);
                    self.fills.histories.remove(account);
                    self.account_page(Page::List);
                }
            });
        } else {
            ui.horizontal(|ui| {
                if private::is_private(account) && !self.private.vault.unlocked() {
                    if ui.button("Unlock API accounts").clicked() {
                        self.account_page(Page::Security);
                    }
                } else if ui
                    .add_enabled(
                        !private::is_private(account) || self.private.vault.can_edit(),
                        egui::Button::new(RichText::new("Remove account").color(MUTED)),
                    )
                    .clicked()
                {
                    self.account_ui.confirm_remove = true;
                }
            });
        }
    }
}
