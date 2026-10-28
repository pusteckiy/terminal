//! Only Envelope is persisted. Passwords, plaintext credentials and CryptoKeys are session-only.
#[cfg(any(target_arch = "wasm32", test))]
use serde::{Deserialize, Serialize};
use terminal_core::Account;
#[cfg(any(target_arch = "wasm32", test))]
use terminal_core::Exchange;
#[cfg(any(target_arch = "wasm32", test))]
use zeroize::Zeroize;

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    pub account: Account,
    pub label: String,
    pub api_key: String,
    pub secret: String,
    pub passphrase: String,
    pub spot: bool,
    pub perp: bool,
}
#[cfg(any(target_arch = "wasm32", test))]
impl Drop for Credential {
    fn drop(&mut self) {
        self.api_key.zeroize();
        self.secret.zeroize();
        self.passphrase.zeroize();
    }
}
#[cfg(any(target_arch = "wasm32", test))]
impl Credential {
    pub fn valid(&self) -> bool {
        matches!(
            self.account.exchange,
            Exchange::Binance | Exchange::Bybit | Exchange::Okx
        ) && self.account.address.starts_with("api:")
            && self.account.address.len() == 36
            && self.account.address[4..]
                .bytes()
                .all(|c| c.is_ascii_hexdigit())
            && self.label.trim().len() <= 64
            && !self.label.trim().is_empty()
            && !self.label.chars().any(char::is_control)
            && [self.api_key.as_str(), self.secret.as_str()]
                .iter()
                .all(|s| {
                    !s.is_empty() && s.len() <= 512 && s.bytes().all(|b| (33..127).contains(&b))
                })
            && (self.account.exchange != Exchange::Okx
                || (!self.passphrase.is_empty()
                    && self.passphrase.len() <= 512
                    && self.passphrase.bytes().all(|b| (33..127).contains(&b))))
            && (self.spot || self.perp)
    }
}
#[cfg(any(target_arch = "wasm32", test))]
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub version: u32,
    pub iterations: u32,
    pub salt: Vec<u8>,
    pub iv: Vec<u8>,
    pub ciphertext: Vec<u8>,
}
#[cfg(any(target_arch = "wasm32", test))]
impl Envelope {
    pub fn valid(&self) -> bool {
        self.version == 1
            && self.iterations == 600_000
            && self.salt.len() == 16
            && self.iv.len() == 12
            && (16..=1024 * 1024).contains(&self.ciphertext.len())
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn secret_input(ui: &mut eframe::egui::Ui, value: &mut String, hint: &str, salt: &str) {
    ui.label(
        eframe::egui::RichText::new(hint)
            .size(11.0)
            .color(crate::MUTED),
    );
    let id = ui.make_persistent_id(("api-credential", salt));
    let mut output = eframe::egui::TextEdit::singleline(value)
        .id(id)
        .password(true)
        .desired_width(f32::INFINITY)
        .hint_text(hint)
        .show(ui);
    // Password masking does not disable egui's in-memory Undo history.
    output.state.clear_undoer();
    output.state.store(ui.ctx(), output.response.id);
}

#[cfg(target_arch = "wasm32")]
mod browser {
    use super::*;
    use eframe::egui;
    use js_sys::{Array, Object, Reflect, Uint8Array};
    use std::{cell::RefCell, rc::Rc};
    use wasm_bindgen::{JsCast, JsValue, closure::Closure};
    use wasm_bindgen_futures::{JsFuture, spawn_local};
    use web_sys::{CryptoKey, IdbDatabase, IdbRequest, IdbTransactionMode};

    pub fn object(fields: &[(&str, JsValue)]) -> Result<Object, JsValue> {
        let value = Object::new();
        for (key, field) in fields {
            Reflect::set(&value, &JsValue::from_str(key), field)?;
        }
        Ok(value)
    }
    pub fn random<const N: usize>() -> Result<[u8; N], JsValue> {
        let mut bytes = [0; N];
        web_sys::window()
            .ok_or(JsValue::NULL)?
            .crypto()?
            .get_random_values_with_u8_array(&mut bytes)?;
        Ok(bytes)
    }
    fn crypto() -> Result<web_sys::SubtleCrypto, JsValue> {
        Ok(web_sys::window().ok_or(JsValue::NULL)?.crypto()?.subtle())
    }
    async fn derive(
        mut password: String,
        salt: &[u8],
        iterations: u32,
    ) -> Result<CryptoKey, JsValue> {
        let bytes = Uint8Array::from(password.as_bytes());
        password.zeroize();
        let imported = crypto()?.import_key_with_str(
            "raw",
            &bytes,
            "PBKDF2",
            false,
            &Array::of1(&"deriveKey".into()),
        );
        bytes.fill(0, 0, bytes.length());
        let material: CryptoKey = JsFuture::from(imported?).await?.dyn_into()?;
        let algorithm = object(&[
            ("name", "PBKDF2".into()),
            ("hash", "SHA-256".into()),
            ("salt", Uint8Array::from(salt).into()),
            ("iterations", iterations.into()),
        ])?;
        let aes = object(&[("name", "AES-GCM".into()), ("length", 256.into())])?;
        JsFuture::from(crypto()?.derive_key_with_object_and_object(
            &algorithm,
            &material,
            &aes,
            false,
            &Array::of2(&"encrypt".into(), &"decrypt".into()),
        )?)
        .await?
        .dyn_into()
    }
    pub async fn hmac(secret: &str, message: &str) -> Result<Vec<u8>, JsValue> {
        let algorithm = object(&[("name", "HMAC".into()), ("hash", "SHA-256".into())])?;
        let bytes = Uint8Array::from(secret.as_bytes());
        let imported = crypto()?.import_key_with_object(
            "raw",
            &bytes,
            &algorithm,
            false,
            &Array::of1(&"sign".into()),
        );
        bytes.fill(0, 0, bytes.length());
        let key: CryptoKey = JsFuture::from(imported?).await?.dyn_into()?;
        let result = JsFuture::from(crypto()?.sign_with_str_and_u8_array(
            "HMAC",
            &key,
            message.as_bytes(),
        )?)
        .await?;
        Ok(Uint8Array::new(&result).to_vec())
    }
    async fn encrypt(
        key: &CryptoKey,
        salt: Vec<u8>,
        entries: &[Credential],
    ) -> Result<Envelope, JsValue> {
        let iv = random::<12>()?.to_vec();
        let algorithm = object(&[
            ("name", "AES-GCM".into()),
            ("iv", Uint8Array::from(iv.as_slice()).into()),
            (
                "additionalData",
                Uint8Array::from(b"terminal-api-vault-v1".as_slice()).into(),
            ),
        ])?;
        let mut plain = serde_json::to_vec(entries).map_err(|_| JsValue::NULL)?;
        let promise = crypto()?.encrypt_with_object_and_u8_array(&algorithm, key, &plain);
        plain.zeroize();
        let ciphertext = Uint8Array::new(&JsFuture::from(promise?).await?).to_vec();
        Ok(Envelope {
            version: 1,
            iterations: 600_000,
            salt,
            iv,
            ciphertext,
        })
    }
    async fn decrypt(key: &CryptoKey, envelope: &Envelope) -> Result<Vec<Credential>, JsValue> {
        if !envelope.valid() {
            return Err(JsValue::NULL);
        }
        let algorithm = object(&[
            ("name", "AES-GCM".into()),
            ("iv", Uint8Array::from(envelope.iv.as_slice()).into()),
            (
                "additionalData",
                Uint8Array::from(b"terminal-api-vault-v1".as_slice()).into(),
            ),
        ])?;
        let result = JsFuture::from(crypto()?.decrypt_with_object_and_u8_array(
            &algorithm,
            key,
            &envelope.ciphertext,
        )?)
        .await?;
        let mut plain = Uint8Array::new(&result).to_vec();
        let entries = serde_json::from_slice::<Vec<Credential>>(&plain).map_err(|_| JsValue::NULL);
        plain.zeroize();
        Uint8Array::new(&result).fill(0, 0, Uint8Array::new(&result).length());
        let entries = entries?;
        let unique: std::collections::HashSet<_> = entries.iter().map(|c| &c.account).collect();
        let keys: std::collections::HashSet<_> = entries
            .iter()
            .map(|c| (c.account.exchange, &c.api_key))
            .collect();
        if entries.len() > 32
            || unique.len() != entries.len()
            || keys.len() != entries.len()
            || !entries.iter().all(Credential::valid)
        {
            return Err(JsValue::NULL);
        }
        Ok(entries)
    }
    async fn request(request: &IdbRequest) -> Result<JsValue, JsValue> {
        let (send, receive) = futures_channel::oneshot::channel();
        let send = Rc::new(RefCell::new(Some(send)));
        let success = {
            let send = send.clone();
            let request = request.clone();
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                if let Some(send) = send.borrow_mut().take() {
                    let _ = send.send(request.result());
                }
            })
        };
        let error = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
            if let Some(send) = send.borrow_mut().take() {
                let _ = send.send(Err(JsValue::NULL));
            }
        });
        request.set_onsuccess(Some(success.as_ref().unchecked_ref()));
        request.set_onerror(Some(error.as_ref().unchecked_ref()));
        let result = receive.await.map_err(|_| JsValue::NULL)?;
        request.set_onsuccess(None);
        request.set_onerror(None);
        result
    }
    async fn database() -> Result<IdbDatabase, JsValue> {
        let open = web_sys::window()
            .ok_or(JsValue::NULL)?
            .indexed_db()?
            .ok_or(JsValue::NULL)?
            .open_with_u32("terminal-private", 1)?;
        let on_upgrade = {
            let open = open.clone();
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                if let Ok(value) = open.result() {
                    if let Ok(db) = value.dyn_into::<IdbDatabase>() {
                        let _ = db.create_object_store("vault");
                    }
                }
            })
        };
        open.set_onupgradeneeded(Some(on_upgrade.as_ref().unchecked_ref()));
        let result = request(&open).await;
        open.set_onupgradeneeded(None);
        result?.dyn_into()
    }
    async fn load() -> Result<Option<Envelope>, JsValue> {
        let db = database().await?;
        let transaction = db.transaction_with_str("vault")?;
        let value = request(&transaction.object_store("vault")?.get(&"accounts".into())?).await;
        db.close();
        let value = value?;
        if value.is_undefined() {
            return Ok(None);
        }
        let text = value.as_string().ok_or(JsValue::NULL)?;
        let envelope: Envelope = serde_json::from_str(&text).map_err(|_| JsValue::NULL)?;
        if !envelope.valid() {
            return Err(JsValue::NULL);
        }
        Ok(Some(envelope))
    }
    async fn store(envelope: &Envelope, expected: Option<&Envelope>) -> Result<(), JsValue> {
        let db = database().await?;
        let transaction =
            db.transaction_with_str_and_mode("vault", IdbTransactionMode::Readwrite)?;
        let text = serde_json::to_string(envelope).map_err(|_| JsValue::NULL)?;
        let expected = expected
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| JsValue::NULL)?;
        let object_store = transaction.object_store("vault")?;
        let read = object_store.get(&"accounts".into())?;
        // Compare and replace in one IndexedDB transaction: another tab cannot lose its keys.
        let compare = {
            let read = read.clone();
            let transaction = transaction.clone();
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                let matches = read.result().ok().is_some_and(|value| {
                    if value.is_undefined() {
                        expected.is_none()
                    } else {
                        value.as_string().as_ref() == expected.as_ref()
                    }
                });
                if !matches
                    || object_store
                        .put_with_key(&JsValue::from_str(&text), &"accounts".into())
                        .is_err()
                {
                    let _ = transaction.abort();
                }
            })
        };
        read.set_onsuccess(Some(compare.as_ref().unchecked_ref()));
        let (send, receive) = futures_channel::oneshot::channel();
        let send = Rc::new(RefCell::new(Some(send)));
        let complete = {
            let send = send.clone();
            Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                if let Some(send) = send.borrow_mut().take() {
                    let _ = send.send(true);
                }
            })
        };
        let failed = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
            if let Some(send) = send.borrow_mut().take() {
                let _ = send.send(false);
            }
        });
        transaction.set_oncomplete(Some(complete.as_ref().unchecked_ref()));
        transaction.set_onabort(Some(failed.as_ref().unchecked_ref()));
        transaction.set_onerror(Some(failed.as_ref().unchecked_ref()));
        let ok = receive.await.unwrap_or(false);
        read.set_onsuccess(None);
        transaction.set_oncomplete(None);
        transaction.set_onabort(None);
        transaction.set_onerror(None);
        db.close();
        if ok { Ok(()) } else { Err(JsValue::NULL) }
    }
    struct Data {
        envelope: Option<Envelope>,
        key: Option<CryptoKey>,
        entries: Vec<Credential>,
        busy: bool,
        error: Option<&'static str>,
        generation: u64,
        connected: bool,
    }
    pub struct Vault {
        data: Rc<RefCell<Data>>,
        password: String,
        backup_password: String,
        confirmation: String,
        pub draft: Credential,
        backup: String,
        importing: bool,
    }
    impl Default for Vault {
        fn default() -> Self {
            Self {
                data: Rc::new(RefCell::new(Data {
                    envelope: None,
                    key: None,
                    entries: Vec::new(),
                    busy: false,
                    error: None,
                    generation: 0,
                    connected: false,
                })),
                password: String::new(),
                backup_password: String::new(),
                confirmation: String::new(),
                draft: empty(),
                backup: String::new(),
                importing: false,
            }
        }
    }
    impl Drop for Vault {
        fn drop(&mut self) {
            self.password.zeroize();
            self.backup_password.zeroize();
            self.confirmation.zeroize();
        }
    }
    fn empty() -> Credential {
        Credential {
            account: Account {
                exchange: Exchange::Binance,
                address: String::new(),
            },
            label: String::new(),
            api_key: String::new(),
            secret: String::new(),
            passphrase: String::new(),
            spot: true,
            perp: true,
        }
    }
    impl Vault {
        pub fn load(&mut self, ctx: &egui::Context) {
            self.data.borrow_mut().busy = true;
            let data = self.data.clone();
            let ctx = ctx.clone();
            spawn_local(async move {
                let result = load().await;
                let mut data = data.borrow_mut();
                data.busy = false;
                match result {
                    Ok(envelope) => data.envelope = envelope,
                    Err(_) => {
                        data.error = Some(
                            "Cannot read encrypted storage. Check browser storage permissions.",
                        )
                    }
                }
                ctx.request_repaint();
            });
        }
        pub fn accounts(&self) -> Vec<Account> {
            self.data
                .borrow()
                .entries
                .iter()
                .map(|c| c.account.clone())
                .collect()
        }
        pub fn label(&self, account: &Account) -> Option<String> {
            self.data
                .borrow()
                .entries
                .iter()
                .find(|c| &c.account == account)
                .map(|c| c.label.clone())
        }
        pub fn credential(&self, account: &Account) -> Option<Credential> {
            self.data
                .borrow()
                .entries
                .iter()
                .find(|c| &c.account == account)
                .cloned()
        }
        pub fn entries(&self) -> Vec<Credential> {
            self.data.borrow().entries.clone()
        }
        pub fn close_ui(&mut self) {
            self.password.zeroize();
            self.backup_password.zeroize();
            self.confirmation.zeroize();
            self.draft = empty();
            self.backup.clear();
            self.importing = false;
        }
        pub fn select_exchange(&mut self, exchange: Exchange) {
            self.close_ui();
            self.draft.account.exchange = exchange;
            self.data.borrow_mut().error = None;
        }
        pub fn take_connected(&mut self) -> bool {
            std::mem::take(&mut self.data.borrow_mut().connected)
        }
        pub fn can_edit(&self) -> bool {
            self.unlocked() && !self.data.borrow().busy
        }
        pub fn generation(&self) -> u64 {
            self.data.borrow().generation
        }
        pub fn unlocked(&self) -> bool {
            self.data.borrow().key.is_some()
        }
        pub fn lock(&mut self) {
            let mut data = self.data.borrow_mut();
            data.generation += 1;
            data.entries.clear();
            data.key = None;
            self.password.zeroize();
            self.backup_password.zeroize();
            self.confirmation.zeroize();
            self.draft = empty();
            self.backup.clear();
        }
        pub fn remove(&mut self, account: &Account, ctx: &egui::Context) {
            let entries = self
                .entries()
                .into_iter()
                .filter(|c| &c.account != account)
                .collect();
            self.save(entries, ctx);
        }
        fn save(&mut self, entries: Vec<Credential>, ctx: &egui::Context) {
            let Some(key) = self.data.borrow().key.clone() else {
                return;
            };
            let expected = self.data.borrow().envelope.clone();
            let salt = self
                .data
                .borrow()
                .envelope
                .as_ref()
                .map(|e| e.salt.clone())
                .unwrap_or_default();
            let data = self.data.clone();
            let ctx = ctx.clone();
            let generation = data.borrow().generation;
            data.borrow_mut().busy = true;
            spawn_local(async move {
                let result = async {
                    let envelope = encrypt(&key, salt, &entries).await?;
                    store(&envelope, expected.as_ref()).await?;
                    Ok::<_, JsValue>(envelope)
                }
                .await;
                let mut data = data.borrow_mut();
                data.busy = false;
                if generation == data.generation {
                    match result {
                        Ok(envelope) => {
                            data.connected = entries.len() > data.entries.len();
                            data.envelope = Some(envelope);
                            data.entries = entries;
                        }
                        Err(_) => {
                            data.error = Some(
                                "Storage changed in another tab or is unavailable. Reload before retrying; saved accounts were kept.",
                            )
                        }
                    }
                }
                ctx.request_repaint();
            });
        }
        pub fn ui(&mut self, ui: &mut egui::Ui, exchange: Option<Exchange>) {
            if !web_sys::window().is_some_and(|w| w.is_secure_context()) {
                ui.colored_label(crate::RED, "API accounts require HTTPS or localhost.");
                return;
            }
            if self.data.borrow().busy {
                ui.label("Working…");
                return;
            }
            if let Some(error) = self.data.borrow().error {
                ui.colored_label(crate::RED, error);
            }
            if !self.unlocked() {
                let creating = self.data.borrow().envelope.is_none();
                ui.label(if creating {
                    "Create a password for your API keys"
                } else {
                    "Unlock your API accounts"
                });
                secret_input(ui, &mut self.password, "API keys password", "master");
                if creating {
                    secret_input(
                        ui,
                        &mut self.confirmation,
                        "Confirm password",
                        "confirmation",
                    );
                    ui.weak("Use at least 12 characters. This password cannot be recovered.");
                }
                if crate::accounts::primary(
                    ui,
                    if creating {
                        "Create encrypted storage"
                    } else {
                        "Unlock"
                    },
                )
                .clicked()
                {
                    if creating
                        && (self.password.chars().count() < 12
                            || self.password != self.confirmation)
                    {
                        self.data.borrow_mut().error =
                            Some("Use matching passwords of at least 12 characters.");
                        return;
                    }
                    let password = std::mem::take(&mut self.password);
                    self.confirmation.zeroize();
                    let data = self.data.clone();
                    let ctx = ui.ctx().clone();
                    let old = data.borrow().envelope.clone();
                    data.borrow_mut().busy = true;
                    spawn_local(async move {
                        let result = async {
                            let latest = load().await?;
                            if old.is_none() != latest.is_none() {
                                return Err(JsValue::NULL);
                            }
                            let old = latest;
                            let salt = match &old {
                                Some(e) => e.salt.clone(),
                                None => random::<16>()?.to_vec(),
                            };
                            let key = derive(
                                password,
                                &salt,
                                old.as_ref().map_or(600_000, |e| e.iterations),
                            )
                            .await?;
                            let (entries, envelope) = if let Some(envelope) = old {
                                (decrypt(&key, &envelope).await?, envelope)
                            } else {
                                let envelope = encrypt(&key, salt, &[]).await?;
                                store(&envelope, None).await?;
                                (Vec::new(), envelope)
                            };
                            Ok::<_, JsValue>((entries, envelope, key))
                        }
                        .await;
                        let mut data = data.borrow_mut();
                        data.busy = false;
                        match result {
                            Ok((entries, envelope, key)) => {
                                data.entries = entries;
                                data.envelope = Some(envelope);
                                data.key = Some(key);
                                data.error = None;
                                data.generation += 1;
                            }
                            Err(_) => {
                                data.error =
                                    Some("Wrong password, damaged backup, or storage unavailable.")
                            }
                        }
                        ctx.request_repaint();
                    });
                }
            } else {
                if exchange.is_none() {
                    ui.label("Keys are encrypted in this browser.");
                    ui.horizontal(|ui| {
                        ui.label("Unlocked for this session");
                        if ui.small_button("Lock").clicked() {
                            self.lock();
                        }
                        if ui.small_button("Export encrypted backup").clicked() {
                            self.backup = self
                                .data
                                .borrow()
                                .envelope
                                .as_ref()
                                .and_then(|e| serde_json::to_string(e).ok())
                                .unwrap_or_default();
                            self.importing = false;
                        }
                    });
                    if !self.unlocked() {
                        return;
                    }
                    ui.separator();
                } else {
                    ui.label(
                        egui::RichText::new("Account name")
                            .size(11.0)
                            .color(crate::MUTED),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut self.draft.label)
                            .hint_text("e.g. Main account")
                            .desired_width(f32::INFINITY),
                    );
                    secret_input(ui, &mut self.draft.api_key, "API Key", "key");
                    secret_input(ui, &mut self.draft.secret, "API Secret", "secret");
                    if self.draft.account.exchange == Exchange::Okx {
                        secret_input(
                            ui,
                            &mut self.draft.passphrase,
                            "API Passphrase",
                            "passphrase",
                        );
                    }
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.draft.spot, "Spot");
                        ui.checkbox(&mut self.draft.perp, "Linear Perps");
                    });
                    ui.weak(
                        "Use read-only keys. Trading, transfers and withdrawals must be disabled.",
                    );
                    if crate::accounts::primary(ui, "Save & connect").clicked() {
                        let id = random::<16>().map(|bytes| {
                            bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
                        });
                        if let Ok(id) = id {
                            self.draft.account.address = format!("api:{id}");
                        }
                        if self.entries().iter().any(|c| {
                            c.account.exchange == self.draft.account.exchange
                                && c.api_key == self.draft.api_key
                        }) {
                            self.data.borrow_mut().error =
                                Some("This exchange API key is already connected.");
                            return;
                        }
                        if !self.draft.valid() || self.entries().len() >= 32 {
                            self.data.borrow_mut().error = Some(
                                "Check account name, credentials and market selection (maximum 32 accounts).",
                            );
                        } else {
                            let mut entries = self.entries();
                            entries.push(self.draft.clone());
                            self.draft = empty();
                            self.save(entries, ui.ctx());
                        }
                    }
                }
            }
            if exchange.is_some() {
                return;
            }
            ui.separator();
            if ui.small_button("Import encrypted backup").clicked() {
                self.importing = true;
                self.backup.clear();
            }
            if self.importing || !self.backup.is_empty() {
                ui.add(
                    egui::TextEdit::multiline(&mut self.backup)
                        .desired_rows(3)
                        .desired_width(290.0)
                        .hint_text("Encrypted backup JSON"),
                );
                if self.importing {
                    ui.weak("Import replaces this browser's saved API accounts after checking the backup password.");
                    secret_input(ui, &mut self.backup_password, "Backup password", "backup");
                    if ui.button("Replace with this encrypted backup").clicked() {
                        let envelope = (self.backup.len() <= 5 * 1024 * 1024)
                            .then(|| serde_json::from_str::<Envelope>(&self.backup).ok())
                            .flatten()
                            .filter(Envelope::valid);
                        if let Some(envelope) = envelope {
                            let password = std::mem::take(&mut self.backup_password);
                            let expected = self.data.borrow().envelope.clone();
                            self.data.borrow_mut().busy = true;
                            let data = self.data.clone();
                            let ctx = ui.ctx().clone();
                            spawn_local(async move {
                                let result = async {
                                    let key = derive(password, &envelope.salt, envelope.iterations)
                                        .await?;
                                    let entries = decrypt(&key, &envelope).await?;
                                    store(&envelope, expected.as_ref()).await?;
                                    Ok::<_, JsValue>((key, entries))
                                }
                                .await;
                                let mut data = data.borrow_mut();
                                data.busy = false;
                                match result {
                                    Ok((key, entries)) => {
                                        data.generation += 1;
                                        data.envelope = Some(envelope);
                                        data.key = Some(key);
                                        data.entries = entries;
                                        data.error = None;
                                    }
                                    Err(_) => {
                                        data.error = Some(
                                            "Import failed or wrong backup password. Previous accounts were kept.",
                                        )
                                    }
                                }
                                ctx.request_repaint();
                            });
                        } else {
                            self.data.borrow_mut().error = Some("Invalid encrypted backup.");
                        }
                    }
                } else {
                    ui.weak("Copy this encrypted backup and keep it with your password stored separately.");
                }
            }
        }
    }
}
#[cfg(target_arch = "wasm32")]
pub use browser::*;
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub struct Vault;
#[cfg(not(target_arch = "wasm32"))]
impl Vault {
    pub fn label(&self, _: &Account) -> Option<String> {
        None
    }
    pub fn can_edit(&self) -> bool {
        false
    }
    pub fn unlocked(&self) -> bool {
        false
    }
    pub fn lock(&mut self) {}
    pub fn remove(&mut self, _: &Account, _: &eframe::egui::Context) {}
    pub fn close_ui(&mut self) {}
    pub fn select_exchange(&mut self, _: terminal_core::Exchange) {}
    pub fn take_connected(&mut self) -> bool {
        false
    }
    pub fn ui(&mut self, ui: &mut eframe::egui::Ui, _: Option<terminal_core::Exchange>) {
        ui.weak("API accounts are available in the web terminal over HTTPS or localhost.");
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secret_fields_do_not_retain_undo_text() {
        let ctx = eframe::egui::Context::default();
        let mut password = "test-only-secret".to_owned();
        let mut output = ctx.run_ui(Default::default(), |ui| {
            let id = ui.make_persistent_id(("api-credential", "test"));
            let mut state = eframe::egui::text_edit::TextEditState::default();
            let current = (
                eframe::egui::text::CCursorRange::one(eframe::egui::text::CCursor::new(0)),
                password.clone(),
            );
            let mut undo = state.undoer();
            undo.add_undo(&current);
            state.set_undoer(undo);
            state.store(ui.ctx(), id);
            secret_input(ui, &mut password, "Test password", "test");
            let state = eframe::egui::text_edit::TextEditState::load(ui.ctx(), id).unwrap();
            assert!(!state.undoer().has_undo(&(current.0, String::new())));
            assert!(
                !serde_json::to_string(&state)
                    .unwrap()
                    .contains("test-only-secret")
            );
        });
        output.textures_delta.clear();
    }
    #[test]
    fn credentials_validate_scope_and_keep_secrets_out_of_the_envelope() {
        let mut credential = Credential {
            account: Account {
                exchange: Exchange::Okx,
                address: format!("api:{}", "a".repeat(32)),
            },
            label: "Test account".into(),
            api_key: "test-key".into(),
            secret: "test-secret".into(),
            passphrase: "test-api-passphrase".into(),
            spot: true,
            perp: true,
        };
        assert!(credential.valid());
        credential.passphrase.clear();
        assert!(!credential.valid());
        credential.account.exchange = Exchange::Bybit;
        assert!(credential.valid());
        credential.spot = false;
        credential.perp = false;
        assert!(!credential.valid());
        let envelope = Envelope {
            version: 1,
            iterations: 600_000,
            salt: vec![1; 16],
            iv: vec![2; 12],
            ciphertext: vec![3; 32],
        };
        let json = serde_json::to_string(&envelope).unwrap();
        for secret in [&credential.api_key, &credential.secret] {
            assert!(!json.contains(secret.as_str()));
        }
    }
    #[test]
    fn encrypted_container_rejects_unsafe_or_corrupt_parameters() {
        let mut e = Envelope {
            version: 1,
            iterations: 600_000,
            salt: vec![0; 16],
            iv: vec![0; 12],
            ciphertext: vec![0; 16],
        };
        assert!(e.valid());
        e.iterations = 1;
        assert!(!e.valid());
        e.iterations = u32::MAX;
        assert!(!e.valid());
        e.iterations = 600_000;
        e.iv.pop();
        assert!(!e.valid());
        assert!(serde_json::from_str::<Envelope>(r#"{"version":1,"iterations":600000,"salt":[],"iv":[],"ciphertext":[],"secret":"not allowed"}"#).is_err());
    }
}
