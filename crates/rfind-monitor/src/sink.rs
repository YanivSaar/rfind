//! Event sink — decouples kernel event reading from SQLite writes.
//!
//! Monitor threads must drain the kernel queue promptly or it overflows.
//! They used to insert each event directly, blocking on the shared DB
//! mutex whenever a scanner batch flush or reconcile held it. Now they
//! push onto an unbounded channel and a writer thread journals events in
//! batched transactions.

use rfind_core::db::{insert_pending_event, pending_event_count, EventType};
use rusqlite::Connection;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{sync_channel, Receiver, SyncSender},
        Arc, Mutex,
    },
    thread,
    time::SystemTime,
};
use tracing::{error, warn};

const MAX_BATCH: usize = 5000;
/// Backpressure: cap at ~2× MAX_BATCH to keep bursts in RAM while still
/// draining the kernel queue promptly. On overflow we drop the event and
/// set a flag the daemon turns into a rescan, mirroring FAN_Q_OVERFLOW.
const SINK_CAPACITY: usize = MAX_BATCH * 2;
/// Hard cap on `pending_events` rows. Rapid rename churn can beat the
/// reconciler (1000-per-5s) and grow the journal without bound,
/// eventually filling /var. On hit we set overflow (a full rescan will
/// subsume the lost events), truncate the journal, and keep draining.
const PENDING_ROW_CAP: i64 = 1_000_000;

struct RawEvent {
    et: EventType,
    path: String,
    new_path: Option<String>,
    is_dir: bool,
    ts: i64,
}

/// Cloneable handle for submitting events. The writer thread exits once
/// every handle is dropped and the queue is drained.
#[derive(Clone)]
pub struct EventSink {
    tx: SyncSender<RawEvent>,
    overflow: Arc<AtomicBool>,
}

impl EventSink {
    pub fn send(&self, et: EventType, path: String, new_path: Option<String>, is_dir: bool) {
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let ev = RawEvent {
            et,
            path,
            new_path,
            is_dir,
            ts,
        };
        if let Err(std::sync::mpsc::TrySendError::Full(ev)) = self.tx.try_send(ev) {
            if !self.overflow.swap(true, Ordering::Relaxed) {
                warn!(
                    "event sink full ({} queued); dropping events and will rescan. First dropped: {}",
                    SINK_CAPACITY, ev.path
                );
            }
        }
    }

    /// True (once) if the sink dropped any event since the last call.
    pub fn take_overflow(&self) -> bool {
        self.overflow.swap(false, Ordering::Relaxed)
    }
}

pub fn spawn_writer(db: Arc<Mutex<Connection>>) -> (EventSink, thread::JoinHandle<()>) {
    let (tx, rx) = sync_channel(SINK_CAPACITY);
    let overflow = Arc::new(AtomicBool::new(false));
    let overflow_for_writer = Arc::clone(&overflow);
    let handle = thread::spawn(move || writer_loop(db, rx, overflow_for_writer));
    (EventSink { tx, overflow }, handle)
}

/// Lock the shared connection, ignoring poisoning. The scanner's panic
/// recovery can leave the mutex poisoned even though the DB itself is
/// fine; crashing the writer thread on top of that would silently stop
/// draining events.
fn lock_db(db: &Mutex<Connection>) -> std::sync::MutexGuard<'_, Connection> {
    db.lock().unwrap_or_else(|p| p.into_inner())
}

fn writer_loop(db: Arc<Mutex<Connection>>, rx: Receiver<RawEvent>, overflow: Arc<AtomicBool>) {
    let mut batch = Vec::with_capacity(MAX_BATCH);
    while let Ok(first) = rx.recv() {
        batch.push(first);
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(ev) => batch.push(ev),
                Err(_) => break,
            }
        }
        let c = lock_db(&db);
        // Cap the journal size: sustained rename churn can beat the
        // reconciler and grow pending_events without bound. On hit, set
        // overflow (a full rescan subsumes the discarded events) and
        // truncate what's there so we don't fill /var.
        if let Ok(count) = pending_event_count(&c) {
            if count >= PENDING_ROW_CAP {
                if !overflow.swap(true, Ordering::Relaxed) {
                    warn!(
                        "pending_events hit the {} row cap; truncating and scheduling a rescan",
                        PENDING_ROW_CAP
                    );
                }
                if let Err(e) = c.execute("DELETE FROM pending_events", []) {
                    error!("truncating pending_events: {e}");
                }
                batch.clear();
                continue;
            }
        }
        let tx = match c.unchecked_transaction() {
            Ok(t) => t,
            Err(e) => {
                error!("event journal tx: {e}");
                batch.clear();
                continue;
            }
        };
        for ev in batch.drain(..) {
            if let Err(e) = insert_pending_event(
                &tx,
                &ev.et,
                &ev.path,
                ev.new_path.as_deref(),
                ev.is_dir,
                ev.ts,
            ) {
                error!("journal event {}: {e}", ev.path);
            }
        }
        if let Err(e) = tx.commit() {
            error!("event journal commit: {e}");
        }
    }
}
