//! Sweet Server — Phase 2: a read-only Federated Wiki server.
//!
//! Serves an existing site directory without writing to it: page JSON,
//! system endpoints, favicon, assets, theme. Behaviour follows the
//! Fedwiki Server Spec (wiki-server 0.27.0); parity is enforced by the
//! conformance harness in ~/Code/conformance (L0 first).
//!
//! Usage: sweet-server --data <site-dir> [--port 4251] [--defaults <dir>]
//! `--defaults` points at wiki-server's default-data dir, which supplies
//! the fallback pages (a missing slug may legitimately 200) and the
//! default favicon — spec §3 read fallback chain.

use axum::{
    body::Body,
    http::{header, HeaderValue, Request, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
struct Site {
    data: PathBuf,
    defaults: Option<PathBuf>,
    home: String,
}

#[tokio::main]
async fn main() {
    let mut data: Option<PathBuf> = None;
    let mut defaults: Option<PathBuf> = None;
    let mut port: u16 = 4251;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data" => data = args.next().map(PathBuf::from),
            "--defaults" => defaults = args.next().map(PathBuf::from),
            "--port" => port = args.next().and_then(|p| p.parse().ok()).unwrap_or(4251),
            _ => {}
        }
    }
    let site = Arc::new(Site {
        data: data.expect("--data <site-dir> is required"),
        defaults,
        home: "welcome-visitors".into(),
    });

    let app = axum::Router::new().fallback({
        let site = site.clone();
        move |req: Request<Body>| {
            let site = site.clone();
            async move { route(&site, req.uri().path()) }
        }
    });

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    eprintln!("sweet-server (read-only) on http://{addr} serving {:?}", site.data);
    axum::serve(listener, app).await.unwrap();
}

fn route(site: &Site, path: &str) -> Response {
    match path {
        "/" => Redirect::temporary(&format!("/{}.html", site.home)).into_response(),
        "/favicon.png" => favicon(site),
        "/system/slugs.json" => cors_json(slugs(site)),
        "/system/sitemap.json" => cors_json(sitemap(site)),
        "/sitemap.xml" => sitemap_xml(site),
        "/system/site-index.json" => match read(&site.data.join("status/site-index.json")) {
            Some(b) => json_bytes(b),
            None => not_found("Site index not found"),
        },
        p if p.starts_with("/assets/") => serve_file(site.data.join(&p[1..]), guess_mime(p)),
        p if p.starts_with("/theme/") => theme(site, p),
        p if p.ends_with(".json") && !p[1..].contains('/') => page_json(site, &p[1..p.len() - 5]),
        _ => not_found("Page not found"),
    }
}

// ---------------------------------------------------------------- pages

/// Read fallback chain, spec §3: site pages/ then default-data pages/.
fn load_page(site: &Site, slug: &str) -> Option<Vec<u8>> {
    if slug.is_empty() || slug.contains(['/', '.', '\\']) {
        return None;
    }
    read(&site.data.join("pages").join(slug)).or_else(|| {
        site.defaults
            .as_ref()
            .and_then(|d| read(&d.join("pages").join(slug)))
    })
}

fn page_json(site: &Site, slug: &str) -> Response {
    match load_page(site, slug) {
        // parse to guarantee we never serve a corrupt page as 200 —
        // spec: unparseable page is a 404, not a garbled body
        Some(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(_) => json_bytes(bytes),
            Err(_) => not_found("Error Parsing Page"),
        },
        None => not_found("Page not found"),
    }
}

// ---------------------------------------------------------------- system

fn site_pages(site: &Site) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(site.data.join("pages")) {
        let mut names: Vec<_> = entries
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for slug in names {
            if let Some(bytes) = read(&site.data.join("pages").join(&slug)) {
                if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                    out.push((slug, v));
                }
            }
        }
    }
    out
}

fn slugs(site: &Site) -> Value {
    Value::Array(
        site_pages(site)
            .into_iter()
            .map(|(slug, page)| json!({"slug": slug, "title": page["title"]}))
            .collect(),
    )
}

/// Sitemap entry per spec §8: {slug, title, date, synopsis, links}.
/// Serves the prebuilt status file when present, else derives from pages.
fn sitemap(site: &Site) -> Value {
    if let Some(bytes) = read(&site.data.join("status/sitemap.json")) {
        if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
            return v;
        }
    }
    Value::Array(
        site_pages(site)
            .into_iter()
            .map(|(slug, page)| {
                let mut entry = json!({
                    "slug": slug,
                    "title": page["title"],
                    "date": last_edit_date(&page),
                    "synopsis": synopsis(&page),
                });
                let links = links(&page);
                if !links.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                    entry["links"] = links;
                }
                entry
            })
            .collect(),
    )
}

