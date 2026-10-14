//! Output in human-readable or JSON form.
//!
//! Results stream as one line each: a path in text mode, a JSON object
//! (JSON Lines) with `--json`. Summaries such as "12 result(s)" go to
//! stderr in text mode so stdout stays pipeable, and are omitted with
//! `--json`.

use rfind_core::{
    db::{HealthInfo, Hit, IndexStats, RecentEntry, Usage},
    DaemonPhase, ScanProgress,
};
use serde_json::{json, Value};
use std::io::{self, BufWriter, Write};

pub struct Out {
    pub json: bool,
    /// Text mode: show size and modification time with each path.
    pub long: bool,
    stdout: BufWriter<io::Stdout>,
}

fn kind(is_dir: bool) -> &'static str {
    if is_dir {
        "dir"
    } else {
        "file"
    }
}

fn local_time(sec: i64) -> String {
    chrono::DateTime::from_timestamp(sec, 0)
        .unwrap_or_default()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

pub fn format_duration(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s if s < 86400 => format!("{}h {}m", s / 3600, (s % 3600) / 60),
        s => format!("{}d {}h", s / 86400, (s % 86400) / 3600),
    }
}

/// Compact human size: 512, 4.0K, 1.5M, 2.1G.
pub fn human(b: u64) -> String {
    // UNITS[u] is the label after `u` divisions by 1024, so UNITS[0] is K.
    const UNITS: [&str; 6] = ["K", "M", "G", "T", "P", "E"];
    if b < 1024 {
        return b.to_string();
    }
    let mut v = b as f64 / 1024.0;
    let mut u = 0; // "K"
                   // Keep scaling into the next label while there is one.
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1}{}", UNITS[u])
}

/// Live-monitored means fanotify or inotify is watching for changes;
/// the index is current no matter how long ago the last full scan was.
fn is_live(h: &HealthInfo) -> bool {
    matches!(h.monitoring_mode.as_str(), "fanotify" | "inotify")
}

/// Human label for the index freshness (shared by text + JSON output).
fn index_status(h: &HealthInfo, pid: Option<i32>, live: bool) -> String {
    if pid.is_none() {
        return "DAEMON STOPPED".into();
    }
    if h.pending_events > 0 {
        return format!("UPDATING ({} events pending)", h.pending_events);
    }
    let now = chrono::Utc::now().timestamp();
    match h.last_scan_completed {
        None => "NO INDEX".into(),
        Some(_) if live => "LIVE".into(),
        Some(t) if now - t < 3600 => "FRESH".into(),
        Some(t) if now - t < 86400 => "RECENT".into(),
        _ => "STALE".into(),
    }
}

pub fn format_bytes(b: u64) -> String {
    const PB: u64 = 1 << 50;
    const EB: u64 = 1 << 60;
    match b {
        b if b < 1024 => format!("{b} B"),
        b if b < 1 << 20 => format!("{:.1} KB", b as f64 / 1024.0),
        b if b < 1 << 30 => format!("{:.1} MB", b as f64 / (1 << 20) as f64),
        b if b < 1u64 << 40 => format!("{:.2} GB", b as f64 / (1 << 30) as f64),
        b if b < 1u64 << 50 => format!("{:.2} TB", b as f64 / (1u64 << 40) as f64),
        b if b < EB => format!("{:.2} PB", b as f64 / PB as f64),
        b => format!("{:.2} EB", b as f64 / EB as f64),
    }
}

impl Out {
    pub fn new(json: bool) -> Self {
        Self {
            json,
            long: false,
            stdout: BufWriter::new(io::stdout()),
        }
    }

    /// Write one line; false once stdout is closed (e.g. piped into head).
    ///
    /// Flushes immediately so a `BufWriter`-buffered broken-pipe error
    /// surfaces as soon as the reader goes away — otherwise `rfind … | head`
    /// would keep running SQL for every row until the buffer finally flushed.
    fn line(&mut self, s: &str) -> bool {
        writeln!(self.stdout, "{s}").is_ok() && self.stdout.flush().is_ok()
    }

    fn object(&mut self, v: &Value) {
        let _ = writeln!(self.stdout, "{v}");
        let _ = self.stdout.flush();
    }

