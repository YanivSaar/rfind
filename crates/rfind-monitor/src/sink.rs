//! Event sink — decouples kernel event reading from SQLite writes.
//!
//! Monitor threads must drain the kernel queue promptly or it overflows.
//! They used to insert each event directly, blocking on the shared DB
//! mutex whenever a scanner batch flush or reconcile held it. Now they
//! push onto an unbounded channel and a writer thread journals events in
//! batched transactions.

use rfind_core::db::{insert_pending_event, EventType};
use rusqlite::Connection;
use std::{
    sync::{
        mpsc::{channel, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::SystemTime,
};
use tracing::error;

const MAX_BATCH: usize = 5000;

struct RawEvent {
    et:       EventType,
    path:     String,
    new_path: Option<String>,
    is_dir:   bool,
    ts:       i64,
}

/// Cloneable handle for submitting events. The writer thread exits once
/// every handle is dropped and the queue is drained.
#[derive(Clone)]
pub struct EventSink {
    tx: Sender<RawEvent>,
}

impl EventSink {
    pub fn send(&self, et: EventType, path: String, new_path: Option<String>, is_dir: bool) {
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let _ = self.tx.send(RawEvent { et, path, new_path, is_dir, ts });
    }
}

pub fn spawn_writer(db: Arc<Mutex<Connection>>) -> (EventSink, thread::JoinHandle<()>) {
    let (tx, rx) = channel();
    let handle = thread::spawn(move || writer_loop(db, rx));
    (EventSink { tx }, handle)
}

fn writer_loop(db: Arc<Mutex<Connection>>, rx: Receiver<RawEvent>) {
    let mut batch = Vec::with_capacity(MAX_BATCH);
    while let Ok(first) = rx.recv() {
        batch.push(first);
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(ev) => batch.push(ev),
                Err(_) => break,
            }
        }
        let c = db.lock().unwrap();
        let tx = match c.unchecked_transaction() {
            Ok(t) => t,
            Err(e) => { error!("event journal tx: {e}"); batch.clear(); continue; }
        };
        for ev in batch.drain(..) {
            if let Err(e) = insert_pending_event(
                &tx, &ev.et, &ev.path, ev.new_path.as_deref(), ev.is_dir, ev.ts,
            ) {
                error!("journal event {}: {e}", ev.path);
            }
        }
        if let Err(e) = tx.commit() { error!("event journal commit: {e}"); }
    }
}
