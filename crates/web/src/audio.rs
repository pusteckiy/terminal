//! Small, shared trade cues. Only live tape events enter the bounded audio batch.
use eframe::egui::{self, RichText, Stroke};
use serde::{Deserialize, Serialize};
use terminal_core::{Trade, TradeSide};

const INTERVAL: f64 = 0.100;

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    enabled: bool,
    volume: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            volume: 25,
        }
    }
}

#[derive(Default)]
struct Batch {
    sides: [bool; 2],
    next: f64,
}

impl Batch {
    fn push(&mut self, trades: &[Trade], now_ms: i64, armed_at_ms: i64) {
        for trade in trades {
            // Subscription snapshots/replayed old trades must not sound like new executions.
            if trade.time_ms < armed_at_ms || !(-1_000..=1_500).contains(&(now_ms - trade.time_ms))
            {
                continue;
            }
            match trade.side {
                TradeSide::Buy => self.sides[0] = true,
                TradeSide::Sell => self.sides[1] = true,
                TradeSide::Unknown => {}
            }
        }
    }

    fn take(&mut self, now: f64) -> Option<[bool; 2]> {
        if now < self.next || !self.sides.iter().any(|side| *side) {
            return None;
        }
        self.next = now + INTERVAL;
        Some(std::mem::take(&mut self.sides))
    }
}

pub struct TradeAudio {
    settings: Settings,
    armed_at_ms: i64,
    batch: Batch,
    #[cfg(target_arch = "wasm32")]
    output: browser::Output,
}

impl TradeAudio {
    pub fn new(mut settings: Settings) -> Self {
        settings.volume = settings.volume.min(100);
        Self {
            settings,
            armed_at_ms: crate::utc_now_ms(),
            batch: Batch::default(),
            #[cfg(target_arch = "wasm32")]
            output: browser::Output::new(settings),
        }
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    pub fn clear_pending(&mut self) {
        self.batch = Batch::default();
    }

    pub fn trades(&mut self, trades: &[Trade], now_ms: i64) {
        if self.settings.enabled && self.settings.volume > 0 {
            self.batch.push(trades, now_ms, self.armed_at_ms);
        }
    }

    pub fn flush(&mut self, ctx: &egui::Context) {
        if !self.settings.enabled || self.settings.volume == 0 {
            self.clear_pending();
            return;
        }
        let now = ctx.input(|input| input.time);
        if let Some(sides) = self.batch.take(now) {
            #[cfg(target_arch = "wasm32")]
            self.output.play(sides);
            #[cfg(not(target_arch = "wasm32"))]
            let _ = sides;
        }
        if self.batch.sides.iter().any(|side| *side) {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(
                (self.batch.next - now).max(0.0),
            ));
        }
    }

    pub fn menu(&mut self, ui: &mut egui::Ui) {
        let was_enabled = self.settings.enabled;
        let button = egui::Button::new("")
            .fill(crate::RAISED)
            .stroke(Stroke::new(1.0, crate::BORDER))
            .corner_radius(egui::CornerRadius::same(6))
            .min_size(egui::vec2(29.0, 25.0));
        let (response, _) = egui::menu::MenuButton::from_button(button)
            .config(
                egui::menu::MenuConfig::new()
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside),
            )
            .ui(ui, |ui| {
                ui.set_min_width(185.0);
                ui.add_enabled_ui(cfg!(target_arch = "wasm32"), |ui| {
                    ui.checkbox(&mut self.settings.enabled, "Trade sounds");
                    ui.horizontal(|ui| {
                        ui.label("Volume");
                        ui.add(egui::Slider::new(&mut self.settings.volume, 0..=100).suffix("%"));
                    });
                });
                #[cfg(not(target_arch = "wasm32"))]
                ui.label(
                    RichText::new("Available in the browser")
                        .small()
                        .color(crate::MUTED),
                );
                #[cfg(target_arch = "wasm32")]
                if let Some(message) = self.output.message(self.settings.enabled) {
                    ui.label(RichText::new(message).small().color(crate::MUTED));
                }
            });
        if was_enabled != self.settings.enabled {
            self.clear_pending();
            self.armed_at_ms = crate::utc_now_ms();
        }
        #[cfg(target_arch = "wasm32")]
        self.output.configure(self.settings);

        let c = response.rect.center();
        let color = if self.settings.enabled && self.settings.volume > 0 {
            crate::GREEN
        } else {
            crate::MUTED
        };
        let stroke = Stroke::new(1.3, color);
        ui.painter().add(egui::Shape::closed_line(
            vec![
                c + egui::vec2(-6.0, -2.5),
                c + egui::vec2(-3.0, -2.5),
                c + egui::vec2(0.0, -5.0),
                c + egui::vec2(0.0, 5.0),
                c + egui::vec2(-3.0, 2.5),
                c + egui::vec2(-6.0, 2.5),
            ],
            stroke,
        ));
        if self.settings.enabled && self.settings.volume > 0 {
            ui.painter().add(egui::Shape::line(
                vec![
                    c + egui::vec2(3.0, -4.0),
                    c + egui::vec2(5.0, -2.0),
                    c + egui::vec2(5.5, 0.0),
                    c + egui::vec2(5.0, 2.0),
                    c + egui::vec2(3.0, 4.0),
                ],
                stroke,
            ));
        } else {
            ui.painter().line_segment(
                [c + egui::vec2(2.0, -3.0), c + egui::vec2(6.0, 3.0)],
                stroke,
            );
        }
        response.on_hover_text(if self.settings.enabled {
            "Trade sounds · On"
        } else {
            "Trade sounds · Off"
        });
    }
}

