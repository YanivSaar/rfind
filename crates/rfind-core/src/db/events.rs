//! Event journal: pending-event rows backing the fanotify/inotify
//! applier. Writers insert on event sink, the daemon's event-replay
//! loop reads and marks applied, and `prune_applied_events` is called
//! after bulk apply to keep the table small.

use rusqlite::{params, Connection, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum EventType {
    Create,
    Delete,
    Rename,
    Modify,
}

impl EventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventType::Create => "CREATE",
            EventType::Delete => "DELETE",
            EventType::Rename => "RENAME",
            EventType::Modify => "MODIFY",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "CREATE" => Some(EventType::Create),
            "DELETE" => Some(EventType::Delete),
            "RENAME" => Some(EventType::Rename),
            "MODIFY" => Some(EventType::Modify),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PendingEvent {
    pub id: i64,
    pub event_type: EventType,
    pub path: String,
    pub new_path: Option<String>,
    pub is_dir: bool,
    pub timestamp: i64,
}

pub fn insert_pending_event(
    conn: &Connection,
    event_type: &EventType,
    path: &str,
    new_path: Option<&str>,
    is_dir: bool,
    timestamp: i64,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO pending_events
             (event_type,path,new_path,is_dir,timestamp)
         VALUES (?1,?2,?3,?4,?5)",
        params![
            event_type.as_str(),
            path,
            new_path,
            is_dir as i32,
            timestamp
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Load up to `limit` unapplied events, oldest first.
pub fn load_unapplied_events(conn: &Connection, limit: usize) -> Result<Vec<PendingEvent>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id,event_type,path,new_path,is_dir,timestamp
         FROM pending_events WHERE applied=0 ORDER BY id ASC LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |r| {
        let et: String = r.get(1)?;
        Ok(PendingEvent {
            id: r.get(0)?,
            event_type: EventType::parse(&et).unwrap_or(EventType::Modify),
            path: r.get(2)?,
            new_path: r.get(3)?,
            is_dir: r.get::<_, i32>(4)? != 0,
            timestamp: r.get(5)?,
        })
    })?;
    rows.collect()
}

pub fn mark_event_applied(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE pending_events SET applied=1 WHERE id=?1",
        params![id],
    )?;
    Ok(())
}

pub fn pending_event_count(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM pending_events WHERE applied=0",
        [],
        |r| r.get(0),
    )
}

pub fn prune_applied_events(conn: &Connection) -> Result<usize> {
    conn.execute("DELETE FROM pending_events WHERE applied=1", [])
}
