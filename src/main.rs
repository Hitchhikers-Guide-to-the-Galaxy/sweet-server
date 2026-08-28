//! Sweet Server — a Federated Wiki server in Rust.
//!
//! Serves a site directory: page JSON, system endpoints, favicon, assets,
//! theme, the native-hosted client, the six-verb write path, friends
//! security, and the assets plugin routes. Behaviour follows the Fedwiki
//! Server Spec (wiki-server 0.27.0); parity is enforced by the conformance
//! harness in ~/Code/conformance (green through L4).
//!
//! Usage: sweet-server --data <site-dir> [--port 4251] [--host 127.0.0.1]
//!        [--defaults <dir>] [--vendor <node_modules>]
//!        [--security_legacy | --security_type friends]
//! `--defaults` points at wiki-server's default-data dir, which supplies
//! the fallback pages (a missing slug may legitimately 200) and the
//! default favicon — spec §3 read fallback chain.

use axum::{
    body::Body,
    http::{header, HeaderValue, Request, StatusCode},
    response::Response,
};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod actions;
mod assets;
mod search;
mod security;

struct Site {
    data: PathBuf,
    defaults: Option<PathBuf>,
    /// a node_modules dir holding wiki-client and wiki-plugin-* packages;
    /// enables the native-hosted client (HTML shell, lineup, bundles)
    vendor: Option<PathBuf>,
    home: String,
    security: security::Security,
}

