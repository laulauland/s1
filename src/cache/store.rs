//! The response store: one `redb` file (`~/.cache/s1/cache.redb`).
//!
//! Tables:
//! - `entries`: key -> encoded [`Entry`] (resolved model, content type,
//!   time, response bytes). No request payloads.
//! - `meta`: key -> (LRU sequence number, size in bytes).
//! - `lru`: sequence number -> key, oldest first.
//!
//! A hit moves the entry to the newest end. An insert evicts the oldest
//! entries until the stored responses, each charged [`ENTRY_OVERHEAD`] for
//! its keys and index rows, fit the data budget.
//!
//! Size cap: `max_bytes` bounds the file on disk, not just the responses.
//! redb keeps pages, metadata, and freed space beside the data (about as
//! much again under churn), so responses get [`DATA_SHARE`] of the cap, and
//! the file is compacted when it grows past the cap (plus [`FILE_SLACK`] for
//! the fixed size of an empty redb file). If the compacted file is still
//! over that, the oldest entries go and the file is compacted again, until
//! it fits. `clear` and `open` compact too.
//!
//! The file holds provider responses, so it is private: the directory is
//! mode 0700 and the file mode 0600, as for `auth.json`.

use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock, RwLockReadGuard};

use bytes::Bytes;
use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use super::key::Key;

const ENTRIES: TableDefinition<&[u8; 32], &[u8]> = TableDefinition::new("entries");
const META: TableDefinition<&[u8; 32], (u64, u64)> = TableDefinition::new("meta");
const LRU: TableDefinition<u64, &[u8; 32]> = TableDefinition::new("lru");

/// Share of `max_bytes` for stored responses, in quarters (3/4); the rest
/// is room for redb pages and freed space between compactions.
pub const DATA_SHARE: (u64, u64) = (3, 4);
/// Fixed redb file overhead (an empty store is about 1 MiB) allowed above
/// `max_bytes` before a compaction.
pub const FILE_SLACK: u64 = 2 << 20;
/// Bytes charged per entry on top of its encoded response: the key in all
/// three tables, the meta and LRU values, and B-tree page overhead (about
/// 120 bytes measured; rounded up). Without it many tiny entries overrun
/// the cap.
pub const ENTRY_OVERHEAD: u64 = 256;

#[derive(Debug, thiserror::Error)]
#[error("cache {path}: {message}")]
pub struct CacheError {
    pub path: String,
    pub message: String,
}

/// One cached response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub body: Bytes,
    pub content_type: Option<String>,
    /// The model version the provider reported.
    pub resolved_model: String,
    /// Unix seconds when stored.
    pub created: u64,
}

#[derive(Serialize, Deserialize)]
struct Header {
    resolved_model: String,
    content_type: Option<String>,
    created: u64,
}

impl Entry {
    fn encode(&self) -> Vec<u8> {
        let header = serde_json::to_vec(&Header {
            resolved_model: self.resolved_model.clone(),
            content_type: self.content_type.clone(),
            created: self.created,
        })
        .expect("header serializes");
        let mut out = Vec::with_capacity(4 + header.len() + self.body.len());
        out.extend_from_slice(&(header.len() as u32).to_le_bytes());
        out.extend_from_slice(&header);
        out.extend_from_slice(&self.body);
        out
    }

    fn decode(raw: &[u8]) -> Option<Self> {
        let len = u32::from_le_bytes(raw.get(..4)?.try_into().ok()?) as usize;
        let header: Header = serde_json::from_slice(raw.get(4..4 + len)?).ok()?;
        Some(Self {
            body: Bytes::copy_from_slice(raw.get(4 + len..)?),
            content_type: header.content_type,
            resolved_model: header.resolved_model,
            created: header.created,
        })
    }
}

/// Size and use of the store, for `GET /status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub entries: u64,
    /// Bytes of stored responses plus [`ENTRY_OVERHEAD`] per entry (at most
    /// [`DATA_SHARE`] of `max_bytes`).
    pub bytes: u64,
    pub max_bytes: u64,
    /// Size of the file on disk.
    pub file_bytes: u64,
}

