//! Append-only diagnostic log shared with the GUI app at
//! `%APPDATA%/Penflow/debug.log`. Best-effort — failures here are silently
//! dropped to keep instrumentation from cascading into the very paths
//! it's tracing.

use std::io::Write;

/// Append `msg` to the shared debug log, prefixed with epoch seconds.
///
/// Cheap enough to call from hot paths sparingly (per-state-transition,
/// per-handshake) but should NOT be called per pen sample — append +
/// fsync on every event would dominate the input-latency budget.
pub fn log(msg: &str) {
    let Some(base) = std::env::var_os("APPDATA").map(std::path::PathBuf::from) else {
        return;
    };
    let dir = base.join("Penflow");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = dir.join("debug.log");
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(f, "[{now}] {msg}");
}
