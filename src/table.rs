//! wiki-plugin-table's plugin-served surface, ported natively — the road
//! the dossier recommends (plugin-port-v1, NATIVE): the json-plugin-parity
//! open read GET /plugin/table/{slug} (.csv variant included), the
//! X-Api-Key headless writer door PUT /plugin/table/{slug}, and the
//! farm-door `write` handler. The write handler lives here (not in the
//! sidecar) because core.savePage persists through the wiki's pagehandler
//! only — and sweet IS the pagehandler: one core, three writer stamps
//! (the key's id, 'farm-api', 'api'), exactly the reference's shape.
//!
//! Ported from wiki-plugin-table 0.5.0: server/server.js, server/core.js,
//! src/parse/parse.cjs. Behaviour pinned by
//! conformance tests/L7_api/plugins/test_table_probe.py.

use axum::{body::Body, http::header, http::StatusCode, response::Response};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::Site;

// Error strings are surface (dossier §5.16) — verbatim, punctuation and all.
const NO_TABLE: &str = "No wiki-plugin-table on this page.";
const BAD_KEY: &str = "Missing or invalid x-api-key in header";
const BAD_BODY: &str = "Body must be {columns, rows}, an array of rows, or {csv}";
const OVER_5MB: &str = "body over 5 MB";

const ALLOW_HEADERS: &str =
    "Origin, X-Requested-With, Content-Type, Accept, X-Api-Key";

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// JS truthiness (null, false, 0, "" are falsy).
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

// ------------------------------------------------ parse.cjs, the one parser

fn re(pattern: &str, cell: &'static OnceLock<regex::Regex>) -> &'static regex::Regex {
    cell.get_or_init(|| regex::Regex::new(pattern).unwrap())
}

/// String(v) — JS ToString on a JSON value (header cells, fromJson).
fn js_display(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => js_number(n),
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|x| if x.is_null() { String::new() } else { js_display(x) })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".into(),
    }
}

/// JS number-to-string: integers lose the trailing .0.
fn js_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e21 {
        format!("{}", f as i128)
    } else {
        format!("{f}")
    }
}

/// cellString: null/undefined -> '', objects -> JSON.stringify, else String.
fn cell_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Object(_) | Value::Array(_) => serde_json::to_string(v).unwrap_or_default(),
        other => js_display(other),
    }
}

#[derive(Default)]
struct Directives {
    caption: Option<String>,
    layout: Option<String>,
    set: usize, // how many directives were recorded (Object.keys().length)
}

const LAYOUTS: [&str; 3] = ["grid", "stack", "auto"];
const FOLDS: [&str; 3] = ["closed", "open", "none"];
const FITS: [&str; 2] = ["first", "even"];
const DIRECTIVE_WORDS: [&str; 6] = ["CAPTION", "LAYOUT", "SORT", "KEY", "FOLD", "FIT"];

fn split_lines(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(String::from)
        .collect()
}

/// takeDirectives: peel known uppercase directives off the top.
fn take_directives(lines: &[String]) -> (Directives, Vec<String>, Vec<String>) {
    static DIR: OnceLock<regex::Regex> = OnceLock::new();
    let dir_re = re(r"^([A-Z]+)(?:\s+(.*))?$", &DIR);
    let mut d = Directives::default();
    let mut warnings = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if line.is_empty() {
            if d.set > 0 {
                i += 1;
                continue; // blank after directives
            }
            break;
        }
        let Some(m) = dir_re.captures(line) else { break };
        let key = m.get(1).unwrap().as_str();
        if !DIRECTIVE_WORDS.contains(&key) {
            break;
        }
        let value = m.get(2).map(|v| v.as_str()).unwrap_or("");
        let vt = value.trim();
        match key {
            "CAPTION" => {
                d.caption = Some(vt.to_string());
                d.set += 1;
            }
            "LAYOUT" => {
                let v = vt.to_lowercase();
                if LAYOUTS.contains(&v.as_str()) {
                    d.layout = Some(v);
                    d.set += 1;
                } else {
                    warnings.push(format!("LAYOUT {vt} — expected grid, stack or auto"));
                }
            }
            "SORT" | "KEY" => d.set += 1, // recorded; unused by the server surface
            "FOLD" => {
                let v = vt.to_lowercase();
                if FOLDS.contains(&v.as_str()) {
                    d.set += 1;
                } else {
                    warnings.push(format!("FOLD {vt} — expected closed, open or none"));
                }
            }
            "FIT" => {
                let v = {
                    let lv = vt.to_lowercase();
                    if lv.is_empty() { "first".to_string() } else { lv }
                };
                if FITS.contains(&v.as_str()) {
                    d.set += 1;
                } else {
                    warnings.push(format!("FIT {vt} — expected first or even"));
                }
            }
            _ => unreachable!(),
        }
        i += 1;
    }
    (d, warnings, lines[i..].to_vec())
}