struct Counters {
    next_seq: u64,
    bytes: u64,
    entries: u64,
}

pub struct Cache {
    /// Transactions take the read side; compaction needs the write side
    /// (redb's `compact` takes `&mut Database`).
    db: RwLock<Database>,
    path: PathBuf,
    max_bytes: u64,
    counters: Mutex<Counters>,
}

impl Cache {
    /// Open or create the store; `max_bytes` caps the file (see the module
    /// docs). Entries over the budget (a lowered cap) are evicted at once.
    pub fn open(path: &Path, max_bytes: u64) -> Result<Self, CacheError> {
        let err = |e: &dyn std::fmt::Display| CacheError {
            path: path.display().to_string(),
            message: e.to_string(),
        };
        make_private(path).map_err(|e| err(&e))?;
        let db = Database::create(path).map_err(|e| err(&e))?;
        let mut counters = Counters {
            next_seq: 0,
            bytes: 0,
            entries: 0,
        };
        {
            let tx = db.begin_write().map_err(|e| err(&e))?;
            {
                let mut entries = tx.open_table(ENTRIES).map_err(|e| err(&e))?;
                let mut meta = tx.open_table(META).map_err(|e| err(&e))?;
                let mut lru = tx.open_table(LRU).map_err(|e| err(&e))?;
                for row in meta.iter().map_err(|e| err(&e))? {
                    let (_, v) = row.map_err(|e| err(&e))?;
                    let (_, size) = v.value();
                    counters.bytes += size;
                    counters.entries += 1;
                }
                if let Some(row) = lru.last().map_err(|e| err(&e))? {
                    counters.next_seq = row.0.value() + 1;
                }
                evict(
                    &mut entries,
                    &mut meta,
                    &mut lru,
                    &mut counters,
                    data_budget(max_bytes),
                )
                .map_err(|e| err(&e))?;
            }
            tx.commit().map_err(|e| err(&e))?;
        }
        let cache = Self {
            db: RwLock::new(db),
            path: path.to_path_buf(),
            max_bytes,
            counters: Mutex::new(counters),
        };
        cache.compact_if_over()?;
        Ok(cache)
    }

    fn err(&self, e: impl std::fmt::Display) -> CacheError {
        CacheError {
            path: self.path.display().to_string(),
            message: e.to_string(),
        }
    }

