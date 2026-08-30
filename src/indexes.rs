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

use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use crate::{derive_sitemap, read, site_pages, sitemap_entry, Site};
use crate::search;

#[derive(Default)]
pub struct Cache(Mutex<Option<Entry>>);

struct Entry {
    stamp: Stamp,
    sitemap: Arc<Value>,
    site_index: Arc<Value>,
}

/// What must be unchanged for the cache to hold: the pages/ population
/// (count + newest mtime — file rewrites bump the file mtime even though
/// the dir mtime stays put) and the status files another writer may touch.
#[derive(PartialEq, Clone)]
struct Stamp {
    page_count: u64,
    pages_mtime: Option<SystemTime>,
    sitemap_file: Option<SystemTime>,
    index_file: Option<SystemTime>,
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
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
    Entry {
        stamp,
        sitemap: Arc::new(sitemap),
        site_index: Arc::new(site_index),
    }
}

fn cached(site: &Site) -> (Arc<Value>, Arc<Value>) {
    let now = stamp(site);
    let mut guard = site.index_cache.0.lock().unwrap();
    match guard.as_ref() {
        Some(e) if e.stamp == now => (e.sitemap.clone(), e.site_index.clone()),
        _ => {
            let e = build(site, now);
            let out = (e.sitemap.clone(), e.site_index.clone());
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
    let _ = write_atomic(
        &status.join("sitemap.json"),
        &serde_json::to_vec(&sm).unwrap_or_default(),
    );
    let _ = write_atomic(
        &status.join("site-index.json"),
        &serde_json::to_vec(&idx).unwrap_or_default(),
    );
    // search.js searchSave touches this flag after every persisted update
    let _ = std::fs::write(status.join("index-updated"), b"");
    let mut guard = site.index_cache.0.lock().unwrap();
    *guard = Some(Entry {
        stamp: stamp(site),
        sitemap: Arc::new(sm),
        site_index: Arc::new(idx),
    });
}
