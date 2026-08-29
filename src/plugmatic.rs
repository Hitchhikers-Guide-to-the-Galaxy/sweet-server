//! Native port of wiki-plugin-plugmatic's server routes (0.8.x) — the
//! plugin browser behind the Browse All Plugins page. Read-only surface
//! implemented from the vendor tree; the admin actions (install, restart)
//! answer the reference's no-admin 403. Like the assets port, this replaces
//! a Node plugin server with sweet-native routes; the longer-term road is
//! the declared-interface door (see the Sweet Plugin Plan).
//!
//! Fidelity notes: the reference builds each plugin's `info` with
//! callback-style reads where a missing file leaves the field undefined —
//! JSON.stringify then DROPS the key — so absent files omit keys here too.
//! `bundle` is the reference's own offline fallback shape (it normally
//! fetches fedwiki/wiki's package.json from GitHub at boot).

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;
use serde_json::{json, Map, Value};
use std::path::Path;

fn json_response(status: StatusCode, value: Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(value.to_string()))
        .unwrap()
}

fn read_json(path: &Path) -> Option<Value> {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn bundle() -> Value {
    json!({"date": now_ms(), "data": {"dependencies": {}}})
}

/// One plugin's `info` record (reference server.js `info`): fields whose
/// backing file is missing are omitted, matching undefined-drop semantics.
fn info(short: &str, dir: &Path) -> Value {
    let file = format!("wiki-plugin-{short}");
    let mut site = Map::new();
    site.insert("plugin".into(), Value::String(short.into()));
    if let Ok(meta) = std::fs::metadata(dir.join("client").join(format!("{short}.js"))) {
        if let Ok(created) = meta.created() {
            if let Ok(d) = created.duration_since(std::time::UNIX_EPOCH) {
                // node's birthtimeMs rounds the nanosecond part to the
                // nearest millisecond; truncation reads one ms early
                let ms = d.as_secs() * 1000 + (d.subsec_nanos() as f64 / 1e6).round() as u64;
                site.insert("birth".into(), json!(ms));
            }
        }
    }
    if let Ok(authors) = std::fs::read_to_string(dir.join("AUTHORS.txt")) {
        site.insert("authors".into(), Value::String(authors));
    }
    if let Some(pkg) = read_json(&dir.join("package.json")) {
        site.insert("package".into(), pkg);
    }
    if let Some(factory) = read_json(&dir.join("factory.json")) {
        site.insert("factory".into(), factory);
    }
    let mut pages = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir.join("pages")) {
        let mut slugs: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        slugs.sort();
        for slug in slugs {
            let page = read_json(&dir.join("pages").join(&slug)).unwrap_or(Value::Null);
            let title = page["title"].as_str().unwrap_or(&slug).to_string();
            let synopsis = page["story"][0]["text"]
                .as_str()
                .or_else(|| page["story"][1]["text"].as_str())
                .unwrap_or("empty")
                .to_string();
            pages.push(json!({"file": file, "slug": slug, "title": title,
                              "synopsis": synopsis}));
        }
    }
    site.insert("pages".into(), Value::Array(pages));
    Value::Object(site)
}

/// GET /plugin/plugmatic/plugins — the BROWSE listing.
pub fn plugins(packages: &[(String, std::path::PathBuf)]) -> Response {
    let install: Vec<Value> = packages.iter().map(|(n, d)| info(n, d)).collect();
    json_response(StatusCode::OK, json!({"install": install, "bundle": bundle()}))
}

/// GET /plugin/plugmatic/sitemap.json — every plugin-shipped page, flat.
pub fn sitemap(packages: &[(String, std::path::PathBuf)]) -> Response {
    let mut out = Vec::new();
    for (n, d) in packages {
        let mut i = info(n, d);
        if let Value::Array(pages) = i["pages"].take() {
            out.extend(pages);
        }
    }
    json_response(StatusCode::OK, Value::Array(out))
}

/// GET /plugin/plugmatic/page/<slug>.json — first plugin shipping the slug.
pub fn page_by_slug(packages: &[(String, std::path::PathBuf)], slug: &str) -> Response {
    if slug.is_empty() || slug.contains(['/', '\\']) {
        return not_found();
    }
    for (_, dir) in packages {
        if let Some(page) = read_json(&dir.join("pages").join(slug)) {
            return json_response(StatusCode::OK, page);
        }
    }
    not_found()
}

/// GET /plugin/plugmatic/file/<file>/slug/<slug>.
pub fn file_slug(vendor: &Path, file: &str, slug: &str) -> Response {
    let clean = |s: &str| !s.is_empty() && !s.contains(['/', '\\']) && !s.contains("..");
    if !clean(file) || !clean(slug) {
        return not_found();
    }
    match read_json(&vendor.join(file).join("pages").join(slug)) {
        Some(page) => json_response(StatusCode::OK, page),
        None => not_found(),
    }
}

/// POST /plugin/plugmatic/plugins — detail for selected plugins:
/// installed info + npm registry view.
pub fn plugins_post(packages: &[(String, std::path::PathBuf)], body: &[u8]) -> Response {
    let requested: Vec<String> = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["plugins"].as_array().cloned())
        .map(|a| {
            a.into_iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let install: Vec<Value> = requested
        .iter()
        .filter_map(|name| {
            packages
                .iter()
                .find(|(n, _)| n == name)
                .map(|(n, d)| info(n, d))
        })
        .collect();
    let publish: Vec<Value> = requested.iter().map(|name| npm_view(name)).collect();
    json_response(
        StatusCode::OK,
        json!({"bundle": bundle(), "install": install, "publish": publish}),
    )
}

/// `npm view wiki-plugin-<name> --json` — the reference's view(); a failed
/// exec or unparsable output omits the npm key (undefined-drop).
fn npm_view(plugin: &str) -> Value {
    if !plugin.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || plugin.is_empty() {
        return Value::Null;
    }
    let pkg = format!("wiki-plugin-{plugin}");
    let mut out = Map::new();
    out.insert("plugin".into(), Value::String(plugin.into()));
    out.insert("pkg".into(), Value::String(pkg.clone()));
    if let Ok(o) = std::process::Command::new("npm")
        .args(["view", &pkg, "--json"])
        .output()
    {
        if let Ok(npm) = serde_json::from_slice::<Value>(&o.stdout) {
            out.insert("npm".into(), npm);
        }
    }
    Value::Object(out)
}

/// POST install / restart — admin-gated in the reference; with no admin
/// configured it answers this exact 403 (plugmatic server.js admin()).
pub fn admin_403() -> Response {
    json_response(
        StatusCode::FORBIDDEN,
        json!({"error": "service requires admin user",
               "admin": "none specified", "user": "not logged in"}),
    )
}

fn not_found() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from("Not Found"))
        .unwrap()
}