    fn counters(&self) -> std::sync::MutexGuard<'_, Counters> {
        self.counters.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn db(&self) -> RwLockReadGuard<'_, Database> {
        self.db.read().unwrap_or_else(|e| e.into_inner())
    }

    fn file_bytes(&self) -> u64 {
        std::fs::metadata(&self.path).map_or(0, |m| m.len())
    }

    /// Rewrite the file without free pages. Needs no open transaction:
    /// the write lock waits for all of them.
    fn compact(&self) -> Result<(), CacheError> {
        let mut db = self.db.write().unwrap_or_else(|e| e.into_inner());
        db.compact().map_err(|e| self.err(e))?;
        Ok(())
    }

    /// Compact when the file has grown past the cap (plus [`FILE_SLACK`]).
    /// If the compacted file is still too big (the per-entry estimate was
    /// low), evict the oldest quarter of the entries and compact again,
    /// until the file fits or the store is empty.
    fn compact_if_over(&self) -> Result<(), CacheError> {
        let limit = self.max_bytes.saturating_add(FILE_SLACK);
        if self.file_bytes() <= limit {
            return Ok(());
        }
        self.compact()?;
        while self.file_bytes() > limit {
            let evicted = {
                let db = self.db();
                let mut c = self.counters();
                if c.entries == 0 {
                    break;
                }
                let drop = c.entries.div_ceil(4);
                let tx = db.begin_write().map_err(|e| self.err(e))?;
                let mut next = Counters {
                    next_seq: c.next_seq,
                    bytes: c.bytes,
                    entries: c.entries,
                };
                {
                    let mut entries = tx.open_table(ENTRIES).map_err(|e| self.err(e))?;
                    let mut meta = tx.open_table(META).map_err(|e| self.err(e))?;
                    let mut lru = tx.open_table(LRU).map_err(|e| self.err(e))?;
                    evict_oldest(&mut entries, &mut meta, &mut lru, &mut next, drop)
                        .map_err(|e| self.err(e))?;
                }
                tx.commit().map_err(|e| self.err(e))?;
                let evicted = c.entries - next.entries;
                c.bytes = next.bytes;
                c.entries = next.entries;
                evicted
            };
            if evicted == 0 {
                break;
            }
            self.compact()?;
        }
        Ok(())
    }

    pub fn stats(&self) -> Stats {
        let c = self.counters();
        Stats {
            entries: c.entries,
            bytes: c.bytes,
            max_bytes: self.max_bytes,
            file_bytes: self.file_bytes(),
        }
    }

    /// Look up an entry and mark it as just used.
    pub fn get(&self, key: &Key) -> Result<Option<Entry>, CacheError> {
        // Lock order everywhere: the database guard, then the counters. The
        // guard lives as long as every transaction, so a compaction never
        // meets an open one.
        let db = self.db();
        let found = {
            let tx = db.begin_read().map_err(|e| self.err(e))?;
            let entries = tx.open_table(ENTRIES).map_err(|e| self.err(e))?;
            let raw = entries.get(key).map_err(|e| self.err(e))?;
            raw.and_then(|v| Entry::decode(v.value()))
        };
        if found.is_some() {
            let mut c = self.counters();
            let seq = c.next_seq;
            c.next_seq += 1;
            let mut tx = db.begin_write().map_err(|e| self.err(e))?;
            // Losing a recency update in a crash is harmless.
            tx.set_durability(Durability::None)
                .map_err(|e| self.err(e))?;
            {
                let mut meta = tx.open_table(META).map_err(|e| self.err(e))?;
                let mut lru = tx.open_table(LRU).map_err(|e| self.err(e))?;
                let old = meta.get(key).map_err(|e| self.err(e))?.map(|v| v.value());
                if let Some((old_seq, size)) = old {
                    lru.remove(old_seq).map_err(|e| self.err(e))?;
                    lru.insert(seq, key).map_err(|e| self.err(e))?;
                    meta.insert(key, (seq, size)).map_err(|e| self.err(e))?;
                }
            }
            tx.commit().map_err(|e| self.err(e))?;
        }
        Ok(found)
    }

    /// Store an entry, then evict the least recently used entries until the
    /// total fits the data budget. An entry larger than the budget is not
    /// stored. Compacts the file if it has grown past the cap.
    pub fn put(&self, key: &Key, entry: &Entry) -> Result<(), CacheError> {
        let raw = entry.encode();
        let size = raw.len() as u64 + ENTRY_OVERHEAD;
        let budget = data_budget(self.max_bytes);
        if size > budget {
            return Ok(());
        }
        {
            let db = self.db();
            let mut c = self.counters();
            let seq = c.next_seq;
            c.next_seq += 1;
            let tx = db.begin_write().map_err(|e| self.err(e))?;
            let mut next = Counters {
                next_seq: c.next_seq,
                bytes: c.bytes,
                entries: c.entries,
            };
            {
                let mut entries = tx.open_table(ENTRIES).map_err(|e| self.err(e))?;
                let mut meta = tx.open_table(META).map_err(|e| self.err(e))?;
                let mut lru = tx.open_table(LRU).map_err(|e| self.err(e))?;
                let old = meta.get(key).map_err(|e| self.err(e))?.map(|v| v.value());
                if let Some((old_seq, old_size)) = old {
                    lru.remove(old_seq).map_err(|e| self.err(e))?;
                    next.bytes -= old_size;
                    next.entries -= 1;
                }
                entries
                    .insert(key, raw.as_slice())
                    .map_err(|e| self.err(e))?;
                meta.insert(key, (seq, size)).map_err(|e| self.err(e))?;
                lru.insert(seq, key).map_err(|e| self.err(e))?;
                next.bytes += size;
                next.entries += 1;
                evict(&mut entries, &mut meta, &mut lru, &mut next, budget)
                    .map_err(|e| self.err(e))?;
            }
            tx.commit().map_err(|e| self.err(e))?;
            c.bytes = next.bytes;
            c.entries = next.entries;
        }
        // The entry is stored; a failed compaction only leaves the file big.
        if let Err(e) = self.compact_if_over() {
            tracing::warn!(event = "cache_compact_failed", error = %e, "cache compaction failed");
        }
        Ok(())
    }

    /// Remove every entry and give the disk space back (compaction).
    pub fn clear(&self) -> Result<Stats, CacheError> {
        let before = {
            let db = self.db();
            let mut c = self.counters();
            let before = Stats {
                entries: c.entries,
                bytes: c.bytes,
                max_bytes: self.max_bytes,
                file_bytes: self.file_bytes(),
            };
            let tx = db.begin_write().map_err(|e| self.err(e))?;
            tx.delete_table(ENTRIES).map_err(|e| self.err(e))?;
            tx.delete_table(META).map_err(|e| self.err(e))?;
            tx.delete_table(LRU).map_err(|e| self.err(e))?;
            tx.open_table(ENTRIES).map_err(|e| self.err(e))?;
            tx.open_table(META).map_err(|e| self.err(e))?;
            tx.open_table(LRU).map_err(|e| self.err(e))?;
            tx.commit().map_err(|e| self.err(e))?;
            c.bytes = 0;
            c.entries = 0;
            before
        };
        self.compact()?;
        Ok(before)
    }
}

