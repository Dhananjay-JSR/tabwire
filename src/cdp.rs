//! Chrome DevTools Protocol over the browser-level WebSocket.
//!
//! `call` sends a command and blocks until its reply. A reader thread routes replies by id and hands
//! every event, in arrival order, to the channel returned by `connect`.

use crate::ws;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How long a single DevTools command may take before the call gives up.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Cdp {
    writer: Arc<ws::Writer>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<Value>>>,
    open: AtomicBool,
}

/// What the reader thread delivers: an event, or the end of the connection.
pub enum Incoming {
    Event(Value),
    Closed,
}

impl Cdp {
    /// Sends `method` (to a page when `session` is set) and returns its `result`.
    pub fn call(
        &self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        if !self.open.load(Ordering::Acquire) {
            return Err("the browser connection is closed".into());
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.lock_pending().insert(id, tx);
        let mut msg = json!({ "id": id, "method": method, "params": params });
        if let Some(s) = session {
            msg["sessionId"] = Value::from(s);
        }
        if let Err(e) = self.writer.send_text(&msg.to_string()) {
            self.lock_pending().remove(&id);
            return Err(format!("{method}: {e}"));
        }
        let reply = rx.recv_timeout(COMMAND_TIMEOUT).map_err(|_| {
            self.lock_pending().remove(&id);
            format!("{method}: no reply from the browser")
        })?;
        match reply.get("error") {
            Some(err) => Err(format!(
                "{method}: {}",
                err.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("failed")
            )),
            None => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
        }
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Sender<Value>>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Connects to the browser's DevTools socket and starts the reader thread.
pub fn connect(url: &str) -> std::io::Result<(Arc<Cdp>, Receiver<Incoming>)> {
    let (mut reader, writer) = ws::connect(url)?;
    let cdp = Arc::new(Cdp {
        writer,
        next_id: AtomicU64::new(1),
        pending: Mutex::new(HashMap::new()),
        open: AtomicBool::new(true),
    });
    let (events_tx, events_rx) = mpsc::channel();
    let shared = cdp.clone();
    thread::Builder::new()
        .name("cdp-reader".into())
        .spawn(move || {
            while let Ok(Some(text)) = reader.next_text() {
                // A malformed message is skipped; one bad frame must not take the bridge down.
                let Ok(msg) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                match msg.get("id").and_then(Value::as_u64) {
                    Some(id) => {
                        if let Some(tx) = shared.lock_pending().remove(&id) {
                            let _ = tx.send(msg);
                        }
                    }
                    None => {
                        if events_tx.send(Incoming::Event(msg)).is_err() {
                            break;
                        }
                    }
                }
            }
            shared.open.store(false, Ordering::Release);
            // Wake every waiting call; dropping the senders makes their recv fail at once.
            shared.lock_pending().clear();
            let _ = events_tx.send(Incoming::Closed);
        })?;
    Ok((cdp, events_rx))
}
