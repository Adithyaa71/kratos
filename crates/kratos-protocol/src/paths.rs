//! Where kratos keeps its state. Standard library only: the pipe name
//! gains its per-user SID suffix in `kratos-ipc`, which is the crate allowed to
//! touch Windows.

use std::path::PathBuf;

/// `%LOCALAPPDATA%\kratos`, created on demand.
///
/// LocalAppData rather than Roaming on purpose: the journal can reach tens of
/// megabytes and the DPAPI-sealed API key is machine-and-user bound, so
/// neither belongs in a roaming profile that syncs between machines.
pub fn state_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("TEMP").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("kratos")
}

pub fn config_path() -> PathBuf {
    if let Some(p) = std::env::var_os("KRATOS_CONFIG") {
        return PathBuf::from(p);
    }
    state_dir().join("config.json")
}

pub fn journal_path() -> PathBuf {
    if let Some(p) = std::env::var_os("KRATOS_JOURNAL") {
        return PathBuf::from(p);
    }
    state_dir().join("actions.jsonl")
}

/// Ensure the state directory exists. Idempotent.
pub fn ensure_state_dir() -> std::io::Result<PathBuf> {
    let d = state_dir();
    std::fs::create_dir_all(&d)?;
    Ok(d)
}
