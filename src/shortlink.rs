//! wiki-plugin-shortlink's plugin-served surface, ported natively — the
//! road the dossier recommends (plugin-port-v1, NATIVE): the resolver
//! GET /plugin/shortlink/{key}, its key-scoped asset proxy, and the
//! farm-door `expand` handler. Expand lives here (not in the sidecar) so
//! both doors hit ONE failure breaker, the way the reference shares its
//! singletons through the CJS require cache (api/handlers.js:4-9).
//!
//! State follows store.js exactly: one machine-global JSON file
//! (SHORTLINK_STORE, read per call — tests switch it under a live
//! process), mtime-gated re-reads so external writers need no restart,
//! tmp+rename writes, and a 30-day purge of dead entries on every save.

use axum::{body::Body, http::header, http::StatusCode, response::Response};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// One body for every failure. 404 (never existed), 410 (was live once)
// and 500 must be indistinguishable to a visitor, or the keyspace can be
// probed. Verbatim server.js:25-37.
const GONE_HTML: &str = r#"<!doctype html>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Link expired</title>
<style>
  body { font: 18px/1.5 Georgia, serif; max-width: 34em; margin: 15vh auto; padding: 0 1em; color: #333; }
  h1 { font-size: 1.3em; }
</style>
<h1>This short link has expired or been revoked.</h1>
<p>Short links here are deliberately temporary — the page it pointed at is
likely still fine, but this way in has closed. Ask whoever shared it for a
fresh link.</p>
<p><em>Don't panic.</em></p>"#;

const CSP: &str = "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'";

// ------------------------------------------------------------ small time

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Date.parse for the ISO shapes the store carries:
/// YYYY-MM-DD[THH:MM[:SS[.fff]]][Z|±HH:MM]. Zone-less datetimes are read
/// as UTC (the reference reads them as local; every writer stamps Z).
fn parse_iso_ms(s: &str) -> Option<f64> {
    let bytes = s.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        s.get(from..from + len)?.parse::<i64>().ok()
    };
    let (y, mo, d) = (num(0, 4)?, num(5, 2)?, num(8, 2)?);
    if bytes.get(4) != Some(&b'-') || bytes.get(7) != Some(&b'-') {
        return None;
    }
    let mut ms = days_from_civil(y, mo, d) as f64 * 86_400_000.0;
    let mut i = 10;
    if bytes.get(i) == Some(&b'T') || bytes.get(i) == Some(&b' ') {
        let (h, mi) = (num(i + 1, 2)?, num(i + 4, 2)?);
        ms += (h * 3_600_000 + mi * 60_000) as f64;
        i += 6;
        if bytes.get(i) == Some(&b':') {
            ms += (num(i + 1, 2)? * 1000) as f64;
            i += 3;
            if bytes.get(i) == Some(&b'.') {
                let mut frac = 0.0;
                let mut scale = 100.0;
                i += 1;
                while let Some(c) = bytes.get(i).filter(|c| c.is_ascii_digit()) {
                    frac += (c - b'0') as f64 * scale;
                    scale /= 10.0;
                    i += 1;
                }
                ms += frac;
            }
        }
    }
    match bytes.get(i) {
        None | Some(b'Z') => Some(ms),
        Some(sign @ (b'+' | b'-')) => {
            let off = (num(i + 1, 2)? * 60 + num(i + 4, 2)?) * 60_000;
            Some(if *sign == b'+' {
                ms - off as f64
            } else {
                ms + off as f64
            })
        }
        _ => None,
    }
}

/// JS Date#toISOString — millisecond precision, Z suffix.
fn to_iso(ms: f64) -> String {
    let total = ms as i64;
    let (days, rem) = (total.div_euclid(86_400_000), total.rem_euclid(86_400_000));
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{y:04}-{mo:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000
    )
}

/// JS Date#toUTCString — "Fri, 01 Jan 2026 00:00:00 GMT".
fn to_utc_string(ms: f64) -> String {
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MO: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun",
                            "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let total = ms as i64;
    let (days, rem) = (total.div_euclid(86_400_000), total.rem_euclid(86_400_000));
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{}, {d:02} {} {y:04} {:02}:{:02}:{:02} GMT",
        WD[days.rem_euclid(7) as usize],
        MO[(mo - 1) as usize],
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60
    )
}

/// JS truthiness for the store's fields (null, false, "" and 0 are falsy).
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

