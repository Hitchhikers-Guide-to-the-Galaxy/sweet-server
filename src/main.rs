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
    /// a node_modules dir holding wiki-client and wiki-plugin-* packages;
    /// enables the native-hosted client (HTML shell, lineup, bundles)
    vendor: Option<PathBuf>,
    home: String,
}

#[tokio::main]
async fn main() {
    let mut data: Option<PathBuf> = None;
    let mut defaults: Option<PathBuf> = None;
    let mut vendor: Option<PathBuf> = None;
    let mut port: u16 = 4251;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data" => data = args.next().map(PathBuf::from),
            "--defaults" => defaults = args.next().map(PathBuf::from),
            "--vendor" => vendor = args.next().map(PathBuf::from),
            "--port" => port = args.next().and_then(|p| p.parse().ok()).unwrap_or(4251),
            _ => {}
        }
    }
    let site = Arc::new(Site {
        data: data.expect("--data <site-dir> is required"),
        defaults,
        vendor,
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
        "/system/plugins.json" => cors_json(plugin_names(site)),
        "/system/factories.json" => cors_json(factories(site)),
        "/sitemap.xml" => sitemap_xml(site),
        "/system/site-index.json" => match read(&site.data.join("status/site-index.json")) {
            Some(b) => json_bytes(b),
            None => not_found("Site index not found"),
        },
        p if p.starts_with("/assets/") => serve_file(site.data.join(&p[1..]), guess_mime(p)),
        p if p.starts_with("/theme/") => theme(site, p),
        p if p.starts_with("/plugins/") => plugin_static(site, p),
        p if p.ends_with(".json") && !p[1..].contains('/') => page_json(site, &p[1..p.len() - 5]),
        p if p.ends_with(".html") && !p[1..].contains('/') => {
            shell(site, &[("view".into(), p[1..p.len() - 5].to_string())])
        }
        p => {
            // lineup URLs: alternating /loc/slug pairs, e.g.
            // /view/welcome-visitors/localhost:4251/some-page (server.js:371)
            if let Some(pairs) = parse_lineup(p) {
                return shell(site, &pairs);
            }
            // wiki-client statics mounted at / (client.js, style/, js/, images/)
            if let Some(client) = client_dir(site) {
                let rel = &p[1..];
                if !rel.contains("..") {
                    if let Some(bytes) = read(&client.join(rel)) {
                        return serve_bytes(bytes, guess_mime(p));
                    }
                }
            }
            not_found("Page not found")
        }
    }
}

// ------------------------------------------------------- native client shell

fn client_dir(site: &Site) -> Option<PathBuf> {
    site.vendor.as_ref().map(|v| v.join("wiki-client/client"))
}

/// Split a path into (loc, slug) pairs per the lineup regex — every odd
/// segment a location (`view` or a site), every even one a slug.
fn parse_lineup(path: &str) -> Option<Vec<(String, String)>> {
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    if segs.len() < 2 || segs.len() % 2 != 0 {
        return None;
    }
    let mut pairs = Vec::new();
    for pair in segs.chunks(2) {
        let (loc, slug) = (pair[0], pair[1]);
        let loc_ok = loc
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ":.-".contains(c));
        let slug_ok = slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !loc_ok || !slug_ok || loc == "plugin" || loc == "auth" {
            return None;
        }
        pairs.push((loc.to_string(), slug.to_string()));
    }
    Some(pairs)
}