fn sniff_delimiter(line: &str) -> char {
    let tabs = line.matches('\t').count();
    let commas = line.matches(',').count();
    if tabs > commas {
        '\t'
    } else {
        ','
    }
}

fn parse_delimited(text: &str, delim: char) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if quoted {
            if c == '"' {
                if chars.get(i + 1) == Some(&'"') {
                    cell.push('"');
                    i += 1;
                } else {
                    quoted = false;
                }
            } else {
                cell.push(c);
            }
        } else if c == '"' {
            quoted = true;
        } else if c == delim {
            row.push(std::mem::take(&mut cell));
        } else if c == '\n' {
            row.push(std::mem::take(&mut cell));
            rows.push(std::mem::take(&mut row));
        } else {
            cell.push(c);
        }
        i += 1;
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows.into_iter()
        .map(|r| r.into_iter().map(|v| v.trim().to_string()).collect::<Vec<_>>())
        .filter(|r: &Vec<String>| r.iter().any(|v| !v.is_empty()))
        .collect()
}

fn is_separator(line: &str) -> bool {
    static SEP: OnceLock<regex::Regex> = OnceLock::new();
    re(
        r"^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?\s*$",
        &SEP,
    )
    .is_match(line)
}

fn split_pipes(line: &str) -> Vec<String> {
    let mut s = line.trim().to_string();
    if let Some(rest) = s.strip_prefix('|') {
        s = rest.to_string();
    }
    if s.ends_with('|') && !s.ends_with("\\|") {
        s.truncate(s.len() - 1);
    }
    let chars: Vec<char> = s.chars().collect();
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && chars.get(i + 1) == Some(&'|') {
            cell.push('|');
            i += 1;
        } else if chars[i] == '|' {
            cells.push(std::mem::take(&mut cell));
        } else {
            cell.push(chars[i]);
        }
        i += 1;
    }
    cells.push(cell);
    cells.into_iter().map(|c| c.trim().to_string()).collect()
}

fn parse_gfm(lines: &[String]) -> Vec<Vec<String>> {
    lines
        .iter()
        .filter(|l| !l.trim().is_empty() && !is_separator(l))
        .map(|l| split_pipes(l))
        .collect()
}