#[tokio::main]
async fn main() {
    let mut data: Option<PathBuf> = None;
    let mut defaults: Option<PathBuf> = None;
    let mut vendor: Option<PathBuf> = None;
    let mut port: u16 = 4251;
    let mut host: std::net::IpAddr = [127, 0, 0, 1].into();
    let mut security_type: Option<String> = None;
    let mut security_legacy = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--data" => data = args.next().map(PathBuf::from),
            "--defaults" => defaults = args.next().map(PathBuf::from),
            "--vendor" => vendor = args.next().map(PathBuf::from),
            "--port" => port = args.next().and_then(|p| p.parse().ok()).unwrap_or(4251),
            "--host" => {
                host = args
                    .next()
                    .and_then(|h| h.parse().ok())
                    .expect("--host <ip> (e.g. 0.0.0.0)")
            }
            "--security_type" => security_type = args.next(),
            "--security_legacy" => security_legacy = true,
            _ => {}
        }
    }
    let data = data.expect("--data <site-dir> is required");
    // mode selection mirrors the reference CLI: --security_type wins,
    // --security_legacy softens the default stub, else read-only (spec §6.2)
    let mode = match security_type.as_deref() {
        Some(t) if t.contains("friends") => security::Mode::Friends,
        Some(other) => panic!("unsupported security_type {other:?} (only friends)"),
        None if security_legacy => security::Mode::Legacy,
        None => security::Mode::Default,
    };
    // multer's dest option creates assets/uploads at plugin startup — the
    // assets list route then answers {error:null, files:[]} on a fresh site
    let _ = std::fs::create_dir_all(data.join("assets/uploads"));
    let sec = security::Security::new(mode, &data.join("status/owner.json"));
    let site = Arc::new(Site {
        data,
        defaults,
        vendor,
        home: "welcome-visitors".into(),
        security: sec,
    });

    let app = axum::Router::new().fallback({
        let site = site.clone();
        move |req: Request<Body>| {
            let site = site.clone();
            async move {
                let method = req.method().clone();
                let path = req.uri().path().to_string();
                let query = req.uri().query().unwrap_or("").to_string();
                fn header_str(req: &Request<Body>, name: header::HeaderName) -> Option<String> {
                    req.headers()
                        .get(name)
                        .and_then(|v| v.to_str().ok())
                        .map(String::from)
                }
                let cookie = header_str(&req, header::COOKIE);
                let content_type = header_str(&req, header::CONTENT_TYPE);
                let req_host = header_str(&req, header::HOST)
                    .unwrap_or_else(|| "localhost".into());
                let body = axum::body::to_bytes(req.into_body(), 8 * 1024 * 1024)
                    .await
                    .unwrap_or_default();
                let authorized = site.security.is_authorized(cookie.as_deref());
                let assets_root = site.data.join("assets");
                match (method.as_str(), path.as_str()) {
                    // friends security routes (spec §6.3)
                    ("POST", "/login") => {
                        let _ = std::fs::create_dir_all(site.data.join("status"));
                        site.security.login(&site.data.join("status/owner.json"))
                    }
                    ("GET" | "HEAD", "/logout") => site.security.logout(cookie.as_deref()),
                    ("POST", "/auth/reclaim/") => site.security.reclaim(&body),
                    // assets plugin routes (wiki-plugin-assets 0.7.4)
                    ("GET" | "HEAD", "/plugin/assets/list") => assets::list(&assets_root, &query),
                    ("GET" | "HEAD", "/plugin/assets/index") => assets::index(&assets_root),
                    ("POST", "/plugin/assets/upload") => {
                        if !authorized {
                            return must_be_owner();
                        }
                        assets::upload(&assets_root, content_type.as_deref(), body).await
                    }
                    ("POST", "/plugin/assets/delete") => {
                        if !authorized {
                            return must_be_owner();
                        }
                        assets::delete(&assets_root, &query)
                    }
                    ("GET" | "HEAD", _) => {
                        let auth = Auth {
                            authorized,
                            user: site.security.user(cookie.as_deref()),
                            host: req_host,
                        };
                        route(&site, &path, auth)
                    }
                    ("PUT" | "DELETE", _) => {
                        write_route(&site, method.as_str(), &path, &body, authorized)
                    }
                    _ => not_found("Page not found"),
                }
            }
        }
    });

    let addr = SocketAddr::from((host, port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    eprintln!("sweet-server (read-only) on http://{addr} serving {:?}", site.data);
    axum::serve(listener, app).await.unwrap();
}

/// Per-request facts the read routes need: recycler gating, the shell's
/// owner variables, and the Host header (absolute urls in sitemap.xml).
#[derive(Clone)]
struct Auth {
    authorized: bool,
    user: bool,
    host: String,
}

fn must_be_owner() -> Response {
    // the assets plugin's own gate (server.js bundle: status(403).send)
    Response::builder()
        .status(StatusCode::FORBIDDEN)
        .body(Body::from("must be owner"))
        .unwrap()
}

/// PUT /page/:slug/action, DELETE /:slug.json, DELETE /recycler/:slug.json
/// — the L2 write surface (server.js:847,986,628). Auth gates AFTER the
/// route pattern matches, like express middleware: an unmatched path is
/// 404 whoever asks; a matched one is 403 without authority.
fn write_route(site: &Site, method: &str, path: &str, body: &[u8], authorized: bool) -> Response {
    // express res.send(string) stamps text/html — but the error helper
    // res.e finishes with res.end(), which sets NO content-type. Every
    // res.e body in the action route starts "Server " (server.js:850-900).
    let plain = |status: u16, text: String| {
        let mut b = Response::builder()
            .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
        if !text.starts_with("Server ") {
            b = b.header(header::CONTENT_TYPE, "text/html; charset=utf-8");
        }
        b.body(Body::from(text)).unwrap()
    };
    match method {
        "PUT" => {
            let Some(slug) = path
                .strip_prefix("/page/")
                .and_then(|r| r.strip_suffix("/action"))
                .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
            else {
                return not_found("Page not found");
            };
            if !authorized {
                return security::forbidden();
            }
            let Some(action) = actions::parse_action(body) else {
                return plain(500, "Server bad action".into());
            };
            match actions::apply(site, slug, action) {
                actions::Outcome::Ok => plain(200, "ok".into()),
                actions::Outcome::Error(status, text) => plain(status, text),
            }
        }
        "DELETE" => {
            if let Some(slug) = path
                .strip_prefix("/recycler/")
                .and_then(|r| r.strip_suffix(".json"))
            {
                if !authorized {
                    return security::forbidden();
                }
                return match actions::recycler_purge(site, slug) {
                    Ok(()) => plain(200, String::new()),
                    Err((status, text)) => plain(status, text),
                };
            }
            if let Some(slug) = path.strip_prefix("/").and_then(|r| r.strip_suffix(".json")) {
                if !slug.contains('/') {
                    if !authorized {
                        return security::forbidden();
                    }
                    return match actions::delete_page(site, slug) {
                        Ok(()) => plain(200, String::new()),
                        Err((status, text)) => plain(status, text),
                    };
                }
            }
            not_found("Page not found")
        }
        _ => not_found("Page not found"),
    }
}

fn route(site: &Site, path: &str, auth: Auth) -> Response {
    match path {
        // 302, like express res.redirect; /assets/home/index.html wins if present
        "/" => {
            let loc = if site.data.join("assets/home/index.html").is_file() {
                "/assets/home/index.html".to_string()
            } else {
                format!("/{}.html", site.home)
            };
            Response::builder()
                .status(StatusCode::FOUND)
                .header(header::LOCATION, loc)
                .body(Body::empty())
                .unwrap()
        }
        "/favicon.png" => favicon(site),
        "/system/slugs.json" => cors_json(slugs(site)),
        // Last-Modified is load-bearing: the client's neighborhood accepts a
        // sitemap only when Date.parse(Last-Modified) beats its cached value
        // (wiki-client neighborhood.js refreshMap) — omit it and search dies
        "/system/sitemap.json" => {
            let map = sitemap(site);
            with_last_modified(cors_json(map.clone()), max_date(&map))
        }
        "/system/plugins.json" => cors_json(plugin_names(site)),
        "/system/factories.json" => cors_json(factories(site)),
        "/sitemap.xml" => sitemap_xml(site, &auth.host),
        "/system/site-index.json" => {
            let idx = match read(&site.data.join("status/site-index.json"))
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            {
                Some(v) => v,
                None => search::site_index(&site_pages(site)),
            };
            let stamp = max_date(&sitemap(site));
            with_last_modified(cors_json(idx), stamp)
        }
        "/system/export.json" => {
            let mut map = serde_json::Map::new();
            for (slug, page) in site_pages(site) {
                map.insert(slug, page);
            }
            cors_json(Value::Object(map))
        }
        p if p.starts_with("/remote/") => remote_proxy(p),
        // recycler reads ride the same authorized middleware as writes
        // (server.js:577,582,617)
        "/recycler/system/slugs.json" => {
            if !auth.authorized {
                return security::forbidden();
            }
            // quirk kept: a site that never recycled has no recycle dir and
            // the reference's readdir errors out as a 500 (server.js:583-586)
            let recycle = site.data.join("recycle");
            if !recycle.is_dir() {
                return Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .body(Body::from(format!(
                        "Server Error: ENOENT: no such file or directory, scandir '{}'",
                        recycle.display()
                    )))
                    .unwrap();
            }
            cors_json(actions::recycler_slugs(site))
        }
        p if p.starts_with("/recycler/") && p.ends_with(".json") => {
            if !auth.authorized {
                return security::forbidden();
            }
            match actions::recycler_page(site, &p["/recycler/".len()..p.len() - 5]) {
                Some(bytes) => json_bytes(bytes),
                None => not_found("Page not found"),
            }
        }
        p if p.starts_with("/assets/") => serve_file(site.data.join(&p[1..]), guess_mime(p)),
        p if p.starts_with("/theme/") => theme(site, p),
        p if p.starts_with("/plugins/") => plugin_static(site, p),
        // security module's client (server.js:337-340; friends by default)
        p if p.starts_with("/security/") => {
            let file = &p["/security/".len()..];
            if file.contains("..") {
                return not_found("Not found");
            }
            match site
                .vendor
                .as_ref()
                .and_then(|v| read(&v.join("wiki-security-friends/client").join(file)))
            {
                Some(bytes) => serve_bytes(bytes, guess_mime(p)),
                None => not_found("Not found"),
            }
        }
        p if p.ends_with(".json") && !p[1..].contains('/') => page_json(site, &p[1..p.len() - 5]),
        p if p.ends_with(".html") && !p[1..].contains('/') => {
            shell(site, &[("view".into(), p[1..p.len() - 5].to_string())], auth)
        }
        p => {
            // lineup URLs: alternating /loc/slug pairs, e.g.
            // /view/welcome-visitors/localhost:4251/some-page (server.js:371)
            if let Some(pairs) = parse_lineup(p) {
                return shell(site, &pairs, auth);
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

/// GET /remote/:site/:slug.json — proxy a remote page over plain http with
/// a short timeout, exactly the reference's shape (server.js:489, 2s).
fn remote_proxy(path: &str) -> Response {
    let rest = &path["/remote/".len()..];
    let Some((remote_site, file)) = rest.split_once('/') else {
        return not_found("Page not found");
    };
    if !file.ends_with(".json") || file.contains('/') {
        return not_found("Page not found");
    }
    let url = format!("http://{remote_site}/{file}");
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(2))
        .build();
    match agent.get(&url).call() {
        Ok(resp) => {
            let mut body = Vec::new();
            use std::io::Read;
            if resp.into_reader().read_to_end(&mut body).is_ok() {
                json_bytes(body)
            } else {
                not_found("Page not found")
            }
        }
        Err(_) => not_found("Page not found"),
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
fn shell(site: &Site, pairs: &[(String, String)], auth: Auth) -> Response {
    if client_dir(site).is_none() {
        return not_found("Page not found"); // no vendor: read-only JSON server
    }
    let title = pairs
        .last()
        .map(|(_, s)| s.replace('-', " "))
        .unwrap_or_default();
    let owner = site.security.owner_name();
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
    // the script block mirrors static.html's mustache output exactly:
    // var isOwner = ('{{isOwner}}' === "true") with booleans rendered as
    // "true"/"false" strings. isOwner must be truthful (server.js:392) —
    // when false the client routes edits to localStorage instead of PUTting
    // them (wiki-client pageHandler.useLocalStorage, pinned finding)
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
      </div>
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
  var isAuthenticated = ('{authenticated}' === "true");
  var isClaimed = ('{owned}' === "true");
  var isOwner = ('{is_owner}' === "true");
  var ownerName = '{owner_name}';
  var seedNeighbors = '';
  var user = '{user}'
  wiki.security(user);
  </script>
  </body>
</html>"#,
        authenticated = auth.user,
        is_owner = auth.authorized,
        user = if auth.user { "true" } else { "" },
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
    // an array of bare slug strings (parity sweep vs 0.40.2)
    Value::Array(
        site_pages(site)
            .into_iter()
            .map(|(slug, _)| Value::String(slug))
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
/// wiki-client/lib/synopsis.js verbatim: the synopsis field, else the
/// first/second item — paragraphs preferred over anything with text —
/// else a count; then first line only, capped at 560.
fn synopsis(page: &Value) -> Value {
    let text = (|| {
        if let Some(s) = page["synopsis"].as_str() {
            return s.to_string();
        }
        let Some(story) = page["story"].as_array() else {
            return "A page with no story.".into();
        };
        let get = |i: usize| story.get(i);
        let para = |i: usize| {
            get(i)
                .filter(|it| it["type"] == "paragraph")
                .and_then(|it| it["text"].as_str())
        };
        let any_text = |i: usize| get(i).and_then(|it| it["text"].as_str());
        para(0)
            .or_else(|| para(1))
            .or_else(|| any_text(0))
            .or_else(|| any_text(1))
            .map(String::from)
            .unwrap_or_else(|| format!("A page with {} items.", story.len()))
    })();
    let first_line = text.trim().split(['\r', '\n']).next().unwrap_or("");
    Value::String(first_line.chars().take(560).collect())
}

/// links maps each [[coined]] slug to the id of the item that coined it
/// (first occurrence wins) — lib/sitemap.js:54-95.
fn links(page: &Value) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(story) = page["story"].as_array() {
        for item in story {
            let Some(id) = item["id"].as_str() else { continue };
            // a reference item links to its target by slug (sitemap.js:64-68)
            if item["type"] == "reference" {
                if let Some(slug) = item["slug"].as_str() {
                    map.entry(slug.to_string())
                        .or_insert_with(|| Value::String(id.into()));
                }
            }
            let Some(text) = item["text"].as_str() else {
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

fn sitemap_xml(site: &Site, host: &str) -> Response {
    // the reference's exact shape: standalone declaration, two-space
    // indent, absolute http://<host> locs, lastmod = entry date YYYY-MM-DD
    let urls: String = sitemap(site)
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    let slug = e["slug"].as_str()?;
                    let lastmod = e["date"]
                        .as_u64()
                        .map(|ms| {
                            format!("\n    <lastmod>{}</lastmod>", ymd(ms))
                        })
                        .unwrap_or_default();
                    Some(format!(
                        "\n  <url>\n    <loc>http://{host}/{slug}.html</loc>{lastmod}\n  </url>"
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<urlset xmlns=\"http://www.sitemaps.org/schemas/sitemap/0.9\">{urls}\n</urlset>"
    );
    with_cors(
        Response::builder()
            .header(header::CONTENT_TYPE, "application/xml")
            .body(Body::from(body))
            .unwrap(),
    )
}

/// ms-epoch -> UTC YYYY-MM-DD (Date.toISOString().substring(0,10))
fn ymd(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let mut y = 1970i64;
    let mut d = days;
    loop {
        let len = if leap(y) { 366 } else { 365 };
        if d < len {
            break;
        }
        d -= len;
        y += 1;
    }
    let months = [
        31,
        if leap(y) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut m = 0usize;
    while d >= months[m] {
        d -= months[m];
        m += 1;
    }
    format!("{y:04}-{:02}-{:02}", m + 1, d + 1)
}

fn leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
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

fn max_date(sitemap: &Value) -> u64 {
    sitemap
        .as_array()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| e["date"].as_u64())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// RFC 7231 IMF-fixdate from epoch milliseconds, no external crates.
fn http_date(ms: u64) -> String {
    let secs = ms / 1000;
    let (s, m, h) = (secs % 60, (secs / 60) % 60, (secs / 3600) % 24);
    let days = (secs / 86400) as i64;
    // Howard Hinnant's civil_from_days
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    let weekday = (days + 4).rem_euclid(7) as usize; // 1970-01-01 was a Thursday
    const WD: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MO: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun",
                            "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WD[weekday], d, MO[(month - 1) as usize], year, h, m, s
    )
}

fn with_last_modified(mut resp: Response, ms: u64) -> Response {
    if ms > 0 {
        if let Ok(v) = HeaderValue::from_str(&http_date(ms)) {
            resp.headers_mut().insert(header::LAST_MODIFIED, v);
        }
    }
    resp
}

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
    // express res.send(string) stamps text/html on plain-text bodies
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(msg))
        .unwrap()
}
