//! Delta §7.7: cache `tools/list` so a startup never spawns a server
//! just to enumerate its tools.
//!
//! # Why
//!
//! Spawning an MCP server is the expensive part of a cold start: a
//! Node server pays a `require()` graph, a Python one pays an import
//! graph. The tool list a server advertises changes only when the
//! server is upgraded, so listing on every start pays the spawn cost
//! for an answer that has been stable for months.
//!
//! # Key
//!
//! SHA-256 over `{command, args, env}` — byte-stable, `env` sorted by
//! `BTreeMap`, `args` in order (it is part of the spec). A change to
//! any of the three produces a different key, so a re-configured
//! server starts cold (correct) and an untouched one hits (the point).
//!
//! # TTL
//!
//! [`DEFAULT_TTL`] is 30 days. A stale entry is deleted on read; a
//! fresh one is deleted only when [`clear`] is called, which a future
//! `kod mcp refresh` can expose.
//!
//! # What this does NOT do
//!
//! It does **not** avoid spawning the server for a `tools/call`. The
//! list cache short-circuits the *enumeration*, but the first actual
//! invocation still spawns. Making a call lazy would need the tool
//! registry to hold a factory instead of a client handle — a larger
//! change to `mcp_adapters` and out of scope for this delta.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::McpToolDef;

/// How long a cached tool list stays valid. 30 days: long enough that
/// a daily driver sees the cache hit every day, short enough that a
/// forgotten stale entry cleans itself up within a month.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// On-disk cache entry.
#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    /// Unix nanoseconds when the entry was written. Nanosecond
    /// granularity lets a test with a sub-second TTL observe the
    /// staleness that a second-granular field would round away.
    cached_at_unix_nanos: u128,
    tools: Vec<McpToolDef>,
}

/// A stable cache key for one server spec. Same bytes in → same hex
/// out; a different command, argument order, or env value → a
/// different key.
pub fn cache_key(command: &str, args: &[String], env: &BTreeMap<String, String>) -> String {
    #[derive(Serialize)]
    struct Canonical<'a> {
        command: &'a str,
        args: &'a [String],
        env: &'a BTreeMap<String, String>,
    }
    // `serde_json::to_string` on a struct with a `BTreeMap` field is
    // byte-stable: the map's iteration order is the key sort order.
    // The `unwrap_or_default` on failure is harmless — an empty string
    // hashes to a valid, unique key; a spec that fails to serialize
    // cannot be spawned anyway.
    let json =
        serde_json::to_string(&Canonical { command, args, env }).unwrap_or_default();
    let mut h = Sha256::new();
    h.update(json.as_bytes());
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Inline hex avoids a `hex` crate dep for a 4-line loop.
        const HEX: &[u8; 16] = b"0123456789abcdef";
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// The cache root. `None` when the platform has no cache dir — the
/// cache is then a no-op (read returns `None`, write is skipped).
pub fn cache_dir() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("kod").join("mcp"))
}

/// Read a cached tool list, if one exists and is fresh.
///
/// A stale entry is deleted as a side effect (best-effort; a
/// permission error on delete still returns `None` for the read).
/// Any parse error also returns `None` — a corrupt entry is discarded
/// and the caller falls back to the wire.
pub fn read(key: &str, ttl: Duration) -> Option<Vec<McpToolDef>> {
    read_from(&cache_dir()?, key, ttl)
}