/// fromJson: a parsed JSON value as {columns, rows} of cell strings.
fn from_json(value: &Value) -> (Vec<String>, Vec<Vec<String>>) {
    match value {
        Value::Array(a) => {
            if a.is_empty() {
                return (Vec::new(), Vec::new());
            }
            if a[0].is_array() {
                let columns = a[0]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(js_display)
                    .collect();
                let rows = a[1..]
                    .iter()
                    .map(|r| {
                        r.as_array()
                            .map(|cells| cells.iter().map(cell_string).collect())
                            .unwrap_or_default()
                    })
                    .collect();
                return (columns, rows);
            }
            // array of row objects: column union in insertion order
            let mut columns: Vec<String> = Vec::new();
            for obj in a {
                for k in obj.as_object().into_iter().flat_map(|o| o.keys()) {
                    if !columns.contains(k) {
                        columns.push(k.clone());
                    }
                }
            }
            let rows = a
                .iter()
                .map(|obj| {
                    columns
                        .iter()
                        .map(|c| match obj.as_object() {
                            Some(o) => o.get(c).map(cell_string).unwrap_or_default(),
                            None => String::new(),
                        })
                        .collect()
                })
                .collect();
            (columns, rows)
        }
        Value::Object(o) => {
            let columns = o.get("columns").and_then(|c| c.as_array());
            let rows = o.get("rows").and_then(|r| r.as_array());
            if let (Some(cols), Some(rows)) = (columns, rows) {
                let columns: Vec<String> = cols.iter().map(js_display).collect();
                let rows = rows
                    .iter()
                    .map(|r| match r {
                        Value::Array(cells) => cells.iter().map(cell_string).collect(),
                        other => columns
                            .iter()
                            .map(|c| {
                                if truthy(other) {
                                    other
                                        .as_object()
                                        .and_then(|o| o.get(c))
                                        .map(cell_string)
                                        .unwrap_or_default()
                                } else {
                                    String::new()
                                }
                            })
                            .collect(),
                    })
                    .collect();
                return (columns, rows);
            }
            if let Some(rows) = o.get("rows").filter(|r| r.is_array()) {
                return from_json(rows);
            }
            // plain object → two-column key/value table
            (
                vec!["key".into(), "value".into()],
                o.iter().map(|(k, v)| vec![k.clone(), cell_string(v)]).collect(),
            )
        }
        _ => (Vec::new(), Vec::new()),
    }
}

struct Parsed {
    caption: Option<String>,
    layout: Option<String>,
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
    format: String,
    warnings: Vec<String>,
}

/// parse.cjs parse() — directives, format sniffing, ragged-row padding.
fn parse_text(text: &str) -> Parsed {
    let lines = split_lines(text);
    let (directives, mut warnings, rest) = take_directives(&lines);
    let body = rest.join("\n").trim().to_string();
    let mut columns: Vec<String> = Vec::new();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let format;
    if body.is_empty() {
        return Parsed {
            caption: directives.caption,
            layout: directives.layout,
            columns,
            rows,
            format: "empty".into(),
            warnings,
        };
    }
    if body.starts_with('[') || body.starts_with('{') {
        match serde_json::from_str::<Value>(&body) {
            Ok(v) => {
                let (c, r) = from_json(&v);
                columns = c;
                rows = r;
                format = "json".to_string();
            }
            Err(e) => {
                warnings.push(format!("JSON did not parse: {e}"));
                return Parsed {
                    caption: directives.caption,
                    layout: directives.layout,
                    columns: Vec::new(),
                    rows: Vec::new(),
                    format: "json".into(),
                    warnings,
                };
            }
        }
    } else if rest.iter().any(|l| is_separator(l)) && body.contains('|') {
        let table = parse_gfm(&rest);
        columns = table.first().cloned().unwrap_or_default();
        rows = table.into_iter().skip(1).collect();
        format = "gfm".to_string();
    } else {
        let first = rest
            .iter()
            .find(|l| !l.trim().is_empty())
            .map(String::as_str)
            .unwrap_or("");
        let delim = sniff_delimiter(first);
        let table = parse_delimited(&body, delim);
        columns = table.first().cloned().unwrap_or_default();
        rows = table.into_iter().skip(1).collect();
        format = if delim == '\t' { "tsv" } else { "csv" }.to_string();
    }
    // ragged rows: pad or truncate to the header, and say so once
    let width = columns.len();
    let mut ragged = false;
    rows = rows
        .into_iter()
        .map(|mut r| {
            if r.len() != width {
                ragged = true;
                r.resize(width, String::new());
            }
            r
        })
        .collect();
    if ragged {
        warnings
            .push("some rows did not match the header width and were padded or trimmed".into());
    }
    Parsed {
        caption: directives.caption,
        layout: directives.layout,
        columns,
        rows,
        format,
        warnings,
    }
}

/// fromResource: {columns, rows} | array | CSV/JSON string, or None.
fn from_resource(resource: &Value) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    match resource {
        Value::Null => None,
        Value::String(s) => {
            let p = parse_text(s);
            Some((p.columns, p.rows))
        }
        other => Some(from_json(other)),
    }
}