fn date_ms(v: &Value) -> Option<f64> {
    v.as_str().and_then(parse_iso_ms)
}

// ------------------------------------------------------- store (store.js)

const PURGE_AFTER_MS: f64 = 30.0 * 24.0 * 60.0 * 60.0 * 1000.0;

struct StoreCache {
    cache: Option<Value>,
    mtime: Option<SystemTime>,
    path: Option<PathBuf>,
}

fn store_state() -> &'static Mutex<StoreCache> {
    static STATE: OnceLock<Mutex<StoreCache>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(StoreCache { cache: None, mtime: None, path: None })
    })
}

/// Read per call, never at boot — tests switch it under a live process.
fn store_path() -> PathBuf {
    match std::env::var("SHORTLINK_STORE") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            .join(".wiki-shortlinks")
            .join("links.json"),
    }
}

fn empty_store() -> Value {
    json!({"version": 1, "links": {}})
}

fn load_locked(st: &mut StoreCache) -> Value {
    let file = store_path();
    if st.path.as_deref() != Some(file.as_path()) {
        // env changed under us (tests) — drop everything
        st.cache = None;
        st.mtime = None;
        st.path = Some(file.clone());
    }
    let mtime = match std::fs::metadata(&file).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => {
            st.cache = Some(empty_store());
            st.mtime = None;
            return st.cache.clone().unwrap();
        }
    };
    if st.cache.is_none() || st.mtime != Some(mtime) {
        match std::fs::read(&file)
            .ok()
            .map(|b| serde_json::from_slice::<Value>(&b))
        {
            Some(Ok(v)) => {
                st.cache = Some(if v["links"].is_object() { v } else { empty_store() });
            }
            _ => {
                // Unreadable or mid-edit despite the rename convention: keep
                // the last good cache rather than replacing real links.
                if st.cache.is_none() {
                    st.cache = Some(empty_store());
                }
            }
        }
        st.mtime = Some(mtime);
    }
    st.cache.clone().unwrap()
}

// Drop entries long past caring about; live and recently-dead ones stay.
fn purge(store: &mut Value) {
    let now = now_ms();
    let cutoff = now - PURGE_AFTER_MS;
    if let Some(links) = store["links"].as_object_mut() {
        links.retain(|_, link| {
            let dead_since = if truthy(&link["revoked"]) {
                date_ms(&link["revoked"])
            } else if truthy(&link["expires"])
                && date_ms(&link["expires"]).map(|e| e < now).unwrap_or(false)
            {
                date_ms(&link["expires"])
            } else {
                None
            };
            !dead_since.map(|d| d < cutoff).unwrap_or(false)
        });
    }
}

