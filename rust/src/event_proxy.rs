use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::vte::ansi::Rgb;

/// A deferred OSC color-query reply. The formatter is alacritty's own reply
/// builder; we feed it the current color after parsing finishes (the proxy
/// can't read term state during `parser.advance`). Not serializable — never
/// part of `EngineEvent`.
#[derive(Clone)]
pub struct ColorReply {
    pub index: usize,
    pub formatter: Arc<dyn Fn(Rgb) -> String + Send + Sync>,
}

/// Side queue of pending color-query replies (parallel to `EventQueue`).
pub type ReplyQueue = Arc<Mutex<Vec<ColorReply>>>;

/// A deferred OSC 52 paste reply: alacritty's formatter, applied to the
/// system clipboard text the host fetches asynchronously.
#[derive(Clone)]
pub struct ClipboardReply {
    pub formatter: Arc<dyn Fn(&str) -> String + Send + Sync>,
}

pub type ClipboardReplyQueue = Arc<Mutex<Vec<ClipboardReply>>>;

/// A deferred text-area size reply (CSI 14/18 t).
#[derive(Clone)]
pub struct SizeReply {
    pub formatter: Arc<dyn Fn(WindowSize) -> String + Send + Sync>,
}

pub type SizeReplyQueue = Arc<Mutex<Vec<SizeReply>>>;

/// Serializable terminal→host events (mirrored to Dart by FRB).
#[derive(Clone, Debug, PartialEq)]
pub enum EngineEvent {
    PtyWrite(Vec<u8>),
    Title(String),
    ResetTitle,
    Bell,
    ClipboardStore(String),
    ClipboardLoad,
    /// OSC 7: current working directory (file://host/path).
    WorkingDir(String),
    /// OSC 9 / OSC 777: desktop notification (body, or "title\0body" for 777).
    Notify(String),
}

/// Shared, thread-safe event queue owned by the engine and filled by the proxy.
pub type EventQueue = Arc<Mutex<Vec<EngineEvent>>>;

/// Bridges alacritty's `EventListener` to the engine-owned queue. Testable:
/// construct with a shared queue, advance the engine, then read the queue.
#[derive(Clone)]
pub struct EventProxy {
    queue: EventQueue,
    replies: ReplyQueue,
    clipboard: ClipboardReplyQueue,
    sizes: SizeReplyQueue,
}

impl EventProxy {
    pub fn new(
        queue: EventQueue,
        replies: ReplyQueue,
        clipboard: ClipboardReplyQueue,
        sizes: SizeReplyQueue,
    ) -> Self {
        Self {
            queue,
            replies,
            clipboard,
            sizes,
        }
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
            Event::ClipboardLoad(_, format) => {
                self.clipboard
                    .lock()
                    .unwrap()
                    .push(ClipboardReply { formatter: format });
                self.emit(EngineEvent::ClipboardLoad);
            }
            Event::ColorRequest(index, formatter) => {
                self.replies.lock().unwrap().push(ColorReply { index, formatter });
            }
            Event::TextAreaSizeRequest(format) => {
                self.sizes.lock().unwrap().push(SizeReply { formatter: format });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collector() -> (EventProxy, EventQueue) {
        let store: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let replies: ReplyQueue = Arc::new(Mutex::new(Vec::new()));
        let clipboard: ClipboardReplyQueue = Arc::new(Mutex::new(Vec::new()));
        let sizes: SizeReplyQueue = Arc::new(Mutex::new(Vec::new()));
        (
            EventProxy::new(store.clone(), replies, clipboard, sizes),
            store,
        )
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
