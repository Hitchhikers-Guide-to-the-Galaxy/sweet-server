//! Sitemap + site-index lifecycle: memoised reads, node-faithful writes.
//!
//! READS. The reference keeps both indexes in memory (2-minute idle
//! timeout) and persists them to status/. Sweet instead memoises per Site:
//! a cheap stat-fingerprint of pages/ and the two status files decides
//! whether the cached values are current — a stat sweep per request instead
//! of re-reading and re-parsing every page (an 800-page site made the
//! derive path a per-request cost; the fingerprint also notices when the
//! node farm rewrites a status file under a dual-served site).
//!
//! WRITES. The reference updates both indexes after every action
//! (server.js:912-917, sitemaphandler/searchhandler.update) and persists
//! them atomically. Before this module sweet never wrote them, so any
//! write against a site carrying prebuilt status files left the served
//! sitemap/search/neighborhood permanently stale — WITH a stale
//! Last-Modified, so neighbors never re-polled. write_update mirrors the
//! reference: splice the entry into the served sitemap (replace by slug,
//! push new at the end, remove on delete), rebuild the serialized search
//! index, write both compact via tmp+rename, and touch status/index-updated
//! (the flag the reference's startUp uses to schedule a reindex).

use bytes::Bytes;
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::{derive_sitemap, read, site_pages, sitemap_entry, Site};
use crate::search;

#[derive(Default)]
pub struct Cache(Mutex<Option<Entry>>);

/// Compact page-JSON byte cache (Sweet Speedup Plan, Phase 2). Page files
/// are pretty on disk; the reference parses and re-stringifies compact on
/// every GET (~68 KB vs the 86 KB raw file). Serving the raw bytes was
/// fast but 27% heavier on the wire; parsing per request was the cost the
/// route already paid to validate. This keeps the compact form keyed by
/// slug and validated by file mtime, so a hot page skips both the parse
/// and the serialise. Bounded by flush-at-cap: crossing PAGE_CACHE_MAX
/// clears the map and the hot set re-warms at one miss per page — cruder
/// than LRU eviction, with the same memory guarantee and no bookkeeping.
#[derive(Default)]
pub struct PageCache(Mutex<std::collections::HashMap<String, (SystemTime, Bytes, String)>>);

const PAGE_CACHE_MAX: usize = 512;

impl PageCache {
    pub fn get(&self, slug: &str, mtime: SystemTime) -> Option<(Bytes, String)> {
        let g = self.0.lock().unwrap();
        g.get(slug)
            .filter(|(m, _, _)| *m == mtime)
            .map(|(_, b, e)| (b.clone(), e.clone()))
    }
    pub fn put(&self, slug: &str, mtime: SystemTime, bytes: Bytes, etag: String) {
        let mut g = self.0.lock().unwrap();
        if g.len() >= PAGE_CACHE_MAX {
            g.clear();
        }
        g.insert(slug.to_string(), (mtime, bytes, etag));
    }
}

struct Entry {
    stamp: Stamp,
    // When the pages/ walk last ran. The full walk (a stat per page file)
    // costs ~0.5 ms on a 585-page site — more than serving the cached
    // bytes. Between walks a request pays only three stats (pages dir +
    // both status files): a rename into pages/ (the reference's atomic
    // write) moves the dir mtime and is caught instantly; an in-place
    // rewrite is caught by the next walk, at most WALK_EVERY later.
    // Sweet's own writes bypass all of this via write_update.
    walked: std::time::Instant,
    sitemap: Arc<Value>,
    site_index: Arc<Value>,
    // The serialised form, built once per change. Serving re-serialised
    // 297 KB (sitemap) / 1 MB (site-index) per GET was the live-farm
    // bench's worst finding (Sweet Speedup Plan, Phase 1): node answers
    // from cached bytes in ~0.45 ms where re-serialisation cost ~2 ms.
    // Bytes clones are refcounted, so a request never copies the buffer.
    sitemap_bytes: Bytes,
    site_index_bytes: Bytes,
    // Weak validators, hashed once at build time (Speedup Phase 3). A
    // per-request hash of a 1 MB buffer would cost more than serving it.
    sitemap_etag: String,
    site_index_etag: String,
}

/// W/"len-hash" — weak, like express's; the exact format is ours to pick,
/// only stability per byte-identical body matters.
pub fn etag_of(bytes: &[u8]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    format!("W/\"{:x}-{:x}\"", bytes.len(), h.finish())
}

/// What must be unchanged for the cache to hold: the pages/ population
/// (count + newest mtime — file rewrites bump the file mtime even though
/// the dir mtime stays put) and the status files another writer may touch.
#[derive(PartialEq, Clone)]
struct Stamp {
    page_count: u64,
    pages_mtime: Option<SystemTime>,
    pages_dir_mtime: Option<SystemTime>,
    sitemap_file: Option<SystemTime>,
    index_file: Option<SystemTime>,
}

/// How stale an in-place external rewrite may go unnoticed. Renames and
/// status-file writes are caught instantly by the cheap check.
const WALK_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The three stats a between-walks request pays.
fn cheap_stamp(site: &Site) -> (Option<SystemTime>, Option<SystemTime>, Option<SystemTime>) {
    (
        mtime(&site.data.join("pages")),
        mtime(&site.data.join("status/sitemap.json")),
        mtime(&site.data.join("status/site-index.json")),
    )
}