    pub fn hit(&mut self, h: &Hit) -> bool {
        if self.json {
            let v = json!({ "path": h.path, "type": kind(h.is_dir), "size": h.size, "mtime": h.mtime_sec });
            self.line(&v.to_string())
        } else if self.long {
            let size = if h.is_dir {
                "-".to_string()
            } else {
                human(h.size)
            };
            self.line(&format!(
                "{size:>9}  {}  {}",
                local_time(h.mtime_sec),
                h.path
            ))
        } else {
            self.line(h.path)
        }
    }

    /// `rfind size`: a tree's total and its largest children.
    pub fn usage(&mut self, total: &Usage, children: &[Usage]) {
        let row = |u: &Usage| json!({ "name": u.name, "bytes": u.bytes, "files": u.files, "dirs": u.dirs });
        let data = json!({ "total": row(total), "children": children.iter().map(row).collect::<Vec<_>>() });
        let mut text = String::new();
        for c in children {
            let pct = if total.bytes > 0 {
                100.0 * c.bytes as f64 / total.bytes as f64
            } else {
                0.0
            };
            text += &format!(
                "{:>9}  {pct:>5.1}%  {:>9} files  {}\n",
                human(c.bytes),
                c.files,
                c.name
            );
        }
        if !children.is_empty() {
            text += "\n";
        }
        text += &format!(
            "{:>9}  total  {} files, {} directories  {}\n",
            human(total.bytes),
            total.files,
            total.dirs,
            total.name
        );
        self.text_or_json(&text, &data);
    }

    pub fn recent(&mut self, e: &RecentEntry) -> bool {
        if self.json {
            let v = json!({
                "path": e.full_path, "type": kind(e.is_dir), "size": e.size,
                "mtime": e.mtime_sec, "modified": local_time(e.mtime_sec),
            });
            self.line(&v.to_string())
        } else if self.long {
            let size = if e.is_dir {
                "-".to_string()
            } else {
                human(e.size)
            };
            self.line(&format!(
                "{size:>9}  {}  {}",
                local_time(e.mtime_sec),
                e.full_path
            ))
        } else {
            self.line(&format!("{}  {}", local_time(e.mtime_sec), e.full_path))
        }
    }

    /// End of a result stream.
    pub fn count(&mut self, n: u64) {
        let _ = self.stdout.flush();
        if !self.json {
            eprintln!("\n{n} result(s)");
        }
    }

    /// A one-line confirmation, or an object with `--json`.
    pub fn message(&mut self, text: &str, data: Value) {
        if self.json {
            self.object(&data);
        } else {
            self.line(text);
            let _ = self.stdout.flush();
        }
    }

    /// Already-formatted text, or `data` with `--json`.
    pub fn text_or_json(&mut self, text: &str, data: &Value) {
        if self.json {
            self.object(data);
        } else {
            let _ = write!(self.stdout, "{text}");
            let _ = self.stdout.flush();
        }
    }

