//! Mode selector: index vs. live walk.
//!
//! Decision order:
//! 1. `--live` / `--no-index` → `Walk`.
//! 2. `[search] mode = "index"` in config → `Index` (hard error if non-root).
//! 3. `[search] mode = "walk"` in config → `Walk`.
//! 4. `auto` (default):
//!    - Any walk-only filter (atime, ctime, type l/any, -L, -H) → `Walk`.
//!    - Non-root caller → `Walk` (silent).
//!    - DB missing / unreadable / daemon not ready → `Walk` (warning).
//!    - Otherwise → `Index`.
//!
//! The selector does not open the index itself — it only decides which
//! branch of `cmd_search` to take. The index branch calls `open_db()`,
//! which enforces root; the walk branch never touches the DB.

use rfind_config::{Config, SearchModePref};
use std::path::Path;

/// Which backend answers the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Index,
    Walk,
}

/// How walk mode treats symbolic links. (Index mode ignores this.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymlinkMode {
    /// `-P`: never follow (default). Matches the indexer's semantics.
    P,
    /// `-H`: follow only on command-line roots.
    H,
    /// `-L`: follow everywhere; a visited set breaks cycles.
    L,
}

/// Inputs the selector needs beyond `SearchArgs` to pick a mode. Kept as a
/// struct so the test suite can override probes without stubbing globals.
pub struct Env<'a> {
    pub caller_is_root: bool,
    pub cfg: &'a Config,
    /// Whether the index DB file exists and is readable by this process.
    /// `cmd_search` fills this in by probing the configured path.
    pub index_available: bool,
    /// Whether the daemon's progress file reports Ready. False when the
    /// progress file is missing or not Ready yet.
    pub daemon_ready: bool,
}

/// What the user passed on the command line that the selector cares about.
/// Mirrors a tiny subset of `SearchArgs` so this module can be unit-tested
/// without pulling the whole clap tree.
pub struct Request {
    pub live_flag: bool,
    pub walk_only_filter: bool,
    pub symlinks: SymlinkMode,
}

/// Pick a mode. Returns the mode and an optional stderr warning the caller
/// should print once (never under `--json`).
pub fn choose_mode<'a>(req: &Request, env: &Env<'a>) -> Result<(Mode, Option<String>), String> {
    // 1. Explicit --live wins.
    if req.live_flag {
        return Ok((Mode::Walk, None));
    }

    // 2 & 3. Hard preference from config.
    match env.cfg.search.mode {
        SearchModePref::Index => {
            if !env.caller_is_root {
                return Err(
                    "search.mode = \"index\" is set, but rfind needs root to read the \
                     index. Rerun with sudo, change the config, or pass --live."
                        .into(),
                );
            }
            // Index-forced mode still errors cleanly if the DB is missing;
            // the caller handles that when open_db fails.
            return Ok((Mode::Index, None));
        }
        SearchModePref::Walk => return Ok((Mode::Walk, None)),
        SearchModePref::Auto => {}
    }

    // 4. Auto. Walk-only filters win first.
    if req.walk_only_filter || matches!(req.symlinks, SymlinkMode::H | SymlinkMode::L) {
        return Ok((Mode::Walk, None));
    }
    if !env.caller_is_root {
        return Ok((Mode::Walk, None));
    }
    if !env.index_available {
        return Ok((
            Mode::Walk,
            Some("index unavailable; falling back to live walk".into()),
        ));
    }
    if !env.daemon_ready {
        return Ok((
            Mode::Walk,
            Some("daemon not ready; falling back to live walk".into()),
        ));
    }
    Ok((Mode::Index, None))
}

/// Does the configured DB file exist and is it a regular file we can see?
/// We don't open it here — just check the path. open_db does the real
/// permission check with `require_root`.
pub fn index_available(cfg: &Config) -> bool {
    index_available_at(&cfg.index.db_path)
}

