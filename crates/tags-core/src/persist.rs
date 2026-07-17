//! Sled-backed persistence: retentive last-values + short history rings.
//!
//! Keys are TAG NAMES, not `TagId`s — ids are renumbered on config edits,
//! names are the stable identity. Two trees:
//! - `"retentive"`: name -> JSON [`StoredValue`] (last published value);
//! - `"history"`:   name -> JSON `Vec<StoredValue>` ring, newest LAST,
//!   truncated to the tag's `retain_last` (config-capped at 1000; rings are
//!   rewritten whole per update, so they are short by contract).
//!
//! Durability: sled auto-flushes (`flush_every_ms`, default 500 ms); the
//! engine calls [`Persist::flush`] on shutdown for a clean final state.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::value::TypedValue;

#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    #[error("sled: {0}")]
    Sled(#[from] sled::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

/// One persisted sample. `TypedValue` itself is not serde-derived (it lives
/// next to non-serde poller types), so mirror it locally.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v", rename_all = "snake_case")]
pub enum StoredKind {
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredValue {
    #[serde(flatten)]
    pub kind: StoredKind,
    /// Source timestamp, ms since the unix epoch.
    pub ts_ms: u64,
}

impl StoredValue {
    pub fn from_typed(value: &TypedValue, ts: SystemTime) -> Option<Self> {
        let kind = match value {
            TypedValue::Bool(b) => StoredKind::Bool(*b),
            TypedValue::Int(v) => StoredKind::Int(*v),
            TypedValue::UInt(v) => StoredKind::UInt(*v),
            TypedValue::Float(v) => StoredKind::Float(*v),
            TypedValue::Text(s) => StoredKind::Text(s.clone()),
            TypedValue::Bytes(b) => StoredKind::Bytes(b.clone()),
            TypedValue::Absent => return None,
        };
        let ts_ms = ts.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        Some(Self { kind, ts_ms })
    }

    pub fn to_typed(&self) -> (TypedValue, SystemTime) {
        let value = match &self.kind {
            StoredKind::Bool(b) => TypedValue::Bool(*b),
            StoredKind::Int(v) => TypedValue::Int(*v),
            StoredKind::UInt(v) => TypedValue::UInt(*v),
            StoredKind::Float(v) => TypedValue::Float(*v),
            StoredKind::Text(s) => TypedValue::Text(s.clone()),
            StoredKind::Bytes(b) => TypedValue::Bytes(b.clone()),
        };
        (value, UNIX_EPOCH + Duration::from_millis(self.ts_ms))
    }
}

pub struct Persist {
    _db: sled::Db,
    retentive: sled::Tree,
    history: sled::Tree,
}

impl Persist {
    /// Open (or create) the store at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PersistError> {
        let db = sled::open(path)?;
        Ok(Self {
            retentive: db.open_tree("retentive")?,
            history: db.open_tree("history")?,
            _db: db,
        })
    }

    /// Persist the last value of a retentive tag.
    pub fn store_retentive(&self, name: &str, v: &StoredValue) -> Result<(), PersistError> {
        self.retentive.insert(name.as_bytes(), serde_json::to_vec(v)?)?;
        Ok(())
    }

    /// Load one retentive tag (at boot).
    pub fn load_retentive(&self, name: &str) -> Result<Option<StoredValue>, PersistError> {
        match self.retentive.get(name.as_bytes())? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Append to a tag's history ring, truncating to `cap` newest entries.
    pub fn push_history(&self, name: &str, v: &StoredValue, cap: u16) -> Result<(), PersistError> {
        if cap == 0 {
            return Ok(());
        }
        let mut ring: Vec<StoredValue> = match self.history.get(name.as_bytes())? {
            Some(bytes) => serde_json::from_slice(&bytes)?,
            None => Vec::new(),
        };
        ring.push(v.clone());
        let cap = cap as usize;
        if ring.len() > cap {
            ring.drain(..ring.len() - cap);
        }
        self.history.insert(name.as_bytes(), serde_json::to_vec(&ring)?)?;
        Ok(())
    }

    /// The stored ring, oldest first.
    pub fn load_history(&self, name: &str) -> Result<Vec<StoredValue>, PersistError> {
        match self.history.get(name.as_bytes())? {
            Some(bytes) => Ok(serde_json::from_slice(&bytes)?),
            None => Ok(Vec::new()),
        }
    }

    /// Synchronous flush (engine shutdown).
    pub fn flush(&self) -> Result<(), PersistError> {
        self.retentive.flush()?;
        self.history.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sv(v: f64, ts_ms: u64) -> StoredValue {
        StoredValue { kind: StoredKind::Float(v), ts_ms }
    }

    #[test]
    fn retentive_round_trip_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let p = Persist::open(dir.path()).unwrap();
            p.store_retentive("pump.speed", &sv(147.5, 1000)).unwrap();
            p.flush().unwrap();
        }
        // Re-open = process restart.
        let p = Persist::open(dir.path()).unwrap();
        let got = p.load_retentive("pump.speed").unwrap().unwrap();
        assert_eq!(got, sv(147.5, 1000));
        assert_eq!(p.load_retentive("unknown").unwrap(), None);

        let (typed, ts) = got.to_typed();
        assert_eq!(typed, TypedValue::Float(147.5));
        assert_eq!(ts, UNIX_EPOCH + Duration::from_millis(1000));
    }

    #[test]
    fn history_ring_truncates_to_cap() {
        let dir = tempfile::tempdir().unwrap();
        let p = Persist::open(dir.path()).unwrap();
        for i in 0..10u64 {
            p.push_history("t", &sv(i as f64, i), 3).unwrap();
        }
        let ring = p.load_history("t").unwrap();
        assert_eq!(ring.len(), 3);
        assert_eq!(
            ring.iter().map(|s| s.ts_ms).collect::<Vec<_>>(),
            vec![7, 8, 9],
            "newest last, oldest dropped"
        );
        // cap 0 = off.
        p.push_history("off", &sv(1.0, 1), 0).unwrap();
        assert!(p.load_history("off").unwrap().is_empty());
    }

    #[test]
    fn stored_value_covers_all_kinds() {
        let ts = UNIX_EPOCH + Duration::from_millis(5);
        for v in [
            TypedValue::Bool(true),
            TypedValue::Int(-3),
            TypedValue::UInt(7),
            TypedValue::Float(1.25),
            TypedValue::Text("sn".into()),
            TypedValue::Bytes(vec![1, 2]),
        ] {
            let s = StoredValue::from_typed(&v, ts).unwrap();
            let json = serde_json::to_string(&s).unwrap();
            let back: StoredValue = serde_json::from_str(&json).unwrap();
            assert_eq!(back.to_typed().0, v);
        }
        assert!(StoredValue::from_typed(&TypedValue::Absent, ts).is_none());
    }
}