fn stamp(site: &Site) -> Stamp {
    let mut count = 0u64;
    let mut newest: Option<SystemTime> = None;
    if let Ok(entries) = std::fs::read_dir(site.data.join("pages")) {
        for e in entries.flatten() {
            if let Ok(meta) = e.metadata() {
                if meta.is_file() {
                    count += 1;
                    let m = meta.modified().ok();
                    if m > newest {
                        newest = m;
                    }
                }
            }
        }
    }
    Stamp {
        page_count: count,
        pages_mtime: newest,
        pages_dir_mtime: mtime(&site.data.join("pages")),
        sitemap_file: mtime(&site.data.join("status/sitemap.json")),
        index_file: mtime(&site.data.join("status/site-index.json")),
    }
}

/// Serve-side truth, same fallback the routes always had: the prebuilt
/// status file when present, else derived from pages/.
fn build(site: &Site, stamp: Stamp) -> Entry {
    let sitemap = read(&site.data.join("status/sitemap.json"))
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_else(|| derive_sitemap(site));
    let site_index = read(&site.data.join("status/site-index.json"))
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .unwrap_or_else(|| search::site_index(&site_pages(site)));
    // compact, the same bytes node's res.json would send
    let sitemap_bytes = Bytes::from(serde_json::to_vec(&sitemap).unwrap_or_default());
    let site_index_bytes = Bytes::from(serde_json::to_vec(&site_index).unwrap_or_default());
    Entry {
        stamp,
        walked: std::time::Instant::now(),
        sitemap: Arc::new(sitemap),
        site_index: Arc::new(site_index),
        sitemap_etag: etag_of(&sitemap_bytes),
        site_index_etag: etag_of(&site_index_bytes),
        sitemap_bytes,
        site_index_bytes,
    }
}

fn cached(site: &Site) -> (Arc<Value>, Arc<Value>, Bytes, Bytes, String, String) {
    let mut guard = site.index_cache.0.lock().unwrap();
    // fast path: recent walk + unchanged cheap stamp -> serve as-is
    if let Some(e) = guard.as_ref() {
        let (dir, sm, idx) = cheap_stamp(site);
        if e.walked.elapsed() < WALK_EVERY
            && dir == e.stamp.pages_dir_mtime
            && sm == e.stamp.sitemap_file
            && idx == e.stamp.index_file
        {
            return (e.sitemap.clone(), e.site_index.clone(),
                    e.sitemap_bytes.clone(), e.site_index_bytes.clone(),
                    e.sitemap_etag.clone(), e.site_index_etag.clone());
        }
    }
    let now = stamp(site);
    match guard.as_mut() {
        Some(e) if e.stamp == now => {
            e.walked = std::time::Instant::now();
            (e.sitemap.clone(), e.site_index.clone(),
             e.sitemap_bytes.clone(), e.site_index_bytes.clone(),
             e.sitemap_etag.clone(), e.site_index_etag.clone())
        }
        _ => {
            let e = build(site, now);
            let out = (e.sitemap.clone(), e.site_index.clone(),
                       e.sitemap_bytes.clone(), e.site_index_bytes.clone(),
                       e.sitemap_etag.clone(), e.site_index_etag.clone());
            *guard = Some(e);
            out
        }
    }
}

pub fn sitemap(site: &Site) -> Arc<Value> {
    cached(site).0
}

pub fn site_index(site: &Site) -> Arc<Value> {
    cached(site).1
}

/// The sitemap's parsed form (for Last-Modified), its served bytes, and
/// its validator.
pub fn sitemap_with_bytes(site: &Site) -> (Arc<Value>, Bytes, String) {
    let (map, _, bytes, _, etag, _) = cached(site);
    (map, bytes, etag)
}

/// The site-index bytes and validator, plus the sitemap's parsed form
/// (its stamp is the Last-Modified for both routes).
pub fn site_index_with_stamp(site: &Site) -> (Bytes, Arc<Value>, String) {
    let (map, _, _, bytes, _, etag) = cached(site);
    (bytes, map, etag)
}

/// JSON.stringify-compact write via tmp+rename (write-file-atomic's shape).
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// After a successful page write (Some) or delete (None): bring the served
/// sitemap and the persisted status files up to date, reference-style.
pub fn write_update(site: &Site, slug: &str, page: Option<&Value>) {
    let mut sm = (*sitemap(site)).clone();
    {
        let entries = match sm.as_array_mut() {
            Some(a) => a,
            None => return,
        };
        let idx = entries.iter().position(|e| e["slug"] == slug);
        match (page, idx) {
            // sitemap.js sitemapUpdate: replace in place, else push at end
            (Some(p), Some(i)) => entries[i] = sitemap_entry(slug, p),
            (Some(p), None) => entries.push(sitemap_entry(slug, p)),
            // sitemapRemovePage: splice
            (None, Some(i)) => {
                entries.remove(i);
            }
            (None, None) => {}
        }
    }
    // full rebuild — the exact form the reference's own startUp reindex
    // produces (its in-memory incremental adds converge here too)
    let idx = search::site_index(&site_pages(site));
    let status = site.data.join("status");
    let sm_bytes = serde_json::to_vec(&sm).unwrap_or_default();
    let idx_bytes = serde_json::to_vec(&idx).unwrap_or_default();
    let _ = write_atomic(&status.join("sitemap.json"), &sm_bytes);
    let _ = write_atomic(&status.join("site-index.json"), &idx_bytes);
    // search.js searchSave touches this flag after every persisted update
    let _ = std::fs::write(status.join("index-updated"), b"");
    let mut guard = site.index_cache.0.lock().unwrap();
    *guard = Some(Entry {
        stamp: stamp(site),
        walked: std::time::Instant::now(),
        sitemap: Arc::new(sm),
        site_index: Arc::new(idx),
        sitemap_etag: etag_of(&sm_bytes),
        site_index_etag: etag_of(&idx_bytes),
        sitemap_bytes: Bytes::from(sm_bytes),
        site_index_bytes: Bytes::from(idx_bytes),
    });
}