fn index_available_at(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// Does the invoking user have their own per-user index built by
/// `rfindd --user`?  If so, searches run against that one (readable
/// without root) in auto mode, before falling through to the system
/// index (root) or live walk.
#[allow(dead_code)] // Consumed by the auto-mode selector in a follow-up
                   // CL that wires the per-user index path through
                   // open_db(); mode.rs lands first.
pub fn user_index_available() -> bool {
    let p = rfind_config::user_paths::index_path();
    std::fs::metadata(&p)
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// Path to the user's per-user index, when it exists.
#[allow(dead_code)]
pub fn user_index_path() -> std::path::PathBuf {
    rfind_config::user_paths::index_path()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rfind_config::Config;

    fn env<'a>(cfg: &'a Config, root: bool, idx: bool, ready: bool) -> Env<'a> {
        Env {
            caller_is_root: root,
            cfg,
            index_available: idx,
            daemon_ready: ready,
        }
    }

    fn req(live: bool, walk_only: bool, s: SymlinkMode) -> Request {
        Request {
            live_flag: live,
            walk_only_filter: walk_only,
            symlinks: s,
        }
    }

    #[test]
    fn live_flag_forces_walk() {
        let cfg = Config::default();
        let (m, _) = choose_mode(
            &req(true, false, SymlinkMode::P),
            &env(&cfg, true, true, true),
        )
        .unwrap();
        assert_eq!(m, Mode::Walk);
    }

    #[test]
    fn non_root_auto_falls_to_walk() {
        let cfg = Config::default();
        let (m, _) = choose_mode(
            &req(false, false, SymlinkMode::P),
            &env(&cfg, false, true, true),
        )
        .unwrap();
        assert_eq!(m, Mode::Walk);
    }

    #[test]
    fn root_auto_prefers_index() {
        let cfg = Config::default();
        let (m, w) = choose_mode(
            &req(false, false, SymlinkMode::P),
            &env(&cfg, true, true, true),
        )
        .unwrap();
        assert_eq!(m, Mode::Index);
        assert!(w.is_none());
    }

    #[test]
    fn walk_only_filter_forces_walk_even_as_root() {
        let cfg = Config::default();
        let (m, _) = choose_mode(
            &req(false, true, SymlinkMode::P),
            &env(&cfg, true, true, true),
        )
        .unwrap();
        assert_eq!(m, Mode::Walk);
    }

    #[test]
    fn big_l_symlink_forces_walk() {
        let cfg = Config::default();
        let (m, _) = choose_mode(
            &req(false, false, SymlinkMode::L),
            &env(&cfg, true, true, true),
        )
        .unwrap();
        assert_eq!(m, Mode::Walk);
    }

    #[test]
    fn index_pref_fails_for_non_root() {
        let mut cfg = Config::default();
        cfg.search.mode = SearchModePref::Index;
        let err = choose_mode(
            &req(false, false, SymlinkMode::P),
            &env(&cfg, false, true, true),
        );
        assert!(err.is_err());
    }

    #[test]
    fn walk_pref_always_walks() {
        let mut cfg = Config::default();
        cfg.search.mode = SearchModePref::Walk;
        let (m, _) = choose_mode(
            &req(false, false, SymlinkMode::P),
            &env(&cfg, true, true, true),
        )
        .unwrap();
        assert_eq!(m, Mode::Walk);
    }

    #[test]
    fn auto_warns_when_db_missing() {
        let cfg = Config::default();
        let (m, w) = choose_mode(
            &req(false, false, SymlinkMode::P),
            &env(&cfg, true, false, true),
        )
        .unwrap();
        assert_eq!(m, Mode::Walk);
        assert!(w.unwrap().contains("index unavailable"));
    }

    #[test]
    fn auto_warns_when_daemon_not_ready() {
        let cfg = Config::default();
        let (m, w) = choose_mode(
            &req(false, false, SymlinkMode::P),
            &env(&cfg, true, true, false),
        )
        .unwrap();
        assert_eq!(m, Mode::Walk);
        assert!(w.unwrap().contains("daemon not ready"));
    }
}
