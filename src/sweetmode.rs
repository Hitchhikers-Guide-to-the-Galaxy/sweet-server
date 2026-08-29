//! Sweet-client mode — serve the Sweet Client (the wiki-rendering layer of
//! sweet-docs) as this site's shell instead of the node wiki-client.
//!
//! The wiki API surface is untouched: page JSON, system routes, actions and
//! security all keep answering, which is what makes the client switchable —
//! a client is just a shell plus a few extra doors:
//!
//!   /                 -> 302 to /?docs=<home>@<host> (bare), else index.html
//!   /app/*            -> the vite build's assets
//!   /plugins/*        -> sweet plugin modules (CORS *, other origins import)
//!   /api/me           -> who am I (drives the client's edit affordances)
//!   /api/wiki-save    -> the client's write path, mapped onto the SAME
//!                        action engine as PUT /page/:slug/action — one
//!                        write path, not a second one (sweet-docs'
//!                        server.mjs rewrites farm files itself; here the
//!                        conformance-proven actions module does it)
//!   /view/...         -> 302 translation into /?docs=... lineups
//!
//! Enabled by --sweet-client <dist-dir> [--sweet-plugins <dir>].

use crate::{actions, guess_mime, not_found, Auth, Site};
use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;
use serde_json::{json, Value};
use std::path::Path;

/// The shell, with the neighborhood seed injected the way the node server
/// interpolates seedNeighbors into static.html (--neighbors flag).
fn index_html(site: &Site, dist: &Path) -> Response {
    let Ok(mut html) = std::fs::read_to_string(dist.join("index.html")) else {
        return not_found("Not found");
    };
    if let Some(seed) = site.neighbors.as_deref() {
        let tag = format!(
            "<script>window.seedNeighbors = '{}';</script></head>",
            seed.replace('\\', "").replace('\'', "")
        );
        html = html.replacen("</head>", &tag, 1);
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(html))
        .unwrap()
}

fn file(path: &Path, mime: &str, cors: bool) -> Option<Response> {
    let bytes = std::fs::read(path).ok()?;
    let mut b = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.to_string());
    if cors {
        b = b.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");
    }
    Some(b.body(Body::from(bytes)).unwrap())
}

fn redirect(loc: String) -> Response {
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, loc)
        .body(Body::empty())
        .unwrap()
}

fn json_status(status: StatusCode, value: Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(value.to_string()))
        .unwrap()
}

/// Try to answer a request in sweet-client mode. None = not ours, fall
/// through to the normal wiki routing.
pub fn route(
    site: &Site,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
    auth: &Auth,
) -> Option<Response> {
    let dist = site.sweet_client.as_ref()?;
    match (method, path) {
        ("GET" | "HEAD", "/") => {
            if query.contains("docs=") || query.contains("doc=") {
                Some(index_html(site, dist))
            } else {
                Some(redirect(format!("/?docs={}@{}", site.home, auth.host)))
            }
        }
        ("GET" | "HEAD", p) if p.starts_with("/app/") => {
            let rel = &p["/app/".len()..];
            if rel.contains("..") {
                return Some(not_found("Not found"));
            }
            Some(
                file(&dist.join("app").join(rel), guess_mime(p), false)
                    .unwrap_or_else(|| not_found("Not found")),
            )
        }
        ("GET" | "HEAD", p) if p.starts_with("/plugins/") => {
            let rel = &p["/plugins/".len()..];
            if rel.contains("..") {
                return Some(not_found("Not found"));
            }
            // sweet plugin modules first; fall back to the vendor tree's
            // classic bundles (wiki-plugin-<type>/client/<file>) so the
            // sweet-compat classic path still reaches every site plugin
            if let Some(resp) = site
                .sweet_plugins
                .as_ref()
                .and_then(|dir| file(&dir.join(rel), guess_mime(p), true))
            {
                return Some(resp);
            }
            let (plugin, tail) = rel.split_once('/')?;
            let vendor = site.vendor.as_ref()?;
            Some(
                file(
                    &vendor
                        .join(format!("wiki-plugin-{plugin}"))
                        .join("client")
                        .join(tail),
                    guess_mime(p),
                    true,
                )
                .unwrap_or_else(|| not_found("Not found")),
            )
        }
        ("GET", "/api/me") => Some(match site.security.owner_name() {
            Some(name) if auth.authorized => {
                json_status(StatusCode::OK, json!({"user": name, "name": name}))
            }
            _ => json_status(StatusCode::UNAUTHORIZED, json!({})),
        }),
        ("POST", "/api/wiki-save") => {
            let req: Value = serde_json::from_slice(body).ok()?;
            let slug = req["slug"].as_str().unwrap_or_default().to_string();
            if slug.is_empty()
                || !slug
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                || !site.data.join("pages").join(&slug).is_file()
            {
                // sweet-docs server.mjs's exact refusal for foreign pages
                return Some(json_status(
                    StatusCode::NOT_FOUND,
                    json!({"error": "not our farm, or no such page"}),
                ));
            }
            if !auth.authorized {
                return Some(json_status(StatusCode::UNAUTHORIZED, json!({})));
            }
            let action = req["action"].clone();
            if !action.is_object() {
                return Some(json_status(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": "bad action"}),
                ));
            }
            Some(match actions::apply(site, &slug, action) {
                actions::Outcome::Ok => json_status(StatusCode::OK, json!({})),
                actions::Outcome::Error(409, _) => {
                    json_status(StatusCode::CONFLICT, json!({"error": "conflict"}))
                }
                actions::Outcome::Error(_, e) => {
                    json_status(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e}))
                }
            })
        }
        // fedwiki-shaped links keep working: translate lineups into ?docs=
        ("GET" | "HEAD", p) if p.starts_with("/view/") => {
            let segs: Vec<&str> = p.trim_matches('/').split('/').collect();
            let mut cols = Vec::new();
            for pair in segs.chunks(2) {
                if pair.len() == 2 {
                    let target = if pair[0] == "view" {
                        format!("{}@{}", pair[1], auth.host)
                    } else {
                        format!("{}@{}", pair[1], pair[0])
                    };
                    cols.push(target);
                }
            }
            Some(redirect(format!("/?docs={}", cols.join(","))))
        }
        ("GET" | "HEAD", p)
            if p.ends_with(".html") && !p[1..].contains('/') && p != "/index.html" =>
        {
            let slug = &p[1..p.len() - 5];
            Some(redirect(format!("/?docs={}@{}", slug, auth.host)))
        }
        _ => None,
    }
}