// Precomputed once per audio context: warm, short notes with soft edges and a
// little filtered texture. No audio files, oscillators per trade, or sample downloads.
#[cfg(any(target_arch = "wasm32", test))]
fn samples(rate: f32, buy: bool) -> Vec<f32> {
    let count = (rate * 0.070) as usize;
    let frequency = if buy { 660.0 } else { 440.0 };
    let mut noise = 0_u32;
    let mut smooth = 0.0;
    (0..count)
        .map(|index| {
            let time = index as f32 / rate;
            noise = noise.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            smooth = 0.92 * smooth + 0.08 * (noise as f32 / u32::MAX as f32 * 2.0 - 1.0);
            let attack = (time / 0.004).min(1.0);
            let release = ((count - 1 - index) as f32 / (rate * 0.012)).min(1.0);
            let envelope = attack * release * (-65.0 * time).exp();
            let phase = std::f32::consts::TAU * frequency * time;
            (phase.sin() + 0.25 * (phase * 1.8).sin() + 0.12 * smooth) * envelope * 0.22
        })
        .collect()
}

#[cfg(target_arch = "wasm32")]
mod browser {
    use super::*;
    use eframe::wasm_bindgen::{JsCast, JsValue, closure::Closure};
    use std::{cell::RefCell, rc::Rc};
    use web_sys::{AudioBuffer, AudioContext, AudioContextState, GainNode};

    const GESTURES: [&str; 4] = ["pointerdown", "mousedown", "touchend", "keydown"];

    struct Engine {
        context: AudioContext,
        master: GainNode,
        buffers: [AudioBuffer; 2],
    }

    impl Engine {
        fn new() -> Result<Self, JsValue> {
            let context = AudioContext::new()?;
            let master = context.create_gain()?;
            master.gain().set_value(0.0);
            master.connect_with_audio_node(&context.destination())?;
            let buffer = |buy| -> Result<AudioBuffer, JsValue> {
                let data = samples(context.sample_rate(), buy);
                let buffer = context.create_buffer(1, data.len() as u32, context.sample_rate())?;
                buffer.copy_to_channel(&data, 0)?;
                Ok(buffer)
            };
            let buffers = [buffer(true)?, buffer(false)?];
            Ok(Self {
                context,
                master,
                buffers,
            })
        }

        fn volume(&self, settings: Settings) {
            let gain = if settings.enabled {
                (settings.volume as f32 / 100.0).powi(2)
            } else {
                0.0
            };
            let _ = self
                .master
                .gain()
                .set_target_at_time(gain, self.context.current_time(), 0.015);
        }

        fn play(&self, sides: [bool; 2]) -> Result<(), JsValue> {
            if self.context.state() != AudioContextState::Running {
                return Ok(());
            }
            let now = self.context.current_time();
            for (index, present) in sides.into_iter().enumerate() {
                if !present {
                    continue;
                }
                let source = self.context.create_buffer_source()?;
                source.set_buffer(Some(&self.buffers[index]));
                source.connect_with_audio_node(&self.master)?;
                source.start_with_when(now)?;
                // One-shot sources are released by the browser after their 70 ms buffer ends.
            }
            Ok(())
        }
    }

    struct State {
        settings: Settings,
        engine: Option<Engine>,
        error: bool,
    }

    pub struct Output {
        state: Rc<RefCell<State>>,
        gesture: Closure<dyn FnMut(web_sys::Event)>,
    }