/// Date of the last non-fork journal entry (lib/sitemap.js:44-48).
fn last_edit_date(page: &Value) -> Value {
    page["journal"]
        .as_array()
        .and_then(|j| {
            j.iter()
                .rev()
                .find(|e| e["type"] != "fork")
                .or_else(|| j.last())
        })
        .map(|e| e["date"].clone())
        .unwrap_or(Value::Null)
}

/// First textual item's text — mirrors wiki-client synopsis.js closely
/// enough for L0; exact parity is a diff-mode concern.
fn synopsis(page: &Value) -> Value {
    if let Some(story) = page["story"].as_array() {
        for item in story {
            if matches!(item["type"].as_str(), Some("paragraph") | Some("markdown")) {
                if let Some(t) = item["text"].as_str() {
                    return Value::String(t.chars().take(560).collect());
                }
            }
        }
    }
    Value::String(format!(
        "A page with {} items.",
        page["story"].as_array().map(|s| s.len()).unwrap_or(0)
    ))
}

/// links maps each [[coined]] slug to the id of the item that coined it
/// (first occurrence wins) — lib/sitemap.js:54-95.
fn links(page: &Value) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(story) = page["story"].as_array() {
        for item in story {
            let (Some(text), Some(id)) = (item["text"].as_str(), item["id"].as_str()) else {
                continue;
            };
            let mut rest = text;
            while let Some(start) = rest.find("[[") {
                let Some(end) = rest[start + 2..].find("]]") else { break };
                let title = &rest[start + 2..start + 2 + end];
                let slug = as_slug(title);
                if !slug.is_empty() {
                    map.entry(slug).or_insert_with(|| Value::String(id.into()));
                }
                rest = &rest[start + 2 + end + 2..];
            }
        }
    }
    Value::Object(map)
}

fn as_slug(title: &str) -> String {
    title
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>()
        .to_lowercase()
}

fn sitemap_xml(site: &Site) -> Response {
    let urls: String = site_pages(site)
        .iter()
        .map(|(slug, _)| format!("<url><loc>/{slug}.html</loc></url>"))
        .collect();
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">{urls}</urlset>"
    );
    with_cors(
        Response::builder()
            .header(header::CONTENT_TYPE, "application/xml")
            .body(Body::from(body))
            .unwrap(),
    )
}

// ---------------------------------------------------------------- statics

fn favicon(site: &Site) -> Response {
    let local = site.data.join("status/favicon.png");
    let fallback = site
        .defaults
        .as_ref()
        .map(|d| d.join("status/favicon.png"));
    match read(&local).or_else(|| fallback.and_then(|f| read(&f))) {
        Some(bytes) => with_cors(
            Response::builder()
                .header(header::CONTENT_TYPE, "image/png")
                .body(Body::from(bytes))
                .unwrap(),
        ),
        None => not_found("Favicon not found"),
    }
}

/// /theme/style.css returns an empty 200 when the site has no theme —
/// spec QUIRK (server.js:502).
fn theme(site: &Site, path: &str) -> Response {
    let file = site.data.join("status/theme").join(&path[7..]);
    match read(&file) {
        Some(bytes) => serve_bytes(bytes, guess_mime(path)),
        None if path == "/theme/style.css" => serve_bytes(Vec::new(), "text/css"),
        None => not_found("Not found"),
    }
}

fn serve_file(path: PathBuf, mime: &str) -> Response {
    match read(&path) {
        Some(bytes) => serve_bytes(bytes, mime),
        None => not_found("Not found"),
    }
}

fn guess_mime(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("css") => "text/css",
        Some("js") | Some("mjs") => "text/javascript",
        Some("json") => "application/json",
        Some("html") => "text/html",
        Some("svg") => "image/svg+xml",
        Some("mp3") => "audio/mpeg",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------- plumbing

fn read(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

fn with_cors(mut resp: Response) -> Response {
    resp.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    resp
}

fn cors_json(v: Value) -> Response {
    json_bytes(serde_json::to_vec(&v).unwrap())
}

fn json_bytes(bytes: Vec<u8>) -> Response {
    with_cors(
        Response::builder()
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Body::from(bytes))
            .unwrap(),
    )
}

fn serve_bytes(bytes: Vec<u8>, mime: &str) -> Response {
    with_cors(
        Response::builder()
            .header(header::CONTENT_TYPE, mime.to_owned())
            .body(Body::from(bytes))
            .unwrap(),
    )
}

/// Plain-text 404 with the exact reference body — the harness asserts
/// the string "Page not found" verbatim.
fn not_found(msg: &'static str) -> Response {
    (StatusCode::NOT_FOUND, msg).into_response()
}
