#[cfg(target_arch = "wasm32")]
mod browser;
#[cfg(any(target_arch = "wasm32", test))]
mod parse;
use crate::{fills, vault::Vault};
use eframe::egui;
use std::collections::HashMap;
#[cfg(target_arch = "wasm32")]
use terminal_core::OwnFill;
use terminal_core::{Account, AccountState, Market};

#[cfg(target_arch = "wasm32")]
#[derive(Default)]
struct Output {
    state: AccountState,
    fills: Vec<OwnFill>,
    version: u64,
    warning: Option<String>,
}
#[derive(Default)]
pub struct Controller {
    pub vault: Vault,
    warnings: HashMap<Account, String>,
    #[cfg(target_arch = "wasm32")]
    running: HashMap<Account, browser::Running>,
    #[cfg(target_arch = "wasm32")]
    generation: u64,
}
pub fn is_private(account: &Account) -> bool {
    matches!(
        account.exchange,
        terminal_core::Exchange::Binance
            | terminal_core::Exchange::Bybit
            | terminal_core::Exchange::Okx
    ) && account.address.starts_with("api:")
        && account.address.len() == 36
        && account.address[4..].bytes().all(|b| b.is_ascii_hexdigit())
}
impl Controller {
    pub fn warning(&self, account: &Account) -> Option<&str> {
        self.warnings.get(account).map(String::as_str)
    }
    pub fn label(&self, account: &Account) -> String {
        self.vault.label(account).unwrap_or_else(|| {
            format!(
                "{}…{}",
                &account.address[..6.min(account.address.len())],
                &account.address[account.address.len().saturating_sub(4)..]
            )
        })
    }
    pub fn poll(
        &mut self,
        ctx: &egui::Context,
        accounts: &mut Vec<Account>,
        states: &mut HashMap<Account, AccountState>,
        fills: &mut fills::Store,
        markets: Vec<Market>,
    ) -> bool {
        #[cfg(target_arch = "wasm32")]
        {
            if self.generation != self.vault.generation() {
                self.running.clear();
                self.generation = self.vault.generation();
            }
            let wanted: std::collections::HashSet<_> = self.vault.accounts().into_iter().collect();
            self.running.retain(|account, _| wanted.contains(account));
            self.warnings.retain(|account, _| wanted.contains(account));
            if self.vault.unlocked() {
                accounts.retain(|a| !is_private(a) || wanted.contains(a));
            }
            let mut changed = false;
            states.retain(|account, _| {
                let keep = !is_private(account) || wanted.contains(account);
                changed |= !keep;
                keep
            });
            fills
                .histories
                .retain(|account, _| !is_private(account) || wanted.contains(account));
            for account in wanted {
                if !accounts.contains(&account) {
                    accounts.push(account.clone());
                    changed = true;
                }
                if !self.running.contains_key(&account) {
                    if let Some(credential) = self.vault.credential(&account) {
                        self.running.insert(
                            account.clone(),
                            browser::start(credential, ctx.clone(), markets.clone()),
                        );
                    }
                }
                let Some(running) = self.running.get_mut(&account) else {
                    continue;
                };
                let mut output = running.output.borrow_mut();
                if running.applied != output.version {
                    if let Some(warning) = &output.warning {
                        self.warnings.insert(account.clone(), warning.clone());
                    } else {
                        self.warnings.remove(&account);
                    }
                    states.insert(account.clone(), output.state.clone());
                    fills.push(account, std::mem::take(&mut output.fills));
                    running.applied = output.version;
                    changed = true;
                }
            }
            changed
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (ctx, accounts, states, fills, markets);
            false
        }
    }
    pub fn ui(&mut self, ui: &mut egui::Ui, exchange: Option<terminal_core::Exchange>) -> bool {
        let was_unlocked = self.vault.unlocked();
        self.vault.ui(ui, exchange);
        let locked = was_unlocked && !self.vault.unlocked();
        if locked {
            self.lock();
        }
        locked
    }
    pub fn lock(&mut self) {
        self.vault.lock();
        self.warnings.clear();
        #[cfg(target_arch = "wasm32")]
        self.running.clear();
    }
}