fn save_locked(st: &mut StoreCache, mut store: Value) -> std::io::Result<()> {
    let file = store_path();
    purge(&mut store);
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = PathBuf::from(format!(
        "{}.tmp-{}",
        file.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&tmp, serde_json::to_string_pretty(&store).unwrap() + "\n")?;
    std::fs::rename(&tmp, &file)?;
    st.mtime = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
    st.path = Some(file);
    st.cache = Some(store);
    Ok(())
}

fn store_get(key: &str) -> Option<Value> {
    let mut st = store_state().lock().unwrap();
    let store = load_locked(&mut st);
    let link = &store["links"][key];
    truthy(link).then(|| link.clone())
}

// ---------------------------------------------------- mint.js (resolving)

fn is_live(link: &Value, now: f64) -> bool {
    if truthy(&link["revoked"]) {
        return false;
    }
    if truthy(&link["expires"]) {
        // strict '<': a link expiring at exactly now is still live
        if date_ms(&link["expires"]).map(|e| e < now).unwrap_or(false) {
            return false;
        }
    }
    if truthy(&link["singleUse"]) && truthy(&link["usedAt"]) {
        return false;
    }
    true
}

enum Resolved {
    Missing,
    Dead,
    Live { location: String, link: Value },
}

/// mint.js resolve: burn/count FIRST, persisted to disk before the
/// response goes out — a crash between the state change and the response
/// leaves a single-use link dead, never doubly live.
fn resolve(key: &str) -> std::io::Result<Resolved> {
    let mut st = store_state().lock().unwrap();
    let mut store = load_locked(&mut st);
    let link = &store["links"][key];
    if !truthy(link) {
        return Ok(Resolved::Missing);
    }
    let now = now_ms();
    if !is_live(link, now) {
        return Ok(Resolved::Dead);
    }
    let mut link = link.clone();
    link["uses"] = json!(link["uses"].as_i64().unwrap_or(0) + 1);
    if truthy(&link["singleUse"]) {
        link["usedAt"] = json!(to_iso(now));
    }
    store["links"][key] = link.clone();
    save_locked(&mut st, store)?;
    let host = link["host"].as_str().unwrap_or("").to_string();
    let scheme = link["scheme"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("http");
    // Location is REBUILT from the stored pairs, never replayed from the
    // stored url: same-host pairs fold to 'view', queries drop.
    let path = link["pairs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let origin = p["origin"].as_str().unwrap_or("");
            let slug = p["slug"].as_str().unwrap_or("");
            format!("{}/{slug}", if origin == host { "view" } else { origin })
        })
        .collect::<Vec<_>>()
        .join("/");
    Ok(Resolved::Live {
        location: format!("{scheme}://{host}/{path}"),
        link,
    })
}

/// mint.js describe — the register's view of a link, never the raw record.
fn describe(link: &Value) -> Value {
    json!({
        "key": link["key"],
        "url": link["url"],
        "title": link["title"],
        "note": link["note"],
        "created": link["created"],
        "expires": link["expires"],
        "singleUse": link["singleUse"],
        "uses": link["uses"],
        "usedAt": link["usedAt"],
        "revoked": link["revoked"],
        "capability": link["capability"],
        "proxy": truthy(&link["proxy"]),
        "live": is_live(link, now_ms()),
    })
}

// ------------------------------------------------ ratelimit (ratelimit.js)

const BUCKET_SIZE: f64 = 30.0;
const REFILL_PER_MS: f64 = 10.0 / 60000.0; // 10 tokens per minute
const FAILURE_WINDOW_MS: f64 = 60.0 * 60.0 * 1000.0;
const FAILURE_LIMIT: usize = 60;

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: f64,
}

fn buckets() -> &'static Mutex<HashMap<String, Bucket>> {
    static B: OnceLock<Mutex<HashMap<String, Bucket>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(HashMap::new()))
}

fn failures() -> &'static Mutex<Vec<f64>> {
    static F: OnceLock<Mutex<Vec<f64>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(Vec::new()))
}

fn allow(ip: &str) -> bool {
    let now = now_ms();
    let mut map = buckets().lock().unwrap();
    // the reference clears the WHOLE map past 10 000 IPs — the detached
    // bucket still answers this one request, then is forgotten
    let mut cleared = false;
    let mut b = match map.get(ip).copied() {
        Some(b) => b,
        None => {
            let b = Bucket { tokens: BUCKET_SIZE, at: now };
            map.insert(ip.to_string(), b);
            if map.len() > 10000 {
                map.clear();
                cleared = true;
            }
            b
        }
    };
    b.tokens = (b.tokens + (now - b.at) * REFILL_PER_MS).min(BUCKET_SIZE);
    b.at = now;
    let ok = if b.tokens < 1.0 {
        false
    } else {
        b.tokens -= 1.0;
        true
    };
    if !cleared {
        map.insert(ip.to_string(), b);
    }
    ok
}

fn breaker_open() -> bool {
    let cutoff = now_ms() - FAILURE_WINDOW_MS;
    let mut f = failures().lock().unwrap();
    f.retain(|t| *t > cutoff);
    f.len() >= FAILURE_LIMIT
}

fn record_failure() {
    failures().lock().unwrap().push(now_ms());
}

// ------------------------------------- the farm-door expand handler

/// api/handlers.js expand: the same question as the resolver ("is this
/// key live and where does it go") without redirecting — it must not
/// become the cheap way to enumerate the keyspace, so it shares the ONE
/// breaker above with the resolver route.
pub fn expand(args: &Value) -> Result<Value, (u16, String)> {
    if breaker_open() {
        return Err((429, "too many failed lookups — cooling down".into()));
    }
    let key = args["key"].as_str().unwrap_or("");
    match store_get(key) {
        Some(link) => Ok(describe(&link)),
        None => {
            record_failure();
            Err((404, "no such key".into()))
        }
    }
}

// ----------------------------------------------------- reader (reader.js)