fn csv_cell(v: &str) -> String {
    if v.contains(['"', ',', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

fn to_csv(columns: &[String], rows: &[Vec<String>]) -> String {
    let mut lines = Vec::with_capacity(rows.len() + 1);
    lines.push(columns.iter().map(|c| csv_cell(c)).collect::<Vec<_>>().join(","));
    for r in rows {
        lines.push(r.iter().map(|c| csv_cell(c)).collect::<Vec<_>>().join(","));
    }
    lines.join("\n") + "\n"
}

// ---------------------------------------------------- core.js on page JSON

/// Index of the first table item on a page, or None — json-plugin semantics.
fn find_table_item(page: &Value) -> Option<usize> {
    page["story"]
        .as_array()?
        .iter()
        .position(|i| i["type"] == "table")
}

struct ReadTable {
    doc: Value, // the response body, keys in readTable's insertion order
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
}

/// readTable: the table as the client sees it.
fn read_table(item: &Value) -> ReadTable {
    let parsed = parse_text(item["text"].as_str().unwrap_or(""));
    let pushed = from_resource(item.get("resource").unwrap_or(&Value::Null));
    let use_resource = pushed
        .as_ref()
        .map(|(c, _)| !c.is_empty())
        .unwrap_or(false);
    let (columns, rows) = if use_resource {
        pushed.unwrap()
    } else {
        (parsed.columns, parsed.rows)
    };
    let book = |v: Option<&Value>| match v {
        Some(v) if truthy(v) => v.clone(),
        _ => Value::Null,
    };
    let mut doc = Map::new();
    doc.insert("columns".into(), json!(columns));
    doc.insert("rows".into(), json!(rows));
    doc.insert(
        "caption".into(),
        parsed
            .caption
            .filter(|c| !c.is_empty())
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    doc.insert(
        "layout".into(),
        Value::String(parsed.layout.unwrap_or_else(|| "auto".into())),
    );
    doc.insert(
        "format".into(),
        Value::String(if use_resource {
            "resource".into()
        } else {
            parsed.format
        }),
    );
    doc.insert(
        "source".into(),
        Value::String(if use_resource { "resource" } else { "text" }.into()),
    );
    doc.insert("warnings".into(), json!(parsed.warnings));
    doc.insert("writes".into(), match item.get("writes") {
        Some(v) if truthy(v) => v.clone(),
        _ => json!(0),
    });
    doc.insert("written".into(), book(item.get("written")));
    doc.insert("interval".into(), book(item.get("interval")));
    doc.insert("writer".into(), book(item.get("writer")));
    if let Some(id) = item.get("id") {
        doc.insert("id".into(), id.clone());
    }
    ReadTable {
        doc: Value::Object(doc),
        columns,
        rows,
    }
}

/// normaliseWrite: what a writer sent, or None (the exact refusals,
/// including the deliberate bare-object one).
fn normalise_write(body: &Value) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    match body {
        Value::Null => None,
        Value::String(s) => {
            let t = from_resource(&Value::String(s.clone()))?;
            if t.0.is_empty() {
                None
            } else {
                Some(t)
            }
        }
        Value::Object(o) if o.contains_key("csv") || o.contains_key("json") => {
            if let Some(c) = o.get("csv").and_then(|v| v.as_str()) {
                if !c.trim().is_empty() {
                    return normalise_write(&Value::String(c.to_string()));
                }
            }
            if let Some(j) = o.get("json").and_then(|v| v.as_str()) {
                if !j.trim().is_empty() {
                    return normalise_write(&Value::String(j.to_string()));
                }
            }
            None
        }
        Value::Object(o) => {
            // a table shape only — a bare object is not silently turned
            // into key/value rows here
            if !o.get("rows").map(|r| r.is_array()).unwrap_or(false) {
                return None;
            }
            let t = from_resource(body)?;
            if t.0.is_empty() {
                None
            } else {
                Some(t)
            }
        }
        Value::Array(_) => {
            let t = from_resource(body)?;
            if t.0.is_empty() {
                None
            } else {
                Some(t)
            }
        }
        _ => None,
    }
}

struct Written {
    writes: Value,
    interval: Option<Value>, // absent on a first write — res.json drops it
    resource_len: usize,     // UTF-16 code units of JSON.stringify(resource)
}

/// applyWrite: mutate the first table item in place with the json plugin's
/// bookkeeping. None when the page has no table item.
fn apply_write(
    page: &mut Value,
    slug: &str,
    columns: &[String],
    rows: &[Vec<String>],
    writer: &Value,
    now: i64,
) -> Option<Written> {
    let idx = find_table_item(page)?;
    let item = &mut page["story"][idx];
    let mut resource = Map::new();
    resource.insert("columns".into(), json!(columns));
    resource.insert("rows".into(), json!(rows));
    let resource = Value::Object(resource);
    let resource_len = serde_json::to_string(&resource)
        .unwrap_or_default()
        .encode_utf16()
        .count();
    item["resource"] = resource;
    let same_slug = item
        .get("slug")
        .map(|s| truthy(s) && s.as_str() == Some(slug))
        .unwrap_or(false);
    let counting = same_slug && item.get("writes").map(truthy).unwrap_or(false);
    if counting {
        let writes = item["writes"].as_i64().unwrap_or(0) + 1;
        item["writes"] = json!(writes);
        if item.get("written").map(truthy).unwrap_or(false) {
            let written = item["written"].as_i64().unwrap_or(0);
            item["interval"] = json!(now - written);
        }
    } else {
        item["slug"] = json!(slug);
        item["writes"] = json!(1);
        if let Some(o) = item.as_object_mut() {
            o.remove("interval"); // item.interval = undefined
        }
    }
    item["writer"] = writer.clone();
    item["written"] = json!(now);
    Some(Written {
        writes: item["writes"].clone(),
        interval: item.get("interval").cloned(),
        resource_len,
    })
}

// -------------------------------------------------------------- page IO
//
// The reference reaches pages through the site's pagehandler (the bridge);
// sweet IS the pagehandler — the same read chain the page route serves
// (site pages/, then default-data, then plugin-shipped pages), and a save
// that rewrites the page verbatim, no journal entry (dossier §5.13-14).

fn origin_of(site: &Site) -> String {
    site.data
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// core.loadPage, sweet-as-pagehandler. The 400 slug gate is unreachable
/// through the routes (their regex gates first); it exists for the farm
/// handler, whose slug arrives via query string.
fn load_page(site: &Site, slug: &str) -> Result<Value, (u16, String)> {
    let origin = origin_of(site);
    if slug.is_empty()
        || !slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err((400, "slug must be lowercase letters, digits and hyphens".into()));
    }
    let bytes = crate::load_page(site, slug)
        .or_else(|| crate::load_plugin_page(site, slug))
        .ok_or_else(|| (404, format!("no page {slug} on {origin}")))?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err((404, format!("no page {slug} on {origin}"))),
        Err(_) => Err((500, format!("page {slug} is not valid JSON"))),
    }
}

fn save_page(site: &Site, slug: &str, page: &Value) -> Result<(), (u16, String)> {
    std::fs::create_dir_all(site.data.join("pages"))
        .and_then(|()| {
            std::fs::write(
                site.data.join("pages").join(slug),
                serde_json::to_vec_pretty(page).unwrap_or_default(),
            )
        })
        .map_err(|e| (500, e.to_string()))?;
    crate::indexes::write_update(site, slug, Some(page));
    Ok(())
}

// ------------------------------------------------- key files (server.js)
//
// Per-site: status/api-keys.json (shared with the farm door's write gate)
// and the legacy status/plugin/table/tokens.json. Read on demand per
// request, cached by mtime — no restart to add, rotate or remove a key.
// Shallow Object.assign merge, legacy file LAST (it replaces colliding
// scopes wholesale).

type KeyCache = HashMap<PathBuf, (SystemTime, Option<Value>)>;

fn key_cache() -> &'static Mutex<KeyCache> {
    static CACHE: OnceLock<Mutex<KeyCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(KeyCache::new()))
}

