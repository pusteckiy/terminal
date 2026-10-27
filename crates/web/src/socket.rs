//! Ordered, bounded socket input. Overflow requires a fresh snapshot, never a
//! partial order-book delta stream with silently dropped messages.
use eframe::egui;
use ewebsock::{WsEvent, WsMessage, WsSender};
use std::{
    collections::VecDeque,
    ops::ControlFlow,
    sync::{Arc, Mutex},
};

const MAX_EVENTS: usize = 16_384;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_QUEUE_AGE_MS: u64 = 5_000;

struct Received {
    event: WsEvent,
    at_ms: u64,
}

#[derive(Default)]
struct Queue {
    events: VecDeque<Received>,
    bytes: usize,
    overflowed: bool,
}

#[derive(Default)]
pub struct Inbox(Arc<Mutex<Queue>>);

fn event_bytes(event: &WsEvent) -> usize {
    match event {
        WsEvent::Message(WsMessage::Text(text) | WsMessage::Unknown(text))
        | WsEvent::Error(text) => text.len(),
        WsEvent::Message(
            WsMessage::Binary(bytes) | WsMessage::Ping(bytes) | WsMessage::Pong(bytes),
        ) => bytes.len(),
        _ => 0,
    }
}

pub fn channel(
    wake: impl Fn() + Send + 'static,
) -> (Inbox, impl Fn(WsEvent) -> ControlFlow<()> + Send + 'static) {
    let inbox = Inbox::default();
    // Old WebSocket callbacks must not retain an abandoned input queue.
    let weak = Arc::downgrade(&inbox.0);
    let handler = move |event| {
        let Some(queue) = weak.upgrade() else {
            return ControlFlow::Break(());
        };
        let mut queue = queue.lock().unwrap();
        if queue.overflowed {
            return ControlFlow::Break(());
        }
        let bytes = event_bytes(&event);
        let result =
            if queue.events.len() >= MAX_EVENTS || bytes > MAX_BYTES.saturating_sub(queue.bytes) {
                queue.events.clear();
                queue.bytes = 0;
                queue.overflowed = true;
                ControlFlow::Break(())
            } else {
                queue.bytes += bytes;
                queue.events.push_back(Received {
                    event,
                    at_ms: now_ms(),
                });
                ControlFlow::Continue(())
            };
        drop(queue);
        wake();
        result
    };
    (inbox, handler)
}

pub fn connect(url: String, ctx: &egui::Context) -> ewebsock::Result<(WsSender, Inbox)> {
    let ctx = ctx.clone();
    let (inbox, handler) = channel(move || ctx.request_repaint());
    ewebsock::ws_connect(url, ewebsock::Options::default(), Box::new(handler))
        .map(|sender| (sender, inbox))
}

impl Inbox {
    pub fn try_recv(&self) -> Option<WsEvent> {
        let mut queue = self.0.lock().unwrap();
        let received = queue.events.pop_front()?;
        queue.bytes -= event_bytes(&received.event);
        Some(received.event)
    }

    pub fn needs_resync(&self, now_ms: u64) -> bool {
        let queue = self.0.lock().unwrap();
        queue.overflowed
            || queue
                .events
                .front()
                .is_some_and(|received| now_ms.saturating_sub(received.at_ms) > MAX_QUEUE_AGE_MS)
    }

    pub fn has_pending(&self) -> bool {
        let queue = self.0.lock().unwrap();
        queue.overflowed || !queue.events.is_empty()
    }
}

pub fn now_ms() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()
            .and_then(|window| window.performance())
            .map_or(0, |clock| clock.now() as u64)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_preserves_order_and_limits_both_count_and_bytes() {
        let (inbox, send) = channel(|| {});
        assert!(send(WsEvent::Opened).is_continue());
        assert!(send(WsEvent::Message(WsMessage::Text("delta".into()))).is_continue());
        assert!(matches!(inbox.try_recv(), Some(WsEvent::Opened)));
        assert!(
            matches!(inbox.try_recv(), Some(WsEvent::Message(WsMessage::Text(text))) if text == "delta")
        );
        assert!(!inbox.has_pending());
        for _ in 0..MAX_EVENTS {
            assert!(send(WsEvent::Opened).is_continue());
        }
        assert!(send(WsEvent::Opened).is_break());
        assert!(inbox.needs_resync(now_ms()));
        assert!(
            inbox.try_recv().is_none(),
            "never keep a partial delta stream after overflow"
        );
        assert!(send(WsEvent::Opened).is_break());
        let (inbox, send) = channel(|| {});
        assert!(send(WsEvent::Message(WsMessage::Text("x".repeat(MAX_BYTES)))).is_continue());
        assert!(send(WsEvent::Message(WsMessage::Text("x".into()))).is_break());
        assert!(inbox.needs_resync(now_ms()));
        assert_eq!(inbox.0.lock().unwrap().bytes, 0);
    }

    #[test]
    fn queue_resyncs_when_old_and_releases_abandoned_callbacks() {
        let (inbox, send) = channel(|| {});
        assert!(send(WsEvent::Opened).is_continue());
        let received_at = inbox.0.lock().unwrap().events[0].at_ms;
        assert!(!inbox.needs_resync(received_at + MAX_QUEUE_AGE_MS));
        assert!(inbox.needs_resync(received_at + MAX_QUEUE_AGE_MS + 1));
        drop(inbox);
        assert!(send(WsEvent::Opened).is_break());
    }
}
