use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};

/// Serializable terminal→host events (mirrored to Dart by FRB).
#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    PtyWrite(Vec<u8>),
    Title(String),
    ResetTitle,
    Bell,
    ClipboardStore(String),
}

/// Shared, thread-safe event queue owned by the engine and filled by the proxy.
pub type EventQueue = Arc<Mutex<Vec<EngineEvent>>>;

/// Bridges alacritty's `EventListener` to the engine-owned queue. Testable:
/// construct with a shared queue, advance the engine, then read the queue.
#[derive(Clone)]
pub struct EventProxy {
    queue: EventQueue,
}

impl EventProxy {
    pub fn new(queue: EventQueue) -> Self {
        Self { queue }
    }
    fn emit(&self, e: EngineEvent) {
        self.queue.lock().unwrap().push(e);
    }
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(s) => self.emit(EngineEvent::PtyWrite(s.into_bytes())),
            Event::Title(s) => self.emit(EngineEvent::Title(s)),
            Event::ResetTitle => self.emit(EngineEvent::ResetTitle),
            Event::Bell => self.emit(EngineEvent::Bell),
            Event::ClipboardStore(_, s) => self.emit(EngineEvent::ClipboardStore(s)),
            // App reads clipboard (OSC52 paste): answer empty for Plan 2A.
            Event::ClipboardLoad(_, format) => {
                self.emit(EngineEvent::PtyWrite(format("").into_bytes()))
            }
            // ColorRequest / TextAreaSizeRequest need engine state → deferred.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collector() -> (EventProxy, EventQueue) {
        let store: EventQueue = Arc::new(Mutex::new(Vec::new()));
        (EventProxy::new(store.clone()), store)
    }

    #[test]
    fn maps_pty_write() {
        let (proxy, store) = collector();
        proxy.send_event(Event::PtyWrite("\x1b[1;3R".to_string()));
        assert_eq!(
            store.lock().unwrap()[0],
            EngineEvent::PtyWrite(b"\x1b[1;3R".to_vec())
        );
    }

    #[test]
    fn maps_title() {
        let (proxy, store) = collector();
        proxy.send_event(Event::Title("hello".to_string()));
        assert_eq!(store.lock().unwrap()[0], EngineEvent::Title("hello".into()));
    }
}