fn load_keys(status_dir: &Path) -> Option<Value> {
    let files = [
        status_dir.join("api-keys.json"),
        status_dir.join("plugin/table/tokens.json"),
    ];
    let mut cache = key_cache().lock().unwrap();
    let mut merged: Option<Map<String, Value>> = None;
    for file in files {
        let Ok(mtime) = std::fs::metadata(&file).and_then(|m| m.modified()) else {
            cache.remove(&file);
            continue;
        };
        let hit = cache.get(&file).filter(|(m, _)| *m == mtime).cloned();
        let keys = match hit {
            Some((_, keys)) => keys,
            None => {
                let keys = match std::fs::read(&file)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                {
                    Some(v) => Some(v),
                    None => {
                        eprintln!("caution: {}: could not be parsed", file.display());
                        None
                    }
                };
                cache.insert(file.clone(), (mtime, keys.clone()));
                keys
            }
        };
        if let Some(k) = keys.filter(truthy) {
            let m = merged.get_or_insert_with(Map::new);
            for (scope, bucket) in k.as_object().into_iter().flatten() {
                m.insert(scope.clone(), bucket.clone());
            }
        }
    }
    merged.map(Value::Object)
}

/// Whether this site has opted in to headless writes at all.
fn writes_offered(status_dir: &Path) -> bool {
    load_keys(status_dir).is_some()
}