    /// Unified status: daemon state (from the progress file) plus index
    /// health (from the DB) plus any background scan in flight.
    ///
    /// Any of the inputs may be missing — the daemon may be down, the DB
    /// may be unreadable, no pidfile. Each section is rendered when its
    /// data is there, so a partial view still tells the user what the
    /// daemon is actually doing right now.
    pub fn status(
        &mut self,
        progress: Option<&ScanProgress>,
        health: Option<&HealthInfo>,
        pid: Option<i32>,
        now: i64,
    ) {
        if self.json {
            let v = serde_json::json!({
                "daemon_pid": pid,
                "scan": progress.map(|p| serde_json::to_value(p).unwrap_or_default()),
                "index": health.map(|h| {
                    let age = h.last_scan_completed.map(|t| now - t);
                    serde_json::json!({
                        "status": index_status(h, pid, is_live(h)),
                        "entries": h.entry_count,
                        "directories": h.dir_count,
                        "files": h.file_count,
                        "monitoring": h.monitoring_mode,
                        "events_pending": h.pending_events,
                        "db_size_bytes": h.db_size_bytes,
                        "indexed_bytes": h.total_bytes,
                        "last_full_scan": h.last_scan_completed,
                        "last_scan_age_secs": age,
                    })
                }),
                "background_scan": progress.and_then(|p| p.background_scan.as_ref().map(|b| {
                    serde_json::json!({
                        "path": b.path,
                        "entries": b.entries,
                        "started_at": b.started_at,
                        "elapsed_secs": now.saturating_sub(b.started_at as i64),
                    })
                })),
            });
            self.object(&v);
            return;
        }

        let mut text = String::new();
        // Daemon pid line is always first — it's the "is anyone home" line.
        text += &format!(
            "Daemon         : {}\n",
            pid.map(|p| format!("running (pid {p})"))
                .unwrap_or_else(|| "STOPPED".into())
        );
        if let Some(p) = progress {
            let phase = match &p.phase {
                DaemonPhase::Scanning => "SCANNING",
                DaemonPhase::Reconciling => "RECONCILING",
                DaemonPhase::Ready => "READY",
                DaemonPhase::Error(_) => "ERROR",
            };
            text += &format!("Phase          : {phase}\n");
            if p.phase != DaemonPhase::Ready {
                text += &format!(
                    "Dirs done      : {}\n\
                     Dirs pending   : {}\n\
                     Rate           : {:.0} entries/s\n\
                     Errors         : {}\n",
                    p.dirs_completed, p.dirs_pending, p.entries_per_sec, p.errors
                );
            }
        }
        if let Some(h) = health {
            let live = is_live(h);
            let status = index_status(h, pid, live);
            let age = h
                .last_scan_completed
                .map(|t| format!("{} ago", format_duration(now - t)))
                .unwrap_or_else(|| "never".into());
            text += &format!(
                "Status         : {status}\n\
                 Last full scan : {age}\n\
                 Entries        : {}\n\
                 Directories    : {}\n\
                 Files          : {} ({})\n\
                 Monitoring     : {}\n\
                 Events pending : {}\n\
                 DB size        : {}\n",
                h.entry_count,
                h.dir_count,
                h.file_count,
                format_bytes(h.total_bytes),
                h.monitoring_mode,
                h.pending_events,
                format_bytes(h.db_size_bytes),
            );
        } else if let Some(p) = progress {
            // No DB readable yet, but the progress file is: show what we have.
            text += &format!(
                "Monitoring     : {}\n\
                 Status         : {}\n",
                p.monitoring_mode, p.status_line
            );
        }
        if let Some(bg) = progress.and_then(|p| p.background_scan.as_ref()) {
            let elapsed = now.saturating_sub(bg.started_at as i64).max(0);
            text += &format!(
                "Background scan: {} (running {}, {} entries)\n",
                bg.path,
                format_duration(elapsed),
                bg.entries,
            );
        }
        self.text_or_json(&text, &Value::Null);
    }

    pub fn stats(&mut self, s: &IndexStats) {
        let data = json!({
            "entries": s.entry_count, "directories": s.dir_count, "files": s.file_count,
            "db_size_bytes": s.db_size_bytes, "index_age_secs": s.index_age_secs,
            "top_extensions": s.top_extensions.iter()
                .map(|e| json!({ "ext": e.ext, "count": e.count })).collect::<Vec<_>>(),
            "largest_dirs": s.largest_dirs.iter()
                .map(|d| json!({ "path": d.path, "entries": d.count })).collect::<Vec<_>>(),
        });
        let mut text = format!(
            "Total entries  : {}\nDirectories    : {}\nFiles          : {}\nDB size        : {}\n",
            s.entry_count,
            s.dir_count,
            s.file_count,
            format_bytes(s.db_size_bytes)
        );
        if let Some(age) = s.index_age_secs {
            text += &format!("Last full scan : {} ago\n", format_duration(age));
        }
        if !s.top_extensions.is_empty() {
            text += "\nTop file types:\n";
            for e in &s.top_extensions {
                text += &format!("  .{:<12} {}\n", e.ext, e.count);
            }
        }
        if !s.largest_dirs.is_empty() {
            text += "\nLargest directories:\n";
            for d in &s.largest_dirs {
                text += &format!("  {:>8} entries  {}\n", d.count, d.path);
            }
        }
        self.text_or_json(&text, &data);
    }
}