fn is_local_family(origin: &str) -> bool {
    origin == "localhost" || origin.ends_with(".localhost") || origin.ends_with(".local")
}

fn scheme_for(origin: &str) -> &'static str {
    if is_local_family(origin) {
        "http"
    } else {
        "https"
    }
}

struct Upstream {
    status: u16,
    ctype: String,
    body: Vec<u8>,
}

/// reader.js get(): local-family origins are dialed at 127.0.0.1 with the
/// origin carried in the Host header (the OS resolver knows nothing of
/// *.localhost); remote origins resolve normally over https with SNI.
/// Redirects are NOT followed (node http.request does not).
fn http_get(url: &str) -> Result<Upstream, String> {
    let hostname = url
        .split("://")
        .nth(1)
        .unwrap_or("")
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    let mut builder = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .redirects(0);
    if is_local_family(hostname) {
        builder = builder.resolver(|netloc: &str| -> std::io::Result<Vec<std::net::SocketAddr>> {
            let port = netloc
                .rsplit(':')
                .next()
                .and_then(|p| p.parse().ok())
                .unwrap_or(80);
            Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))])
        });
    }
    let resp = match builder.build().get(url).call() {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(e.to_string()),
    };
    let status = resp.status();
    let ctype = resp
        .header("Content-Type")
        .unwrap_or("application/octet-stream")
        .to_string();
    let mut body = Vec::new();
    resp.into_reader()
        .read_to_end(&mut body)
        .map_err(|e| e.to_string())?;
    Ok(Upstream { status, ctype, body })
}

struct Fetched {
    origin: String,
    slug: String,
    page: Option<Value>, // None = the error branch renders a placeholder
}