/// authFor: scope precedence slug -> table.write -> `*`; the key's value
/// is the auth record {id, nb}.
fn auth_for(status_dir: &Path, slug: &str, key: Option<&str>) -> Option<Value> {
    let t = load_keys(status_dir)?;
    let key = key?;
    for scope in [slug, "table.write", "*"] {
        let hit = &t[scope][key];
        if truthy(hit) {
            return Some(hit.clone());
        }
    }
    None
}

// ------------------------------------------------------------- responses

fn table_json(status: u16, body: Value) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, ALLOW_HEADERS)
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn csv_response(body: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv; charset=utf-8")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, ALLOW_HEADERS)
        .body(Body::from(body))
        .unwrap()
}

fn slug_ok(slug: &str) -> bool {
    !slug.is_empty()
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// GET /plugin/table/:slug and …/:slug.csv — the open read. None falls
/// through to the wiki's own routing, exactly like the reference's next().
pub fn get_route(site: &Site, path: &str) -> Option<Response> {
    let raw = path.strip_prefix("/plugin/table/")?;
    if raw.is_empty() || raw.contains('/') {
        return None;
    }
    // one trailing .csv strips, exactly one — x.csv.csv falls through
    let (slug, csv) = match raw.strip_suffix(".csv") {
        Some(s) => (s, true),
        None => (raw, false),
    };
    if !slug_ok(slug) {
        return None;
    }
    let page = match load_page(site, slug) {
        Ok(p) => p,
        Err((status, msg)) => {
            // GET error shape: {error, slug} — NO status member
            return Some(table_json(status, json!({"error": msg, "slug": slug})));
        }
    };
    let Some(idx) = find_table_item(&page) else {
        return Some(table_json(404, json!({"error": NO_TABLE, "slug": slug})));
    };
    let table = read_table(&page["story"][idx]);
    Some(if csv {
        csv_response(to_csv(&table.columns, &table.rows))
    } else {
        table_json(200, table.doc)
    })
}

/// PUT /plugin/table/:slug — the X-Api-Key writer door. Mounted, but dark
/// until the site opts in by writing a key file: without one the request
/// falls through (None) to the wiki's own 404 with no CORS header, and a
/// key file added later needs no restart (dossier §5.3-4).
pub fn put_route(
    site: &Site,
    path: &str,
    body: &[u8],
    content_type: Option<&str>,
    api_key: Option<&str>,
) -> Option<Response> {
    let raw = path.strip_prefix("/plugin/table/")?;
    if raw.is_empty() || raw.contains('/') {
        return None;
    }
    let status_dir = site.data.join("status");
    if !writes_offered(&status_dir) {
        return None; // the dark door — before cors, before the body
    }
    // textBody: text/* bodies over 5 MB of decoded code units answer 413
    // in the reader, BEFORE auth (dossier §5.10)
    let is_text = content_type
        .map(|t| t.trim().to_ascii_lowercase().starts_with("text/"))
        .unwrap_or(false);
    let text_body = is_text.then(|| String::from_utf8_lossy(body).into_owned());
    if let Some(t) = &text_body {
        if t.encode_utf16().count() > 5 * 1024 * 1024 {
            return Some(table_json(413, json!({"status": "error", "error": OVER_5MB})));
        }
    }
    // slug gate: no .csv stripping on PUT — t.csv falls through
    let slug = raw;
    if !slug_ok(slug) {
        return None;
    }
    let Some(auth) = auth_for(&status_dir, slug, api_key) else {
        return Some(table_json(401, json!({"status": "error", "error": BAD_KEY})));
    };
    let body_val: Value = if let Some(t) = text_body {
        Value::String(t)
    } else if content_type
        .map(|t| t.trim().to_ascii_lowercase().starts_with("application/json"))
        .unwrap_or(false)
    {
        serde_json::from_slice(body).unwrap_or(Value::Null)
    } else {
        Value::Null // no parsed body — normaliseWrite(undefined)
    };
    let Some((columns, rows)) = normalise_write(&body_val) else {
        return Some(table_json(400, json!({"status": "error", "error": BAD_BODY})));
    };
    let mut page = match load_page(site, slug) {
        Ok(p) => p,
        Err((status, msg)) => {
            return Some(table_json(
                status,
                json!({"status": "error", "error": msg, "slug": slug}),
            ));
        }
    };
    // writer is the KEY's id — not 'api', not the farm door's 'farm-api'
    let writer = match auth.get("id") {
        Some(id) => id.clone(),
        None => json!("api"),
    };
    let Some(w) = apply_write(&mut page, slug, &columns, &rows, &writer, now_ms()) else {
        return Some(table_json(
            404,
            json!({"status": "error", "error": NO_TABLE, "slug": slug}),
        ));
    };
    if let Err((status, msg)) = save_page(site, slug, &page) {
        return Some(table_json(
            status,
            json!({"status": "error", "error": msg, "slug": slug}),
        ));
    }
    let mut ok = Map::new();
    ok.insert("status".into(), json!("ok"));
    ok.insert("writes".into(), w.writes);
    if let Some(interval) = w.interval {
        ok.insert("interval".into(), interval); // dropped on a first write
    }
    ok.insert("length".into(), json!(w.resource_len));
    ok.insert("rows".into(), json!(rows.len()));
    ok.insert("columns".into(), json!(columns.len()));
    Some(table_json(200, Value::Object(ok)))
}

// ----------------------------------------- the farm-door write handler
//
// api/handlers.js write: request-blind, farm-gated (key -> place -> person
// runs in api_route before this). Served natively because core.savePage
// persists through the pagehandler only — which is sweet itself.

pub fn farm_write(site: &Site, args: &Value) -> Result<Value, (u16, String)> {
    let origin = origin_of(site);
    let slug = match args.get("slug") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(v) if !v.is_null() && v.as_str() != Some("") => js_display(v),
        _ => return Err((400, "slug is required".into())),
    };
    let body = match args.get("csv").and_then(|v| v.as_str()) {
        Some(c) if !c.is_empty() => json!({"csv": c}),
        _ => json!({"json": args.get("json").cloned().unwrap_or(Value::Null)}),
    };
    let Some((columns, rows)) = normalise_write(&body) else {
        return Err((400, "supply csv (a CSV/TSV/markdown table) or json ({columns, rows} or an array of rows) with at least a header".into()));
    };
    let mut page = load_page(site, &slug)?;
    if find_table_item(&page).is_none() {
        return Err((404, format!("no table item on {origin}/{slug}")));
    }
    let w = apply_write(&mut page, &slug, &columns, &rows, &json!("farm-api"), now_ms())
        .expect("table item just found");
    save_page(site, &slug, &page)?;
    let mut ok = Map::new();
    ok.insert("status".into(), json!("ok"));
    ok.insert("site".into(), json!(origin));
    ok.insert("slug".into(), json!(slug));
    ok.insert("writes".into(), w.writes);
    if let Some(interval) = w.interval {
        ok.insert("interval".into(), interval);
    }
    ok.insert("rows".into(), json!(rows.len()));
    ok.insert("columns".into(), json!(columns.len()));
    Ok(Value::Object(ok))
}