/// [`read`] against an explicit directory. Split out so tests do not
/// touch the process-global `XDG_CACHE_HOME`.
pub fn read_from(dir: &Path, key: &str, ttl: Duration) -> Option<Vec<McpToolDef>> {
    let path = dir.join(format!("{key}.json"));
    let data = std::fs::read_to_string(&path).ok()?;
    let entry: CacheEntry = serde_json::from_str(&data).ok()?;
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let age_nanos = now_nanos.saturating_sub(entry.cached_at_unix_nanos);
    if Duration::from_nanos(age_nanos.min(u64::MAX as u128) as u64) > ttl {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(entry.tools)
}

/// Write a tool list under `key`. Creates the cache dir on demand.
pub fn write(key: &str, tools: &[McpToolDef]) -> io::Result<()> {
    let dir = cache_dir()
        .ok_or_else(|| io::Error::other("no platform cache dir"))?;
    write_to(&dir, key, tools)
}

/// [`write`] against an explicit directory. Writes to a temp file and
/// renames, so a concurrent reader never sees a half-written entry.
pub fn write_to(dir: &Path, key: &str, tools: &[McpToolDef]) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let entry = CacheEntry {
        cached_at_unix_nanos: now_nanos,
        tools: tools.to_vec(),
    };
    let json = serde_json::to_string(&entry)
        .map_err(|e| io::Error::other(format!("serialize cache entry: {e}")))?;
    let path = dir.join(format!("{key}.json"));
    let tmp = dir.join(format!(".{key}.json.tmp"));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Delete the entry for `key`. A missing entry is not an error.
pub fn clear(key: &str) -> io::Result<()> {
    let Some(dir) = cache_dir() else {
        return Ok(());
    };
    clear_from(&dir, key)
}

/// [`clear`] against an explicit directory.
pub fn clear_from(dir: &Path, key: &str) -> io::Result<()> {
    let path = dir.join(format!("{key}.json"));
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn tool(name: &str) -> McpToolDef {
        McpToolDef {
            name: name.to_string(),
            description: Some(format!("the {name} tool")),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn same_spec_same_key() {
        let a = cache_key("npx", &["x".to_string()], &env(&[("K", "v")]));
        let b = cache_key("npx", &["x".to_string()], &env(&[("K", "v")]));
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "sha-256 hex");
    }

    #[test]
    fn env_order_does_not_change_the_key() {
        // BTreeMap sorts by key on iteration, so a caller whose map
        // was built in a different order still gets a hit.
        let a = cache_key("npx", &[], &env(&[("A", "1"), ("B", "2")]));
        let b = cache_key("npx", &[], &env(&[("B", "2"), ("A", "1")]));
        assert_eq!(a, b);
    }

    #[test]
    fn arg_order_changes_the_key() {
        let a = cache_key("npx", &["a".to_string(), "b".to_string()], &env(&[]));
        let b = cache_key("npx", &["b".to_string(), "a".to_string()], &env(&[]));
        assert_ne!(a, b);
    }

    #[test]
    fn command_change_changes_the_key() {
        let a = cache_key("npx", &[], &env(&[]));
        let b = cache_key("uvx", &[], &env(&[]));
        assert_ne!(a, b);
    }

    #[test]
    fn env_value_change_changes_the_key() {
        let a = cache_key("npx", &[], &env(&[("TOKEN", "1")]));
        let b = cache_key("npx", &[], &env(&[("TOKEN", "2")]));
        assert_ne!(a, b);
    }

    #[test]
    fn write_then_read_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let tools = vec![tool("a"), tool("b")];
        write_to(dir.path(), "k", &tools).unwrap();
        let back = read_from(dir.path(), "k", DEFAULT_TTL).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].name, "a");
        assert_eq!(back[1].name, "b");
    }

    #[test]
    fn read_missing_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_from(dir.path(), "nope", DEFAULT_TTL).is_none());
    }

    #[test]
    fn stale_entry_is_not_returned() {
        let dir = tempfile::tempdir().unwrap();
        write_to(dir.path(), "k", &[tool("a")]).unwrap();
        // The entry is stamped in nanoseconds, so a 2 ms sleep is
        // observable against a 1 ns TTL. A second-granular field
        // would round this away.
        std::thread::sleep(Duration::from_millis(2));
        assert!(
            read_from(dir.path(), "k", Duration::from_nanos(1)).is_none(),
            "an entry older than the TTL must not be returned",
        );
        // Side effect: the stale file is gone.
        let path = dir.path().join("k.json");
        assert!(!path.exists(), "a stale entry is deleted on read");
    }

    #[test]
    fn corrupt_entry_is_none_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("k.json"), b"not json").unwrap();
        assert!(read_from(dir.path(), "k", DEFAULT_TTL).is_none());
    }

    #[test]
    fn clear_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        write_to(dir.path(), "k", &[tool("a")]).unwrap();
        clear_from(dir.path(), "k").unwrap();
        // Second clear is a no-op, not an error.
        clear_from(dir.path(), "k").unwrap();
        assert!(read_from(dir.path(), "k", DEFAULT_TTL).is_none());
    }
}