    impl Output {
        pub fn new(settings: Settings) -> Self {
            let state = Rc::new(RefCell::new(State {
                settings,
                engine: None,
                error: false,
            }));
            let shared = state.clone();
            // egui draws on requestAnimationFrame. Unlock inside the actual trusted
            // input event so Safari and Chromium both honour their autoplay policy.
            let gesture =
                Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
                    if !event.is_trusted() {
                        return;
                    }
                    let mut state = shared.borrow_mut();
                    if state.engine.is_none() {
                        match Engine::new() {
                            Ok(engine) => state.engine = Some(engine),
                            Err(_) => {
                                state.error = true;
                                return;
                            }
                        }
                    }
                    let engine = state.engine.as_ref().unwrap();
                    engine.volume(state.settings);
                    if engine.context.state() == AudioContextState::Suspended {
                        if let Ok(promise) = engine.context.resume() {
                            let shared = shared.clone();
                            wasm_bindgen_futures::spawn_local(async move {
                                if wasm_bindgen_futures::JsFuture::from(promise).await.is_err() {
                                    shared.borrow_mut().error = true;
                                }
                            });
                        }
                    }
                });
            if let Some(window) = web_sys::window() {
                for event in GESTURES {
                    // Capture before egui's canvas listeners stop event propagation.
                    let _ = window.add_event_listener_with_callback_and_bool(
                        event,
                        gesture.as_ref().unchecked_ref(),
                        true,
                    );
                }
            }
            Self { state, gesture }
        }

        pub fn configure(&self, settings: Settings) {
            let mut state = self.state.borrow_mut();
            if state.settings.enabled != settings.enabled
                || state.settings.volume != settings.volume
            {
                state.settings = settings;
                if let Some(engine) = &state.engine {
                    engine.volume(settings);
                }
            }
        }

        pub fn play(&self, sides: [bool; 2]) {
            let mut state = self.state.borrow_mut();
            if let Some(engine) = &state.engine {
                if engine.play(sides).is_err() {
                    state.error = true;
                }
            }
        }

        pub fn message(&self, enabled: bool) -> Option<&'static str> {
            let state = self.state.borrow();
            if state.error {
                Some("Audio is unavailable in this browser")
            } else if enabled
                && state
                    .engine
                    .as_ref()
                    .is_none_or(|engine| engine.context.state() != AudioContextState::Running)
            {
                Some("Click in the terminal to enable audio")
            } else {
                None
            }
        }
    }

    impl Drop for Output {
        fn drop(&mut self) {
            if let Some(window) = web_sys::window() {
                for event in GESTURES {
                    let _ = window.remove_event_listener_with_callback_and_bool(
                        event,
                        self.gesture.as_ref().unchecked_ref(),
                        true,
                    );
                }
            }
            if let Some(engine) = &self.state.borrow().engine {
                if let Ok(promise) = engine.context.close() {
                    wasm_bindgen_futures::spawn_local(async move {
                        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trade(time_ms: i64, side: TradeSide) -> Trade {
        Trade {
            time_ms,
            side,
            price: "100".into(),
            size: "0.000001".into(),
        }
    }

    #[test]
    fn dense_live_tape_is_bounded_and_both_sides_sound_without_a_size_filter() {
        let mut batch = Batch::default();
        let trades: Vec<_> = (0..50_000)
            .map(|index| {
                trade(
                    10_000,
                    if index % 2 == 0 {
                        TradeSide::Buy
                    } else {
                        TradeSide::Sell
                    },
                )
            })
            .collect();
        batch.push(&trades, 10_000, 9_900);
        assert_eq!(batch.take(1.0), Some([true, true]));
        batch.push(&[trade(10_020, TradeSide::Buy)], 10_020, 9_900);
        assert_eq!(batch.take(1.05), None);
        assert_eq!(batch.take(1.101), Some([true, false]));
        assert_eq!(batch.take(2.0), None);
    }

    #[test]
    fn history_unknown_sides_and_muted_trades_do_not_replay_after_enabling() {
        let mut audio = TradeAudio::new(Settings::default());
        audio.trades(&[trade(10_000, TradeSide::Buy)], 10_000);
        assert_eq!(audio.batch.take(1.0), None);
        audio.settings.enabled = true;
        audio.armed_at_ms = 10_000;
        audio.trades(
            &[
                trade(9_999, TradeSide::Buy),
                trade(10_000, TradeSide::Unknown),
            ],
            10_000,
        );
        assert_eq!(audio.batch.take(1.0), None);
        audio.trades(&[trade(10_000, TradeSide::Buy)], 12_000);
        assert_eq!(audio.batch.take(1.0), None);
        audio.settings.volume = 0;
        audio.trades(&[trade(12_000, TradeSide::Sell)], 12_000);
        assert_eq!(audio.batch.take(1.0), None);
        audio.settings.volume = 25;
        audio.trades(&[trade(12_001, TradeSide::Sell)], 12_001);
        assert_eq!(audio.batch.take(1.0), Some([false, true]));
        audio.trades(&[trade(12_002, TradeSide::Buy)], 12_002);
        audio.settings.enabled = false;
        audio.flush(&egui::Context::default());
        assert_eq!(audio.batch.take(2.0), None);
    }

    #[test]
    fn synthesized_cues_are_short_finite_and_have_silent_edges() {
        for rate in [44_100.0, 48_000.0] {
            let buy = samples(rate, true);
            let sell = samples(rate, false);
            assert_ne!(buy, sell);
            for data in [buy, sell] {
                assert!(data.len() < (rate / 10.0) as usize);
                assert_eq!(data.first(), Some(&0.0));
                assert_eq!(data.last(), Some(&0.0));
                assert!(
                    data.iter()
                        .all(|sample| sample.is_finite() && sample.abs() < 0.3)
                );
                assert!(data.iter().any(|sample| sample.abs() > 0.05));
            }
        }
        let saved = serde_json::to_string(&Settings {
            enabled: true,
            volume: 42,
        })
        .unwrap();
        let loaded: Settings = serde_json::from_str(&saved).unwrap();
        assert!(loaded.enabled);
        assert_eq!(loaded.volume, 42);
    }
}