/// The static.html shell (wiki-client/views/static.html) with the page divs
/// interpolated — the client boots from here and fetches page JSON itself.
fn shell(site: &Site, pairs: &[(String, String)]) -> Response {
    if client_dir(site).is_none() {
        return not_found("Page not found"); // no vendor: read-only JSON server
    }
    let title = pairs
        .last()
        .map(|(_, s)| s.replace('-', " "))
        .unwrap_or_default();
    let owner = read(&site.data.join("status/owner.json"))
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|o| o["name"].as_str().map(String::from));
    let divs: String = pairs
        .iter()
        .map(|(loc, slug)| {
            let origin = if loc == "view" {
                String::new()
            } else {
                format!(" data-site={loc}")
            };
            format!(
                "      <div class='page' id='{slug}' tabindex=\"-1\"{origin}>\n        <div class='paper'>\n        </div>\n      </div>\n"
            )
        })
        .collect();
    let owner_footer = owner
        .as_deref()
        .map(|n| format!("Wiki by: <span id='site-owner'>{n}</span></div>"))
        .unwrap_or_default();
    let owned = owner.is_some();
    let body = format!(
        r#"<!DOCTYPE html>
<html class='no-js'>
  <head>
    <title>{title}</title>
    <meta content='text/html; charset=UTF-8' http-equiv='Content-Type'>
    <meta content='width=device-width, height=device-height, initial-scale=1.0, user-scalable=no' name='viewport'>
    <link id='favicon' href='/favicon.png' rel='icon' type='image/png'>
    <link href='/style/style.css' rel='stylesheet' type='text/css' media='screen'>
    <link href='/theme/style.css' rel='stylesheet' type='text/css' media='screen'>
    <link href='/style/print.css' rel='stylesheet' type='text/css' media='print'>
    <link href='/js/jquery-ui/1.14.2/jquery-ui.min.css' rel='stylesheet' type='text/css'>
    <script src='/js/jquery-4.0.0.js' type='text/javascript'></script>
    <script src='/js/jquery-migrate-4.0.2.js' type='text/javascript'></script>
    <script src='/js/jquery-ui/1.14.2/jquery-ui.min.js' type='text/javascript'></script>
    <script src='/js/jquery.ui.touch-punch.min.js' type='text/javascript'></script>
    <script src='/js/underscore-min.js' type='text/javascript'></script>
    <script src='/client.js' type='text/javascript'></script>
  </head>
  <body>
    <section class='main'>
{divs}    </section>
    <footer>
      <div id='site-owner' class='footer-item'>
        {owner_footer}
      <div id='security' class='footer-item'></div>
      <span class='searchbox' class='footer-item'>
        &nbsp;
        <input class='search' name='search' type='text' placeholder="Search">
        &nbsp;
        <span class='pages'></span>
      </span>
      <span class='neighborhood'></span>
    </footer>
  <script>
  var isAuthenticated = false;
  var isClaimed = {owned};
  var isOwner = false;
  var ownerName = '{owner_name}';
  var seedNeighbors = '';
  var user = '';
  wiki.security(user);
  </script>
  </body>
</html>"#,
        owner_name = owner.as_deref().unwrap_or("")
    );
    with_cors(
        Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Body::from(body))
            .unwrap(),
    )
}

fn plugin_packages(site: &Site) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    if let Some(vendor) = &site.vendor {
        if let Ok(entries) = std::fs::read_dir(vendor) {
            let mut dirs: Vec<_> = entries
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("wiki-plugin-"))
                .collect();
            dirs.sort_by_key(|e| e.file_name());
            for e in dirs {
                let name = e.file_name().to_string_lossy()[12..].to_string();
                out.push((name, e.path()));
            }
        }
    }
    out
}

fn plugin_names(site: &Site) -> Value {
    Value::Array(
        plugin_packages(site)
            .into_iter()
            .map(|(n, _)| Value::String(n))
            .collect(),
    )
}

fn factories(site: &Site) -> Value {
    Value::Array(
        plugin_packages(site)
            .into_iter()
            .filter_map(|(_, dir)| {
                read(&dir.join("factory.json"))
                    .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            })
            .collect(),
    )
}

/// /plugins/<name>/<file> from the vendored wiki-plugin-<name>/client dir
/// (server.js:328-335).
fn plugin_static(site: &Site, path: &str) -> Response {
    let rest = &path["/plugins/".len()..];
    let Some((name, file)) = rest.split_once('/') else {
        return not_found("Not found");
    };
    if file.contains("..") {
        return not_found("Not found");
    }
    match site.vendor.as_ref().and_then(|v| {
        read(&v.join(format!("wiki-plugin-{name}/client")).join(file))
    }) {
        Some(bytes) => serve_bytes(bytes, guess_mime(path)),
        None => not_found("Not found"),
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