fn fetch_lineup(link: &Value) -> Vec<Fetched> {
    link["pairs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let origin = p["origin"].as_str().unwrap_or("").to_string();
            let slug = p["slug"].as_str().unwrap_or("").to_string();
            let url = format!("{}://{origin}/{slug}.json", scheme_for(&origin));
            let page = match http_get(&url) {
                Ok(u) if u.status == 200 => serde_json::from_slice(&u.body).ok(),
                _ => None,
            };
            Fetched { origin, slug, page }
        })
        .collect()
}

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn re(pattern: &str, cell: &'static OnceLock<regex::Regex>) -> &'static regex::Regex {
    cell.get_or_init(|| regex::Regex::new(pattern).unwrap())
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The em rule `(^|\W)\*([^*\n]+)\*(?=\W|$)` — the lookahead has no rust
/// regex spelling, so the boundary checks run by hand.
fn em_pass(s: &str) -> String {
    static EM: OnceLock<regex::Regex> = OnceLock::new();
    let em = re(r"\*([^*\n]+)\*", &EM);
    let mut out = String::new();
    let mut last = 0;
    let mut search = 0;
    while search <= s.len() {
        let Some(c) = em.captures(&s[search..]) else { break };
        let m = c.get(0).unwrap();
        let (mstart, mend) = (search + m.start(), search + m.end());
        let prefix_ok =
            mstart == 0 || s[..mstart].chars().last().map(|c| !is_word(c)).unwrap_or(true);
        let suffix_ok =
            mend == s.len() || s[mend..].chars().next().map(|c| !is_word(c)).unwrap_or(true);
        if prefix_ok && suffix_ok {
            out.push_str(&s[last..mstart]);
            out.push_str("<em>");
            out.push_str(c.get(1).unwrap().as_str());
            out.push_str("</em>");
            last = mend;
            search = mend;
        } else {
            search = mstart + s[mstart..].chars().next().map(char::len_utf8).unwrap_or(1);
        }
    }
    out.push_str(&s[last..]);
    out
}

/// Minimal inline markup over already-escaped text: bold, italic, code.
/// [[Internal links]] go inert — the capability grants the minted pages only.
fn inline(escaped: &str) -> String {
    static WIKI: OnceLock<regex::Regex> = OnceLock::new();
    static EXT: OnceLock<regex::Regex> = OnceLock::new();
    static STRONG: OnceLock<regex::Regex> = OnceLock::new();
    static CODE: OnceLock<regex::Regex> = OnceLock::new();
    let s = re(r"\[\[([^\]]+)\]\]", &WIKI).replace_all(
        escaped,
        r#"<span class="wikilink" title="not included in this share">$1</span>"#,
    );
    let s = re(r"\[(https?://[^ \]]+) ([^\]]+)\]", &EXT)
        .replace_all(&s, r#"<a href="$1" rel="nofollow noopener">$2</a>"#);
    let s = re(r"\*\*([^*]+)\*\*", &STRONG).replace_all(&s, "<strong>$1</strong>");
    let s = em_pass(&s);
    re(r"`([^`]+)`", &CODE)
        .replace_all(&s, "<code>$1</code>")
        .into_owned()
}

fn render_markdown(text: &str) -> String {
    static H: OnceLock<regex::Regex> = OnceLock::new();
    static LI: OnceLock<regex::Regex> = OnceLock::new();
    let h_re = re(r"^(#{1,4}) +(.*)$", &H);
    let li_re = re(r"^[-*] +(.*)$", &LI);
    let mut out: Vec<String> = Vec::new();
    let mut list = false;
    for raw in text.split('\n') {
        let line = esc(raw);
        let h = h_re.captures(&line);
        let li = li_re.captures(&line);
        if let (Some(li), None) = (&li, &h) {
            if !list {
                out.push("<ul>".into());
                list = true;
            }
            out.push(format!("<li>{}</li>", inline(&li[1])));
            continue;
        }
        if list {
            out.push("</ul>".into());
            list = false;
        }
        if let Some(h) = h {
            let level = (h[1].len() + 2).min(6); // page titles take h1/h2
            out.push(format!("<h{level}>{}</h{level}>", inline(&h[2])));
        } else if !line.trim().is_empty() {
            out.push(format!("<p>{}</p>", inline(&line)));
        }
    }
    if list {
        out.push("</ul>".into());
    }
    out.join("\n")
}

/// encodeURIComponent.
fn encode_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!'
            | b'~' | b'*' | b'\'' | b'(' | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// reader.js proxiedAsset: is this image src inside the share's scope?
/// Returns the proxied path (relative to the reader page) or None.
fn proxied_asset(src: &str, link: &Value) -> Option<String> {
    static ASSET: OnceLock<regex::Regex> = OnceLock::new();
    let pairs: Vec<(&str, &str)> = link["pairs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            (p["origin"].as_str().unwrap_or(""), p["slug"].as_str().unwrap_or(""))
        })
        .collect();
    let base = pairs.first()?.0;
    // new URL(src, base) — the shapes that occur: absolute, host-relative,
    // path-relative. Query and fragment fall off the pathname either way.
    let (host, path) = if let Some(rest) = src
        .strip_prefix("http://")
        .or_else(|| src.strip_prefix("https://"))
    {
        let (hostport, path) = rest.split_once('/').unwrap_or((rest, ""));
        (
            hostport.split(':').next().unwrap_or("").to_string(),
            format!("/{path}"),
        )
    } else if let Some(rest) = src.strip_prefix("//") {
        let (hostport, path) = rest.split_once('/').unwrap_or((rest, ""));
        (
            hostport.split(':').next().unwrap_or("").to_string(),
            format!("/{path}"),
        )
    } else if src.starts_with('/') {
        (base.to_string(), src.to_string())
    } else {
        (base.to_string(), format!("/{src}"))
    };
    let path = path.split(['?', '#']).next().unwrap_or("").to_string();
    if !pairs.iter().any(|(o, _)| *o == host) {
        return None;
    }
    let m = re(r"^/assets/([a-z0-9-]+)/(.+)$", &ASSET).captures(&path)?;
    if !pairs.iter().any(|(_, s)| *s == &m[1]) {
        return None;
    }
    Some(format!(
        "{}/assets/{host}/{}/{}",
        encode_component(link["key"].as_str().unwrap_or("")),
        &m[1],
        &m[2]
    ))
}

fn render_item(item: &Value, link: &Value) -> String {
    let text = item["text"].as_str().unwrap_or("");
    match item["type"].as_str().unwrap_or("") {
        "paragraph" => format!("<p>{}</p>", inline(&esc(text))),
        "markdown" => render_markdown(text),
        "image" => {
            let src = proxied_asset(item["url"].as_str().unwrap_or(""), link);
            let caption = inline(&esc(
                item["caption"].as_str().filter(|s| !s.is_empty()).unwrap_or(text),
            ));
            match src {
                Some(src) => format!(
                    r#"<figure><img src="{}" alt=""><figcaption>{caption}</figcaption></figure>"#,
                    esc(&src)
                ),
                None => r#"<p class="omitted">[image not included in this share]</p>"#.into(),
            }
        }
        "html" => {
            r#"<p class="omitted">[interactive content not included in this share]</p>"#.into()
        }
        other => format!(
            r#"<p class="omitted">[{} item not included in this share]</p>"#,
            esc(other)
        ),
    }
}

fn render_page(f: &Fetched, link: &Value) -> String {
    let Some(page) = &f.page else {
        return format!(
            "<section><h2>{}</h2>\n      <p class=\"omitted\">[this page could not be fetched right now]</p></section>",
            esc(&f.slug)
        );
    };
    let story = page["story"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|item| render_item(item, link))
        .collect::<Vec<_>>()
        .join("\n");
    let title = page["title"].as_str().filter(|s| !s.is_empty()).unwrap_or(&f.slug);
    format!(
        "<section>\n    <h2>{}</h2>\n    <p class=\"origin\">{}</p>\n    {story}\n  </section>",
        esc(title),
        esc(&f.origin)
    )
}

fn render_reader(link: &Value, fetched: &[Fetched]) -> String {
    let expiry = if truthy(&link["expires"]) {
        let when = date_ms(&link["expires"])
            .map(to_utc_string)
            .unwrap_or_else(|| "Invalid Date".into());
        format!("This shared view expires {when}.")
    } else {
        "This shared view does not expire.".into()
    };
    let title = fetched
        .first()
        .and_then(|f| f.page.as_ref())
        .and_then(|p| p["title"].as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| link["title"].as_str().unwrap_or(""));
    let note = link["note"].as_str().filter(|s| !s.is_empty());
    let note_line = note
        .map(|n| format!("<p>{}</p>", inline(&esc(n))))
        .unwrap_or_default();
    let sections = fetched
        .iter()
        .map(|f| render_page(f, link))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="robots" content="noindex">
<title>{}</title>
<style>
  body {{ font: 18px/1.6 Georgia, serif; max-width: 38em; margin: 3em auto 6em; padding: 0 1em; color: #222; background: #fdfdfa; }}
  header {{ border-bottom: 1px solid #ddd; margin-bottom: 2em; padding-bottom: 1em; }}
  header p {{ color: #777; font-size: 80%; margin: 0.2em 0; }}
  section {{ margin-bottom: 3em; }}
  h2 {{ border-bottom: 1px solid #eee; padding-bottom: 0.2em; }}
  .origin {{ color: #999; font-size: 75%; margin-top: -0.8em; }}
  .wikilink {{ border-bottom: 1px dotted #aaa; color: #555; cursor: help; }}
  .omitted {{ color: #999; font-style: italic; font-size: 85%; }}
  figure {{ margin: 1em 0; }}
  img {{ max-width: 100%; }}
  figcaption {{ color: #777; font-size: 80%; }}
  code {{ background: #f2f2ec; padding: 0 3px; }}
  footer {{ color: #999; font-size: 75%; border-top: 1px solid #ddd; padding-top: 1em; }}
</style>
<header>
  <p>A read-only view shared from a federated wiki. {}</p>
  {}
</header>
{}
<footer><p>Shared via shortlink · dotted terms are pages not included in this share · don't panic.</p></footer>
</html>"#,
        esc(title),
        esc(&expiry),
        note_line,
        sections
    )
}

// -------------------------------------------------------------- the routes

fn body_response(status: u16, ctype: &str, body: impl Into<Body>) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap())
        .header(header::CONTENT_TYPE, ctype.to_owned())
        .body(body.into())
        .unwrap()
}

fn gone(status: u16) -> Response {
    body_response(status, "text/html; charset=utf-8", GONE_HTML)
}

fn too_many() -> Response {
    body_response(
        429,
        "text/plain; charset=utf-8",
        "Too many requests — slow down.",
    )
}

/// The route gate `^[a-z]+(-[a-z]+)*$` — one or more words, deliberately
/// looser than the 3-5 word mint format. Non-matching keys fall through
/// to the wiki's own routing; single words 404 at the store instead.
fn key_shape(key: &str) -> bool {
    !key.is_empty()
        && key
            .split('-')
            .all(|w| !w.is_empty() && w.bytes().all(|b| b.is_ascii_lowercase()))
}

/// Express 5 res.redirect(302, …): Location plus a content-negotiated
/// body — plain text by default, <p>-wrapped under Accept: text/html.
fn redirect_302(location: &str, accept: Option<&str>) -> Response {
    let wants_html = accept.map(|a| a.contains("html")).unwrap_or(false);
    let (ctype, body) = if wants_html {
        (
            "text/html; charset=utf-8",
            format!("<p>Found. Redirecting to {}</p>", esc(location)),
        )
    } else {
        (
            "text/plain; charset=utf-8",
            format!("Found. Redirecting to {location}"),
        )
    };
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, location.to_owned())
        .header(header::CONTENT_TYPE, ctype)
        .body(Body::from(body))
        .unwrap()
}

fn resolver_response(key: &str, ip: &str, accept: Option<&str>) -> Response {
    // evaluation order matters: allow() consumes a token even when the
    // breaker is already open (server.js:46)
    if !allow(ip) || breaker_open() {
        return too_many();
    }
    match resolve(key) {
        Err(e) => {
            // an internal error is dressed as an expired link, on purpose
            eprintln!("shortlink plugin: resolve failed — {e}");
            gone(500)
        }
        Ok(Resolved::Missing) => {
            record_failure();
            gone(404)
        }
        Ok(Resolved::Dead) => {
            record_failure();
            gone(410)
        }
        Ok(Resolved::Live { location, link }) => {
            if truthy(&link["proxy"]) {
                // Capability proxy: the read-only reader, never a redirect.
                // The single-use burn already happened inside resolve().
                let fetched = fetch_lineup(&link);
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .header("Content-Security-Policy", CSP)
                    .body(Body::from(render_reader(&link, &fetched)))
                    .unwrap()
            } else {
                redirect_302(&location, accept)
            }
        }
    }
}

/// Key-scoped asset proxy for the reader's images. Deliberately looser
/// than the page view on single-use: images load AFTER the page that
/// burned the link, so usedAt is NOT checked here (server.js:71-83).
fn asset_response(key: &str, origin: &str, slug: &str, rest: &str) -> Response {
    let link = store_get(key);
    let in_scope = link
        .as_ref()
        .map(|link| {
            truthy(&link["proxy"])
                && !truthy(&link["revoked"])
                && !(truthy(&link["expires"])
                    && date_ms(&link["expires"]).map(|e| e < now_ms()).unwrap_or(false))
                && link["pairs"].as_array().into_iter().flatten().any(|p| {
                    p["origin"].as_str() == Some(origin) && p["slug"].as_str() == Some(slug)
                })
        })
        .unwrap_or(false);
    if !in_scope {
        record_failure();
        return gone(404);
    }
    let url = format!("{}://{origin}/assets/{slug}/{rest}", scheme_for(origin));
    match http_get(&url) {
        Ok(up) if up.status == 200 => {
            // the upstream's content-type is echoed (charset added for
            // text types, like express res.type)
            let ctype = if up.ctype.starts_with("text/") && !up.ctype.contains("charset") {
                format!("{}; charset=utf-8", up.ctype)
            } else {
                up.ctype
            };
            body_response(200, &ctype, up.body)
        }
        Ok(_) => gone(404),
        Err(e) => {
            eprintln!("shortlink plugin: asset proxy failed — {e}");
            gone(502)
        }
    }
}

/// GET/HEAD under /plugin/shortlink/ — None falls through to the wiki's
/// own routing, exactly like the reference's next().
pub fn route(path: &str, ip: &str, accept: Option<&str>) -> Option<Response> {
    let rest = path.strip_prefix("/plugin/shortlink/")?;
    let segs: Vec<&str> = rest.split('/').collect();
    match segs.as_slice() {
        [key] | [key, ""] if key_shape(key) => Some(resolver_response(key, ip, accept)),
        // /:key/assets/:origin/:slug/*splat — a dot-segment in the splat
        // could escape /assets/<slug>/ upstream; rejected here, a recorded
        // candidate-only hardening (dossier TRAPS §5.14)
        [key, "assets", origin, slug, splat @ ..] if !splat.is_empty() => {
            let rest = splat.join("/");
            if splat.iter().any(|s| *s == ".." || *s == "." || s.contains("%2e") || s.contains("%2E"))
            {
                record_failure();
                return Some(gone(404));
            }
            Some(asset_response(key, origin, slug, &rest))
        }
        _ => None,
    }
}