/// Create the store's directory (mode 0700) and file (mode 0600), and
/// tighten the modes of ones that exist already.
fn make_private(path: &Path) -> std::io::Result<()> {
    use std::fs;
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

/// Bytes of responses the store keeps for a cap of `max_bytes`.
pub fn data_budget(max_bytes: u64) -> u64 {
    max_bytes / DATA_SHARE.1 * DATA_SHARE.0
}

/// Drop the oldest entries until `c.bytes <= budget`.
fn evict(
    entries: &mut redb::Table<&[u8; 32], &[u8]>,
    meta: &mut redb::Table<&[u8; 32], (u64, u64)>,
    lru: &mut redb::Table<u64, &[u8; 32]>,
    c: &mut Counters,
    budget: u64,
) -> Result<(), redb::Error> {
    while c.bytes > budget {
        if !evict_one(entries, meta, lru, c)? {
            break;
        }
    }
    Ok(())
}

/// Drop the `n` oldest entries.
fn evict_oldest(
    entries: &mut redb::Table<&[u8; 32], &[u8]>,
    meta: &mut redb::Table<&[u8; 32], (u64, u64)>,
    lru: &mut redb::Table<u64, &[u8; 32]>,
    c: &mut Counters,
    n: u64,
) -> Result<(), redb::Error> {
    for _ in 0..n {
        if !evict_one(entries, meta, lru, c)? {
            break;
        }
    }
    Ok(())
}

/// Drop the oldest entry; `false` if the store is empty.
fn evict_one(
    entries: &mut redb::Table<&[u8; 32], &[u8]>,
    meta: &mut redb::Table<&[u8; 32], (u64, u64)>,
    lru: &mut redb::Table<u64, &[u8; 32]>,
    c: &mut Counters,
) -> Result<bool, redb::Error> {
    let Some(victim) = lru.pop_first()?.map(|(_, k)| *k.value()) else {
        return Ok(false);
    };
    let gone = meta.remove(&victim)?.map(|v| v.value());
    entries.remove(&victim)?;
    if let Some((_, s)) = gone {
        c.bytes = c.bytes.saturating_sub(s);
        c.entries = c.entries.saturating_sub(1);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(body: &str) -> Entry {
        Entry {
            body: Bytes::from(body.to_string()),
            content_type: Some("application/json".into()),
            resolved_model: "jev-1.13.0".into(),
            created: 1,
        }
    }

    fn k(n: u8) -> Key {
        [n; 32]
    }

    #[test]
    fn put_get_and_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c/cache.redb");
        {
            let c = Cache::open(&path, 1 << 20).unwrap();
            assert_eq!(c.get(&k(1)).unwrap(), None);
            c.put(&k(1), &entry("{\"a\":1}")).unwrap();
            assert_eq!(c.get(&k(1)).unwrap(), Some(entry("{\"a\":1}")));
            c.put(&k(1), &entry("{\"a\":2}")).unwrap();
            assert_eq!(c.stats().entries, 1);
        }
        let c = Cache::open(&path, 1 << 20).unwrap();
        assert_eq!(c.get(&k(1)).unwrap().unwrap().body, "{\"a\":2}");
        assert_eq!(c.stats().entries, 1);
        assert!(c.stats().bytes > 0);
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn store_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s1/cache.redb");
        let c = Cache::open(&path, 1 << 20).unwrap();
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
        // Compaction keeps the mode.
        c.put(&k(1), &entry("a")).unwrap();
        c.clear().unwrap();
        assert_eq!(mode(&path), 0o600);
        drop(c);
        // A store left with loose modes is tightened on open.
        std::fs::set_permissions(
            path.parent().unwrap(),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let _c = Cache::open(&path, 1 << 20).unwrap();
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn lru_eviction_keeps_recently_used_entries() {
        let dir = tempfile::tempdir().unwrap();
        let one = entry(&"x".repeat(100)).encode().len() as u64 + ENTRY_OVERHEAD;
        // A cap whose data budget holds exactly three entries.
        let cap = one * 3 * DATA_SHARE.1 / DATA_SHARE.0 + DATA_SHARE.1;
        assert_eq!(data_budget(cap) / one, 3);
        let c = Cache::open(&dir.path().join("cache.redb"), cap).unwrap();
        for n in 1..=3 {
            c.put(&k(n), &entry(&"x".repeat(100))).unwrap();
        }
        // Use 1, so 2 is the oldest.
        assert!(c.get(&k(1)).unwrap().is_some());
        c.put(&k(4), &entry(&"x".repeat(100))).unwrap();
        assert!(c.get(&k(2)).unwrap().is_none(), "least recently used goes");
        for n in [1, 3, 4] {
            assert!(c.get(&k(n)).unwrap().is_some(), "{n} stays");
        }
        assert!(c.stats().bytes <= data_budget(cap));
        // Too big for the cap: not stored.
        c.put(&k(9), &entry(&"x".repeat(1000))).unwrap();
        assert!(c.get(&k(9)).unwrap().is_none());
    }

    #[test]
    fn clear_removes_everything() {
        let dir = tempfile::tempdir().unwrap();
        let c = Cache::open(&dir.path().join("cache.redb"), 1 << 20).unwrap();
        c.put(&k(1), &entry("a")).unwrap();
        let before = c.clear().unwrap();
        assert_eq!(before.entries, 1);
        assert!(c.get(&k(1)).unwrap().is_none());
        assert_eq!(c.stats().entries, 0);
        c.put(&k(2), &entry("b")).unwrap();
        assert!(c.get(&k(2)).unwrap().is_some());
    }

    fn sized(n: usize) -> Entry {
        Entry {
            body: Bytes::from(vec![b'x'; n]),
            content_type: None,
            resolved_model: "m".into(),
            created: 1,
        }
    }

    fn key(i: u32) -> Key {
        let mut k = [0u8; 32];
        k[..4].copy_from_slice(&i.to_le_bytes());
        k
    }

    #[test]
    fn reopen_with_a_smaller_cap_evicts_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.redb");
        {
            let c = Cache::open(&path, 1 << 20).unwrap();
            for i in 0..10 {
                c.put(&key(i), &sized(10_000)).unwrap();
            }
            assert_eq!(c.stats().entries, 10);
        }
        let c = Cache::open(&path, 20_000).unwrap();
        let st = c.stats();
        assert!(st.bytes <= data_budget(20_000), "{st:?}");
        assert!(st.bytes <= st.max_bytes, "{st:?}");
        assert_eq!(st.entries, 1, "{st:?}");
        // The newest entry stays; the oldest are gone.
        assert!(c.get(&key(9)).unwrap().is_some());
        assert!(c.get(&key(0)).unwrap().is_none());
    }

    #[test]
    fn churn_keeps_the_file_near_the_cap_and_clear_frees_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.redb");
        let cap: u64 = 4 << 20;
        let c = Cache::open(&path, cap).unwrap();
        let empty = c.stats().file_bytes;
        let mut s: u64 = 1;
        let mut peak = 0;
        for i in 0..3000u32 {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let n = 1000 + (s >> 33) as usize % 30_000;
            c.put(&key(i), &sized(n)).unwrap();
            if i % 7 == 0 {
                let _ = c.get(&key(i / 2)).unwrap();
            }
            peak = peak.max(c.stats().file_bytes);
        }
        let st = c.stats();
        assert!(st.bytes <= data_budget(cap), "{st:?}");
        assert!(
            peak <= cap + FILE_SLACK,
            "file peaked at {peak} bytes, cap {cap}"
        );
        let before = c.clear().unwrap();
        assert!(before.entries > 0);
        let after = c.stats();
        assert_eq!(after.entries, 0);
        assert!(
            after.file_bytes <= empty + (64 << 10),
            "clear left {} bytes (empty store: {empty})",
            after.file_bytes
        );
    }

    #[test]
    fn many_tiny_entries_stay_under_the_cap() {
        // Per-entry bookkeeping (keys, the meta and LRU rows, B-tree
        // pages) dwarfs a tiny response; the file must still fit the cap.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.redb");
        let cap: u64 = 1 << 20;
        let c = Cache::open(&path, cap).unwrap();
        const N: u32 = 20_000;
        for i in 0..N {
            c.put(&key(i), &sized(1)).unwrap();
            let st = c.stats();
            assert!(
                st.file_bytes <= cap + FILE_SLACK,
                "after {i} puts the file is {} bytes, cap {cap}: {st:?}",
                st.file_bytes
            );
        }
        assert!(c.stats().entries > 0);
        // The newest entry survives.
        assert!(c.get(&key(N - 1)).unwrap().is_some());
    }

    #[test]
    fn understated_sizes_still_fit_the_cap_after_compaction() {
        // A store whose recorded sizes are too small (written before
        // ENTRY_OVERHEAD existed): eviction by budget keeps everything, so
        // the compacted file is still over the cap and must shed entries.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.redb");
        let cap: u64 = 1 << 20;
        const N: u32 = 30_000;
        {
            let db = Database::create(&path).unwrap();
            let tx = db.begin_write().unwrap();
            {
                let mut entries = tx.open_table(ENTRIES).unwrap();
                let mut meta = tx.open_table(META).unwrap();
                let mut lru = tx.open_table(LRU).unwrap();
                let raw = sized(1).encode();
                for i in 0..N {
                    let k = key(i);
                    entries.insert(&k, raw.as_slice()).unwrap();
                    meta.insert(&k, (u64::from(i), 1)).unwrap();
                    lru.insert(u64::from(i), &k).unwrap();
                }
            }
            tx.commit().unwrap();
        }
        assert!(std::fs::metadata(&path).unwrap().len() > cap + FILE_SLACK);
        let c = Cache::open(&path, cap).unwrap();
        let st = c.stats();
        assert!(st.file_bytes <= cap + FILE_SLACK, "{st:?}");
        assert!(st.entries > 0 && st.entries < u64::from(N), "{st:?}");
        // The oldest went; the newest stayed.
        assert!(c.get(&key(0)).unwrap().is_none());
        assert!(c.get(&key(N - 1)).unwrap().is_some());
    }
}
