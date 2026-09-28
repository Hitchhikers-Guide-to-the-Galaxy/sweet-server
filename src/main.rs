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
//!        [--security_legacy | --security_type friends|hitchhiker]
//!        [--farm <root> [--farm-security friends --farm-config <cfg.json>]]
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
mod api;
mod assets;
mod indexes;
mod oidc;
mod oneclick;
mod plugmatic;
mod render;
mod shortlink;
mod similarity;
mod sidecar;
mod table;
mod search;
mod tools;
mod security;
mod sweetmode;

struct Site {
    data: PathBuf,
    defaults: Option<PathBuf>,
    /// a node_modules dir holding wiki-client and wiki-plugin-* packages;
    /// enables the native-hosted client (HTML shell, lineup, bundles)
    vendor: Option<PathBuf>,
    home: String,
    security: security::Security,
    /// sweet-client mode: serve the Sweet Client build as this site's shell
    sweet_client: Option<PathBuf>,
    sweet_plugins: Option<PathBuf>,
    /// comma-separated neighborhood seed injected into the shell, like the
    /// node server's argv.neighbors -> seedNeighbors
    neighbors: Option<String>,
    /// memoised sitemap/site-index (see indexes.rs)
    index_cache: indexes::Cache,
    /// compact page-JSON bytes, mtime-validated (see indexes.rs)
    page_cache: indexes::PageCache,
    /// --tint <css-color>: appended to /theme/style.css as a .page
    /// background so a reader can SEE a site is served by sweet (the farm
    /// toggle's visual marker). Off by default — parity surfaces unchanged.
    tint: Option<String>,
    /// Mode::OAuth2 only: the OIDC flow state (see oidc.rs)
    oidc: Option<Arc<oidc::Oidc>>,
    /// --tool-pages: the customisable search / page-actions tool pages
    /// (tools.rs). Off by default, so the shell and /system/tool/ stay
    /// exactly as they were.
    tools: tools::Config,
    /// --url: the site's own absolute URL, the reference's argv.url —
    /// context for the fedwiki.api door (api.json servers block, handler
    /// context). Default mirrors defaultargs: http://localhost:<port>.
    url: String,
}

/// Farm mode (--farm <root>): the Host header picks an EXISTING site dir
/// under root — the test framework for real wikis in place, no copies, no
/// ports. A "sweet-" prefix on the first host label is stripped, so behind
/// a wildcard proxy sweet-pi.pi5.private.fish serves root/pi.pi5.private.fish.
/// Default farm security is read-only (the node farm keeps sole write
/// ownership — the shadow-farm stance); `--farm-security oauth2` turns on
/// Keycloak sign-in with per-site owner gating instead (Phase 4).
struct Farm {
    root: PathBuf,
    prefix: String,
    defaults: Option<PathBuf>,
    vendor: Option<PathBuf>,
    sweet_client: Option<PathBuf>,
    sweet_plugins: Option<PathBuf>,
    neighbors: Option<String>,
    tint: Option<String>,
    /// --farm-security oauth2: farmed sites run Mode::OAuth2 with
    /// per-site owner.json gating instead of the read-only Default —
    /// Keycloak Plan Phase 4. One Oidc (and one signed-cookie secret)
    /// serves the whole farm; authorisation stays per-site.
    oidc: Option<Arc<oidc::Oidc>>,
    /// --farm-security friends: farmed sites run Mode::Friends, the claim
    /// model every laptop/Pi private farm already uses (and which
    /// `wiki-security-hitchhiker` is a fork of). The farm's own
    /// `cookieSecret`, read from --farm-config, makes each site's session
    /// stateless so a restart does not sign the operator out of every site
    /// at once.
    friends_secret: Option<String>,
    tools: tools::Config,
    cache: std::sync::RwLock<std::collections::HashMap<String, Arc<Site>>>,
}

impl Farm {
    /// Host normalisation shared with the refusal page: port stripped,
    /// lowercased, and "www." treated as absent (farm.js:149-152).
    fn normalize(host: &str) -> String {
        let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
        host.strip_prefix("www.").unwrap_or(&host).to_string()
    }

    fn site_for(&self, host: &str) -> Option<Arc<Site>> {
        let host = Self::normalize(host);
        let name = match host.split_once('.') {
            Some((first, rest)) if first.starts_with(self.prefix.as_str()) => {
                format!("{}.{}", &first[self.prefix.len()..], rest)
            }
            _ => host.clone(),
        };
        if let Some(s) = self.cache.read().unwrap().get(&name) {
            return Some(s.clone());
        }
        let data = self.root.join(&name);
        if name.contains("..") || name.contains('/') || !data.join("pages").is_dir() {
            return None;
        }
        let mode = if self.oidc.is_some() {
            security::Mode::OAuth2
        } else if self.friends_secret.is_some() {
            security::Mode::Friends
        } else {
            security::Mode::Default
        };
        let sec = security::Security::with_cookie_secret(
            mode,
            &data.join("status/owner.json"),
            self.friends_secret.clone(),
        );
        let site = Arc::new(Site {
            data,
            defaults: self.defaults.clone(),
            vendor: self.vendor.clone(),
            home: "welcome-visitors".into(),
            security: sec,
            sweet_client: self.sweet_client.clone(),
            sweet_plugins: self.sweet_plugins.clone(),
            neighbors: self.neighbors.clone(),
            index_cache: Default::default(),
            page_cache: Default::default(),
            tint: self.tint.clone(),
            oidc: self.oidc.clone(),
            tools: self.tools.clone(),
            url: format!("http://{host}"),
        });
        self.cache.write().unwrap().insert(name, site.clone());
        Some(site)
    }
}

enum Serving {
    Single(Arc<Site>),
    Farm(Arc<Farm>),
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
    let mut sweet_client: Option<PathBuf> = None;
    let mut sweet_plugins: Option<PathBuf> = None;
    let mut neighbors: Option<String> = None;
    let mut farm: Option<PathBuf> = None;
    let mut tint: Option<String> = None;
    let mut oidc_config: Option<PathBuf> = None;
    let mut farm_security: Option<String> = None;
    // --farm-config <wiki-config.json>: the farm's OWN config file, read
    // for `cookieSecret` under --farm-security friends. Same stance as
    // --oidc-config: a mode-600 file, so the secret is never in argv.
    let mut farm_config: Option<PathBuf> = None;
    let mut url: Option<String> = None;
    let mut tool_pages = false;
    let mut search_page: Option<String> = None;
    let mut tools_page: Option<String> = None;
    let mut farm_prefix = "sweet-".to_string();
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
            "--sweet-client" => sweet_client = args.next().map(PathBuf::from),
            "--sweet-plugins" => sweet_plugins = args.next().map(PathBuf::from),
            "--neighbors" => neighbors = args.next(),
            "--farm" => farm = args.next().map(PathBuf::from),
            "--tint" => tint = args.next(),
            "--oidc-config" => oidc_config = args.next().map(PathBuf::from),
            "--farm-security" => farm_security = args.next(),
            "--farm-config" => farm_config = args.next().map(PathBuf::from),
            "--url" => url = args.next(),
            // the customisable tool pages (tools.rs); opt-in, so a site that
            // does not ask for them serves exactly what it served before
            "--tool-pages" => tool_pages = true,
            "--search-page" => search_page = args.next(),
            "--tools-page" => tools_page = args.next(),
            "--farm-prefix" => {
                if let Some(p) = args.next() {
                    farm_prefix = p;
                }
            }
            _ => {}
        }
    }
    let tool_config = tools::Config {
        enabled: tool_pages,
        search: search_page,
        page: tools_page,
    };
    let serving = if let Some(root) = farm {
        let farm_oidc = match farm_security.as_deref() {
            Some("oauth2") | Some("social") => {
                let path = oidc_config.clone().expect(
                    "--farm-security oauth2 requires --oidc-config <wiki-config.json>",
                );
                match oidc::Oidc::from_config_file(&path) {
                    Ok(o) => Some(Arc::new(o)),
                    Err(e) => panic!("{e}"),
                }
            }
            _ => None, // friends, or read-only Default — see below
        };
        let farm_friends = match farm_security.as_deref() {
            Some(t) if t.contains("friends") || t == "hitchhiker" => {
                let path = farm_config.clone().expect(
                    "--farm-security friends requires --farm-config <wiki-config.json>",
                );
                let cfg: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(&path)
                        .unwrap_or_else(|e| panic!("--farm-config {path:?}: {e}")),
                )
                .unwrap_or_else(|e| panic!("--farm-config {path:?}: {e}"));
                match cfg["cookieSecret"].as_str() {
                    Some(sec) if !sec.is_empty() => Some(sec.to_string()),
                    // Refuse rather than fall back to per-boot sessions: a
                    // farm whose logins silently died on every restart is the
                    // failure this flag exists to prevent.
                    _ => panic!("--farm-config {path:?} carries no cookieSecret"),
                }
            }
            Some("oauth2") | Some("social") | None => None,
            Some(other) => {
                panic!("unsupported --farm-security {other:?} (friends|hitchhiker|oauth2)")
            }
        };
        Serving::Farm(Arc::new(Farm {
            root,
            prefix: farm_prefix,
            defaults,
            vendor,
            sweet_client,
            sweet_plugins,
            neighbors,
            tint,
            oidc: farm_oidc,
            friends_secret: farm_friends,
            tools: tool_config,
            cache: std::sync::RwLock::new(std::collections::HashMap::new()),
        }))
    } else {
        let data = data.expect("--data <site-dir> is required (or --farm <root>)");
        // mode selection mirrors the reference CLI: --security_type wins,
        // --security_legacy softens the default stub, else read-only (spec §6.2)
        let mode = match security_type.as_deref() {
            // wiki-security-hitchhiker is wiki-security-friends verbatim
            // plus three middlewares this server does not run (view gate,
            // terms gate, semindex maintenance 503) — the claim model it
            // presents on the wire is friends, so a farm config that names
            // it is accepted as written.
            Some(t) if t.contains("friends") || t == "hitchhiker" => security::Mode::Friends,
            // "social" is the node module's name for the same mode —
            // accepted so a wiki config can be reused verbatim
            Some("oauth2") | Some("social") => security::Mode::OAuth2,
            Some(other) => {
                panic!("unsupported security_type {other:?} (friends|hitchhiker|oauth2)")
            }
            None if security_legacy => security::Mode::Legacy,
            None => security::Mode::Default,
        };
        let oidc = if matches!(mode, security::Mode::OAuth2) {
            let path = oidc_config
                .expect("--security_type oauth2 requires --oidc-config <wiki-config.json>");
            match oidc::Oidc::from_config_file(&path) {
                Ok(o) => Some(Arc::new(o)),
                Err(e) => panic!("{e}"),
            }
        } else {
            None
        };
        // multer's dest option creates assets/uploads at plugin startup — the
        // assets list route then answers {error:null, files:[]} on a fresh site
        let _ = std::fs::create_dir_all(data.join("assets/uploads"));
        let sec = security::Security::new(mode, &data.join("status/owner.json"));
        Serving::Single(Arc::new(Site {
            data,
            defaults,
            vendor,
            home: "welcome-visitors".into(),
            security: sec,
            sweet_client,
            sweet_plugins,
            neighbors,
            index_cache: Default::default(),
            page_cache: Default::default(),
            tint,
            oidc,
            tools: tool_config,
            url: url.unwrap_or_else(|| format!("http://localhost:{port}")),
        }))
    };
    let serving = Arc::new(serving);
    // similarity routes (similarity.rs): the farm's restricted sites come
    // from --farm-config; a farm warms its vector store now, not on the
    // first query, as the plugin's startServer does
    similarity::set_farm_config(farm_config.clone());
    if let Serving::Farm(farm) = serving.as_ref() {
        similarity::for_farm(&farm.root, farm.vendor.as_deref());
    }
    // the fedwiki.api door: built lazily on first use, only when the
    // vendor carries wiki-plugin-farm (parity: no farm plugin, no door)
    let api_state: Arc<std::sync::OnceLock<Option<Arc<api::Api>>>> =
        Arc::new(std::sync::OnceLock::new());
    let api_vendor = match serving.as_ref() {
        Serving::Single(site) => site.vendor.clone(),
        Serving::Farm(farm) => farm.vendor.clone(),
    };

    let app = axum::Router::new().fallback({
        let serving = serving.clone();
        let api_state = api_state.clone();
        let api_vendor = api_vendor.clone();
        move |req: Request<Body>| {
            let serving = serving.clone();
            let api_state = api_state.clone();
            let api_vendor = api_vendor.clone();
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
                let origin = header_str(&req, header::ORIGIN);
                let api_key =
                    header_str(&req, header::HeaderName::from_static("x-api-key"));
                let content_type = header_str(&req, header::CONTENT_TYPE);
                let accept = header_str(&req, header::ACCEPT);
                let if_none_match = header_str(&req, header::IF_NONE_MATCH);
                let if_modified_since = header_str(&req, header::IF_MODIFIED_SINCE);
                let range = header_str(&req, header::RANGE);
                let xff = header_str(
                    &req,
                    header::HeaderName::from_static("x-forwarded-for"),
                );
                let remote_ip = req
                    .extensions()
                    .get::<axum::extract::ConnectInfo<SocketAddr>>()
                    .map(|ci| ci.0.ip().to_string());
                let proto = header_str(
                    &req,
                    header::HeaderName::from_static("x-forwarded-proto"),
                )
                .and_then(|v| v.split(',').next().map(|p| p.trim().to_string()))
                .filter(|p| p == "https")
                .unwrap_or_else(|| "http".into());
                let host_header = header_str(&req, header::HOST);
                let req_host = host_header.clone().unwrap_or_else(|| "localhost".into());
                let site = match serving.as_ref() {
                    Serving::Single(site) => site.clone(),
                    Serving::Farm(farm) => {
                        // farm.js:139-169: no Host is a 400; a host the farm
                        // will not serve gets the reference's refusal page
                        let Some(hh) = host_header.as_deref() else {
                            return Response::builder()
                                .status(StatusCode::BAD_REQUEST)
                                .body(Body::from("Missing host header"))
                                .unwrap();
                        };
                        match farm.site_for(hh) {
                            Some(site) => site,
                            None => return wiki_does_not_exist(hh),
                        }
                    }
                };
                // oauth2: one signed-cookie decode per request; the
                // identity is farm-wide, authorisation stays per-site
                let oauth2_identity = site
                    .oidc
                    .as_ref()
                    .and_then(|o| oidc::decode_session(o, cookie.as_deref()));
                let authorized = if site.oidc.is_some() {
                    site.security.oauth2_is_authorized(oauth2_identity.as_ref())
                } else {
                    site.security.is_authorized(cookie.as_deref())
                };
                let assets_root = site.data.join("assets");
                let auth = Auth {
                    authorized,
                    user: if site.oidc.is_some() {
                        oauth2_identity.is_some()
                    } else {
                        site.security.user(cookie.as_deref())
                    },
                    host: req_host.clone(),
                    proto,
                    origin,
                    api_key,
                    oauth2_identity,
                    if_none_match,
                    range,
                };
                // an api operation whose body is a file is streamed to disk
                // rather than buffered (farm 0.9.0) — decided before the
                // body is read, since reading it is what the stream replaces
                if path.starts_with("/system/api/") && method != axum::http::Method::GET {
                    let state = api_state.get_or_init(|| {
                        api_vendor
                            .as_ref()
                            .filter(|v| v.join("wiki-plugin-farm").is_dir())
                            .map(|v| Arc::new(api::Api::build(v, port)))
                    });
                    if let Some(api) = state {
                        if let Some(entry) = api
                            .find(method.as_str(), &path)
                            .filter(|e| api::takes_binary_body(&e.op))
                        {
                            return api_binary_route(
                                api, entry, &site, &query, content_type.as_deref(),
                                req.into_body(), &auth, port,
                            )
                            .await;
                        }
                    }
                }
                let body = axum::body::to_bytes(req.into_body(), 8 * 1024 * 1024)
                    .await
                    .unwrap_or_default();
                // the similarity routes, native (similarity.rs + the
                // similarity-core crate); unported ones fall through
                if path.starts_with("/system/") {
                    let caller = similarity_core::Caller {
                        remote_ip: remote_ip.clone(),
                        forwarded_for: xff.clone(),
                        origin: auth.origin.clone(),
                        host: req_host.clone(),
                        // trust.js trustedSession: an authorised session
                        // counts only on a site that has an owner file
                        session_trusted: auth.authorized
                            && site.data.join("status/owner.json").is_file(),
                    };
                    let inc = similarity::Incoming {
                        method: method.as_str(),
                        path: &path,
                        query: &query,
                        if_modified_since: if_modified_since.as_deref(),
                        caller,
                        body: &body,
                    };
                    if let Some(resp) = similarity::route(&site.data, site.vendor.as_deref(), &inc) {
                        return resp;
                    }
                }
                // sweet-client mode answers shell/app/api paths first; the
                // wiki API below keeps serving either way
                if let Some(resp) =
                    sweetmode::route(&site, method.as_str(), &path, &query, &body, &auth)
                {
                    return resp;
                }
                // the fedwiki.api door (api.rs) — reference farm-plugin
                // routes under /system/api
                if path.starts_with("/system/api") {
                    let state = api_state.get_or_init(|| {
                        api_vendor
                            .as_ref()
                            .filter(|v| v.join("wiki-plugin-farm").is_dir())
                            .map(|v| Arc::new(api::Api::build(v, port)))
                    });
                    if let Some(api) = state {
                        if let Some(resp) = api_route(
                            api, &site, method.as_str(), &path, &query, &body, &auth, port,
                        ) {
                            return resp;
                        }
                    }
                }
                match (method.as_str(), path.as_str()) {
                    // oauth2 security routes (oidc.rs; OIDC-SECURITY-SPEC.md)
                    ("POST", "/auth/sign-in/oauth2") if site.oidc.is_some() => {
                        oidc::sign_in(site.oidc.as_ref().unwrap(), &auth, &body)
                    }
                    ("GET" | "HEAD", "/auth/oauth2/callback/oauth2")
                        if site.oidc.is_some() =>
                    {
                        oidc::callback(site.oidc.as_ref().unwrap(), &site, &auth, &query)
                    }
                    ("POST", "/auth/sign-out") if site.oidc.is_some() => {
                        oidc::sign_out(site.oidc.as_ref().unwrap(), &auth)
                    }
                    ("GET" | "HEAD", "/auth/client-settings.json")
                        if site.oidc.is_some() =>
                    {
                        oidc::client_settings(site.oidc.as_ref().unwrap(), &site, &auth)
                    }
                    ("GET" | "HEAD", "/auth/loginDialog") if site.oidc.is_some() => {
                        oidc::login_dialog(&auth)
                    }
                    ("GET" | "HEAD", "/auth/loginDone") if site.oidc.is_some() => {
                        oidc::login_done(&site, &auth)
                    }
                    ("GET" | "HEAD", "/auth/claim-wiki") if site.oidc.is_some() => {
                        oidc::claim_wiki(&site, &auth)
                    }
                    ("GET" | "HEAD", "/logout") if site.oidc.is_some() => {
                        oidc::logout(site.oidc.as_ref().unwrap())
                    }
                    // admin-gated version answer (social.js isAdmin ->
                    // admin middleware; the payload names this server)
                    ("GET" | "HEAD", "/system/version.json") if site.oidc.is_some() => {
                        let session = auth.oauth2_identity.clone();
                        if site.oidc.as_ref().unwrap().is_admin(session.as_ref()) {
                            cors_json(json!({"sweet-server": env!("CARGO_PKG_VERSION")}))
                        } else {
                            security::forbidden()
                        }
                    }
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
                    // plugmatic plugin routes (native port — Browse All Plugins)
                    ("GET" | "HEAD", "/plugin/plugmatic/plugins") => {
                        plugmatic::plugins(&plugin_packages(&site))
                    }
                    ("GET" | "HEAD", "/plugin/plugmatic/sitemap.json") => {
                        plugmatic::sitemap(&plugin_packages(&site))
                    }
                    ("GET" | "HEAD", p)
                        if p.starts_with("/plugin/plugmatic/page/") && p.ends_with(".json") =>
                    {
                        let slug = &p["/plugin/plugmatic/page/".len()..p.len() - 5];
                        plugmatic::page_by_slug(&plugin_packages(&site), slug)
                    }
                    ("GET" | "HEAD", p) if p.starts_with("/plugin/plugmatic/file/") => {
                        let rest = &p["/plugin/plugmatic/file/".len()..];
                        match rest.split_once("/slug/") {
                            Some((file, slug)) if site.vendor.is_some() => {
                                plugmatic::file_slug(site.vendor.as_ref().unwrap(), file, slug)
                            }
                            _ => not_found("Page not found"),
                        }
                    }
                    ("POST", "/plugin/plugmatic/plugins") => {
                        plugmatic::plugins_post(&plugin_packages(&site), &body)
                    }
                    ("POST", "/plugin/plugmatic/install" | "/plugin/plugmatic/restart") => {
                        plugmatic::admin_403()
                    }
                    // shortlink plugin routes (native port — shortlink.rs);
                    // a non-key-shaped path falls through to the wiki's own
                    // routing, exactly like the reference's next()
                    ("GET" | "HEAD", p) if p.starts_with("/plugin/shortlink/") => {
                        // XFF is trusted unconditionally, first element wins
                        // (server.js:45); '?' is the last-resort bucket key
                        let ip = xff
                            .as_deref()
                            .and_then(|v| v.split(',').next())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .or(remote_ip)
                            .unwrap_or_else(|| "?".into());
                        match shortlink::route(p, &ip, accept.as_deref()) {
                            Some(resp) => resp,
                            None => route(&site, &path, auth),
                        }
                    }
                    // table plugin page-parity routes (native port — table.rs);
                    // a non-slug-shaped path falls through to the wiki's own
                    // routing, and a dark PUT (no key file) falls through
                    // BEFORE any CORS header, like the reference's next('route')
                    ("GET" | "HEAD", p) if p.starts_with("/plugin/table/") => {
                        match table::get_route(&site, p) {
                            Some(resp) => resp,
                            None => route(&site, &path, auth),
                        }
                    }
                    ("PUT", p) if p.starts_with("/plugin/table/") => {
                        match table::put_route(
                            &site,
                            p,
                            &body,
                            content_type.as_deref(),
                            auth.api_key.as_deref(),
                        ) {
                            Some(resp) => resp,
                            None => write_route(&site, method.as_str(), &path, &body, authorized),
                        }
                    }
                    // One Click Orgs, rewilded (oneclick.rs) — an organisation
                    // is a site; reads fold its pages, commands journal the diff
                    ("GET" | "HEAD", p) if p.starts_with("/plugin/oneclick/") => {
                        match oneclick::get_route(&site, p) {
                            Some(resp) => resp,
                            None => route(&site, &path, auth),
                        }
                    }
                    ("POST", p) if p.starts_with("/plugin/oneclick/") => {
                        match oneclick::post_route(&site, p, &body, authorized) {
                            Some(resp) => resp,
                            None => not_found("Page not found"),
                        }
                    }
                    ("GET" | "HEAD", _) => route(&site, &path, auth),
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
    match serving.as_ref() {
        Serving::Single(site) => eprintln!("sweet-server on http://{addr} serving {:?}", site.data),
        Serving::Farm(farm) => eprintln!("sweet-server FARM on http://{addr} over {:?} (prefix {:?}, read-only)", farm.root, farm.prefix),
    }
    // Server header on every response: the which-server discriminator.
    // sweet-switch historically read ETag-present-as-node; now that sweet
    // mints ETags too (Speedup Phase 3), this header is the probe.
    let app = app.layer(axum::middleware::map_response(|mut r: Response| async move {
        r.headers_mut()
            .insert(header::SERVER, HeaderValue::from_static("sweet-server"));
        r
    }));
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .unwrap();
}

/// Per-request facts the read routes need: recycler gating, the shell's
/// owner variables, and the Host header (absolute urls in sitemap.xml).
#[derive(Clone)]
pub(crate) struct Auth {
    authorized: bool,
    user: bool,
    host: String,
    /// "https" only when X-Forwarded-Proto says so (a TLS proxy in front),
    /// else "http" — the reference's argv.url is http, so direct traffic
    /// keeps parity while proxied sitemap.xml locs stop lying
    proto: String,
    /// the Origin header — better-auth-style CSRF gate on sign-out
    origin: Option<String>,
    /// X-Api-Key — the farm gate's headless-writer door (api.rs keys)
    api_key: Option<String>,
    /// oauth2 mode: the signed-cookie identity {id, username, name},
    /// decoded once per request
    oauth2_identity: Option<Value>,
    /// If-None-Match — 304s on the cached-bytes routes (Speedup Phase 3)
    if_none_match: Option<String>,
    /// Range — single-range 206 on file routes (Speedup Phase 4)
    range: Option<String>,
}

/// The reference farm's refusal, byte-for-byte: 400 + error-page.js
/// render('Requested Wiki Does Not Exist', ...) with the parent domain as
/// the suggested destination (farm.js:158-169). Raw res.end — no
/// content-type header.
fn wiki_does_not_exist(host: &str) -> Response {
    let inc = Farm::normalize(host);
    let up = inc.split('.').skip(1).collect::<Vec<_>>().join(".");
    let body = error_page(
        "Requested Wiki Does Not Exist",
        "The wiki you are trying to access does not exist.",
        &format!("You may visit <a href='//{up}'>{up}</a> for more information."),
    );
    Response::builder()
        .status(StatusCode::BAD_REQUEST)
        .body(Body::from(body))
        .unwrap()
}

/// wiki/error-page.js render(), template reproduced exactly.
fn error_page(title: &str, error_text: &str, message_text: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset=UTF-8>
    <title>{title}</title>
    <style>
    body {{
        background-image:  url('data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAoAAAAKCAMAAAC67D+PAAAABlBMVEXW2N7x8fFtFbdeAAAAKUlEQVR42jXKAQoAAAjCQPf/T8egCaEcbcxgA8MkjhcvsVmapBqJgb4PC6kAOLAu0ikAAAAASUVORK5CYII=');
    }}
    section {{
        background-color: white;
        width: 490px;
        box-shadow: 2px 1px 4px rgba(0,0,0,0.2),
                    inset 0px 0px 40px 0px pink;
        padding: 30px;
        position: absolute;
        left: 50%;
        top: 50%;
        transform: translate(-50%, -50%);
    }}
    header {{
        margin: 0px;
    }}
    header h1:before {{
        content: '';
        background: linear-gradient(135deg, #FE9B36, #507D1A);
        width: 32px;
        height: 32px;
        display: inline-block;
        margin-right: 10px;
        margin-bottom: -6px;
    }}
    header h1 {{
        margin-top: 0px;
    }}
    </style>
</head>
<body>
    <section id="nowiki">
    <header>
        <h1>Oops!</h1>
        <p>{error_text}</p>
    </header>
    <p>{message_text}</p>
    </section>
</body>
</html>"#
    )
}

/// Routes under /system/api — the fedwiki.api door. None = fall through
/// (e.g. try.json, Phase 3) so the page route can 404 normally.
#[allow(clippy::too_many_arguments)]
fn api_route(
    api: &api::Api,
    site: &Site,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
    auth: &Auth,
    port: u16,
) -> Option<Response> {
    let ctx = api::SiteContext::for_site(site, port);
    let cors = |mut resp: Response| {
        let h = resp.headers_mut();
        h.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        h.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, OPTIONS"),
        );
        h.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("Accept, Content-Type"),
        );
        resp
    };
    match (method, path) {
        ("GET" | "HEAD", "/system/api.json") => {
            let origin = format!("{}://{}", auth.proto, auth.host);
            let doc = api.document(&origin, &ctx);
            Some(cors(cors_json(doc)))
        }
        ("OPTIONS", "/system/api.json") => Some(cors(
            Response::builder()
                .status(StatusCode::NO_CONTENT)
                .body(Body::empty())
                .unwrap(),
        )),
        ("POST", "/system/api/try.json") => {
            // form-posted; ALWAYS 200 page JSON, no CORS (same-origin door)
            let params: serde_json::Map<String, Value> =
                match serde_json::from_slice::<Value>(body) {
                    Ok(Value::Object(m)) => m,
                    _ => oidc::url_query_pairs(&String::from_utf8_lossy(body))
                        .into_iter()
                        .map(|(k, v)| (k, Value::String(v)))
                        .collect(),
                };
            let origin = format!("{}://{}", auth.proto, auth.host);
            let doc = api.document(&origin, &ctx);
            let page = api::try_operation(
                api, &doc, &ctx, &params, &origin, auth.authorized,
                auth.api_key.as_deref(),
            );
            Some(
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                    .body(Body::from(page.to_string()))
                    .unwrap(),
            )
        }
        ("GET" | "HEAD", "/system/api/mcp") => Some(
            Response::builder()
                .status(StatusCode::METHOD_NOT_ALLOWED)
                .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                .body(Body::from(
                    json!({"error": "this endpoint answers POST only — it opens no event stream"})
                        .to_string(),
                ))
                .unwrap(),
        ),
        ("POST", "/system/api/mcp") => {
            if !api::origin_allowed(auth.origin.as_deref(), &auth.host) {
                return Some(
                    Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                        .body(Body::from(
                            json!({"error": "cross-origin requests are not accepted here"})
                                .to_string(),
                        ))
                        .unwrap(),
                );
            }
            let origin = format!("{}://{}", auth.proto, auth.host);
            let doc = api.document(&origin, &ctx);
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let reply = if let Some(batch) = parsed.as_array() {
                let replies: Vec<Value> = batch
                    .iter()
                    .filter_map(|m| {
                        wrap_mcp(api, &doc, &ctx, m)
                    })
                    .collect();
                if replies.is_empty() {
                    None
                } else {
                    Some(Value::Array(replies))
                }
            } else {
                wrap_mcp(api, &doc, &ctx, &parsed)
            };
            Some(match reply {
                Some(v) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                    .body(Body::from(v.to_string()))
                    .unwrap(),
                None => Response::builder()
                    .status(StatusCode::ACCEPTED)
                    .body(Body::empty())
                    .unwrap(),
            })
        }
        _ => {
            let entry = api.find(method, path)?;
            if let Some(refused) = api_gate(entry, &ctx, auth) {
                return Some(refused);
            }
            // a JSON body's fields are parameters too (farm 0.9.0)
            let parsed: Option<Value> = serde_json::from_slice(body).ok();
            // table's write persists through the pagehandler — sweet itself
            // — so it is answered natively (table.rs), like shortlink's
            // expand; every other handler runs in the sidecar
            let outcome = if entry.short == "table" && entry.op_id == "write" {
                api::arguments_for(&entry.op, query, parsed.as_ref(), &ctx)
                    .and_then(|args| table::farm_write(site, &args))
            } else {
                entry.call(api, query, parsed.as_ref(), None, &ctx)
            };
            Some(api_outcome(entry, outcome))
        }
    }
}

fn api_outcome(entry: &api::Mounted, outcome: Result<Value, (u16, String)>) -> Response {
    match outcome {
        Ok(value) => cors_json(value),
        Err((status, msg)) => Response::builder()
            .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Body::from(entry.error_body(&msg).to_string()))
            .unwrap(),
    }
}

/// The farm gate for a writing operation: key -> place -> person, a valid
/// key short-circuiting the rest. Some(response) = refused.
fn api_gate(entry: &api::Mounted, ctx: &api::SiteContext, auth: &Auth) -> Option<Response> {
    if !entry.writes() {
        return None;
    }
    let scope = format!("{}.{}", entry.short, entry.op_id);
    if api::authorize_key(Path::new(&ctx.status), auth.api_key.as_deref(), &scope).is_some() {
        return None;
    }
    let (status, msg) = api::write_refusal(&ctx.origin, auth.authorized)?;
    Some(
        Response::builder()
            .status(StatusCode::from_u16(status).unwrap())
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Body::from(entry.error_body(&msg).to_string()))
            .unwrap(),
    )
}

/// An operation whose body is a file (farm 0.9.0 mount.js): gate first,
/// then stream the body into <status>/farm-tmp — never into anything the
/// wiki serves — refusing it the moment it passes the limit, call the
/// handler with {tmpPath, size, sha256, type}, and remove the file after
/// (a handler that kept it has already moved it).
async fn api_binary_route(
    api: &api::Api,
    entry: &api::Mounted,
    site: &Site,
    query: &str,
    content_type: Option<&str>,
    body: Body,
    auth: &Auth,
    port: u16,
) -> Response {
    let ctx = api::SiteContext::for_site(site, port);
    if let Some(refused) = api_gate(entry, &ctx, auth) {
        return refused;
    }
    let dir = Path::new(&ctx.status).join("farm-tmp");
    let file = match receive_body(body, &dir, api::body_limit(&entry.op), content_type).await {
        Ok(f) => f,
        Err((status, msg)) => return api_outcome(entry, Err((status, msg))),
    };
    let tmp = file["tmpPath"].as_str().unwrap_or_default().to_string();
    let outcome = entry.call(api, query, None, Some(&file), &ctx);
    let _ = tokio::fs::remove_file(&tmp).await;
    api_outcome(entry, outcome)
}

async fn receive_body(
    body: Body,
    dir: &Path,
    limit: u64,
    content_type: Option<&str>,
) -> Result<Value, (u16, String)> {
    use futures_util::StreamExt;
    use sha2::Digest;
    use tokio::io::AsyncWriteExt;
    let too_large = || (413, format!("the body is larger than this farm accepts ({limit} bytes)"));
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| (500, format!("cannot prepare the farm's temporary folder: {e}")))?;
    let mut rnd = [0u8; 6];
    getrandom::getrandom(&mut rnd).map_err(|e| (500, e.to_string()))?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let hex: String = rnd.iter().map(|b| format!("{b:02x}")).collect();
    let tmp = dir.join(format!("{millis}-{hex}.part"));
    let mut out = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .await
        .map_err(|e| (500, format!("cannot open a temporary file: {e}")))?;
    let mut hash = sha2::Sha256::new();
    let mut size: u64 = 0;
    let mut stream = body.into_data_stream();
    let failure = loop {
        match stream.next().await {
            None => break None,
            Some(Err(_)) => break Some((400, "the upload was abandoned".to_string())),
            Some(Ok(chunk)) => {
                size += chunk.len() as u64;
                if size > limit {
                    break Some(too_large());
                }
                hash.update(&chunk);
                if let Err(e) = out.write_all(&chunk).await {
                    break Some((500, format!("cannot write the temporary file: {e}")));
                }
            }
        }
    };
    let flushed = out.flush().await;
    drop(out);
    if let Some(err) = failure {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(err);
    }
    if let Err(e) = flushed {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err((500, format!("cannot write the temporary file: {e}")));
    }
    let sha: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok(json!({
        "tmpPath": tmp.to_string_lossy(),
        "size": size,
        "sha256": sha,
        "type": content_type.unwrap_or("application/octet-stream"),
    }))
}

/// mcp handleMessage with the reference's throw-wrapper (INTERNAL_ERROR).
fn wrap_mcp(
    api: &api::Api,
    doc: &Value,
    ctx: &api::SiteContext,
    message: &Value,
) -> Option<Value> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        api::handle_mcp(api, doc, ctx, message)
    })) {
        Ok(reply) => reply,
        Err(_) => Some(api::rpc_error(
            &message["id"],
            api::INTERNAL_ERROR,
            "internal error",
        )),
    }
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
            let (map, bytes, etag) = indexes::sitemap_with_bytes(site);
            if inm_matches(&auth, &etag) {
                return with_last_modified(not_modified(&etag), max_date(&map));
            }
            with_last_modified(with_etag(cors_json_shared(bytes), &etag), max_date(&map))
        }
        "/system/plugins.json" => cors_json(plugin_names(site)),
        "/system/factories.json" => cors_json(factories(site)),
        "/sitemap.xml" => sitemap_xml(site, &auth.host, &auth.proto),
        "/system/site-index.json" => {
            let (bytes, map, etag) = indexes::site_index_with_stamp(site);
            if inm_matches(&auth, &etag) {
                return with_last_modified(not_modified(&etag), max_date(&map));
            }
            with_last_modified(with_etag(cors_json_shared(bytes), &etag), max_date(&map))
        }
        "/system/export.json" => {
            let mut map = serde_json::Map::new();
            for (slug, page) in site_pages(site) {
                map.insert(slug, page);
            }
            cors_json(Value::Object(map))
        }
        "/system/sweet-ux.js" if site.tools.enabled => Response::builder()
            .header(header::CONTENT_TYPE, "application/javascript; charset=utf-8")
            .body(Body::from(tools::SCRIPT))
            .unwrap(),
        // Customisable tool pages: /system/tool/<kind>/<slug>.json — the
        // template resolved through the ordinary page fallback and handed to
        // the client as a ghost page (tools.rs). Read-only by construction:
        // it opens files and writes nothing, per the farm-autovivify lesson.
        // Gated, so with --tool-pages absent this path 404s as it always has.
        p if site.tools.enabled
            && p.starts_with("/system/tool/")
            && p.ends_with(".json") =>
        {
            let rest = &p["/system/tool/".len()..p.len() - 5];
            match rest.split_once('/') {
                Some((kind, from)) => {
                    match tools::resolve(site, kind, from, &auth.host, &auth.proto) {
                        Some(page) => cors_json(page),
                        None => not_found("Page not found"),
                    }
                }
                None => not_found("Page not found"),
            }
        }
        p if p.starts_with("/remote/") => remote_proxy(p),
        // recycler reads ride the same authorized middleware as writes
        // (server.js:577,582,617)
        "/recycler/favicon.png" => {
            // the recycler pane's header flag (server.js:576-579); the png
            // ships in wiki-server's default-data
            if !auth.authorized {
                return security::forbidden();
            }
            match site
                .defaults
                .as_ref()
                .and_then(|d| read(&d.join("status/recycler.png")))
            {
                Some(bytes) => serve_bytes(bytes, "image/png"),
                None => not_found("Page not found"),
            }
        }
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
        p if p.starts_with("/assets/") => {
            serve_file_ranged(site.data.join(&p[1..]), guess_mime(p), auth.range.as_deref())
        }
        p if p.starts_with("/theme/") => theme(site, p),
        p if p.starts_with("/plugins/") => plugin_static(site, p),
        // security module's client (server.js:337-340) — the package
        // matches the mode: oauth2 serves wiki-security-social's client
        // (padlock, dialog, better-auth bundle), everything else friends
        p if p.starts_with("/security/") => {
            let file = &p["/security/".len()..];
            if file.contains("..") {
                return not_found("Not found");
            }
            let package = if site.oidc.is_some() {
                "wiki-security-social/client"
            } else {
                "wiki-security-friends/client"
            };
            match site
                .vendor
                .as_ref()
                .and_then(|v| read(&v.join(package).join(file)))
            {
                Some(bytes) => serve_bytes(bytes, guess_mime(p)),
                None => not_found("Not found"),
            }
        }
        p if p.ends_with(".json") && !p[1..].contains('/') => {
            page_json_conditional(site, &p[1..p.len() - 5], auth.if_none_match.as_deref())
        }
        p if p.ends_with(".html") && !p[1..].contains('/') => {
            // server.js:407 reads the page first and 404s when it is missing;
            // the page it finds is rendered into the shell (data-server-generated).
            let slug = p[1..p.len() - 5].to_string();
            match load_page(site, &slug)
                .or_else(|| load_plugin_page(site, &slug))
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            {
                Some(mut page) => {
                    if page["title"].as_str().unwrap_or("").is_empty() {
                        page["title"] = Value::String(slug.replace('-', " "));
                    }
                    shell(site, &[("view".into(), slug)], auth, Some(&page))
                }
                None => not_found("Page not found"),
            }
        }
        p => {
            // lineup URLs: alternating /loc/slug pairs, e.g.
            // /view/welcome-visitors/localhost:4251/some-page (server.js:371)
            if let Some(pairs) = parse_lineup(p) {
                return shell(site, &pairs, auth, None);
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
fn shell(
    site: &Site,
    pairs: &[(String, String)],
    auth: Auth,
    generated: Option<&Value>,
) -> Response {
    if client_dir(site).is_none() {
        return not_found("Page not found"); // no vendor: read-only JSON server
    }
    let title = generated
        .and_then(|p| p["title"].as_str())
        .map(str::to_string)
        .or_else(|| pairs.last().map(|(_, s)| s.replace('-', " ")))
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
            // The generated route (server.js:407) renders the story into the
            // paper and marks the div; the lineup route (server.js:371) never
            // does — it passes {page} alone and lets the client fetch. Keeping
            // the lineup arm byte-for-byte as it shipped is what holds the
            // /view/ parity the suite already pins.
            match generated {
                Some(page) => {
                    let title = page["title"].as_str().unwrap_or(slug);
                    format!(
                        "      <div class='page' id='{slug}' tabindex=\"-1\"{origin} data-server-generated=true>\n        <div class='paper'>\n          {}\n        </div>\n      </div>\n",
                        render::render_story(page, title)
                    )
                }
                None => format!(
                    "      <div class='page' id='{slug}' tabindex=\"-1\"{origin}>\n        <div class='paper'>\n        </div>\n      </div>\n"
                ),
            }
        })
        .collect();
    let owner_footer = owner
        .as_deref()
        .map(|n| format!("Wiki by: <span id='site-owner'>{n}</span></div>"))
        .unwrap_or_default();
    let owned = owner.is_some();
    // The tool-page client, carried by the shell itself. A node farm has to
    // fork its security module to get a tag here; sweet writes the shell, so
    // it simply adds one — and only when --tool-pages asked for it, which is
    // what keeps the default shell byte-for-byte what it always was.
    let ux = if site.tools.enabled {
        "\n    <script src='/system/sweet-ux.js' type='text/javascript'></script>"
    } else {
        ""
    };
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
    <script src='/client.js' type='text/javascript'></script>{ux}
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
pub(crate) fn load_page(site: &Site, slug: &str) -> Option<Vec<u8>> {
    if slug.is_empty() || slug.contains(['/', '.', '\\']) {
        return None;
    }
    read(&site.data.join("pages").join(slug)).or_else(|| {
        site.defaults
            .as_ref()
            .and_then(|d| read(&d.join("pages").join(slug)))
    })
}

/// Third rung of the read-fallback chain (page.js:76-94, 289-291):
/// installed plugins ship default pages in their pages/ dirs — Recent
/// Changes comes from wiki-plugin-activity, Local Changes from
/// wiki-plugin-changes. First plugin in scan order wins, and the served
/// page is annotated with the plugin's short name.
pub(crate) fn load_plugin_page(site: &Site, slug: &str) -> Option<Vec<u8>> {
    if slug.is_empty() || slug.contains(['/', '.', '\\']) {
        return None;
    }
    for (name, dir) in plugin_packages(site) {
        if let Some(bytes) = read(&dir.join("pages").join(slug)) {
            if let Ok(mut page) = serde_json::from_slice::<Value>(&bytes) {
                page["plugin"] = Value::String(name);
                return serde_json::to_vec(&page).ok();
            }
        }
    }
    None
}

fn page_json_conditional(site: &Site, slug: &str, inm: Option<&str>) -> Response {
    if slug.is_empty() || slug.contains(['/', '.', '\\']) {
        return not_found("Page not found");
    }
    // pages/ first, defaults second — the same fallback chain as load_page,
    // walked here by path so the compact byte cache can key on file mtime.
    // Serving the parsed page compact (not the pretty disk bytes) matches
    // the reference's parse-and-stringify wire form; the parse was already
    // paid per request to reject corrupt pages as 404, the cache now keeps
    // its result (Sweet Speedup Plan, Phase 2; validators Phase 3).
    let mut candidates = vec![site.data.join("pages").join(slug)];
    if let Some(d) = &site.defaults {
        candidates.push(d.join("pages").join(slug));
    }
    for path in candidates {
        let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) else {
            continue;
        };
        if let Some((bytes, etag)) = site.page_cache.get(slug, mtime) {
            if inm.is_some_and(|v| v.contains(etag.as_str())) {
                return not_modified(&etag);
            }
            return with_etag(cors_json_shared(bytes), &etag);
        }
        let Some(raw) = read(&path) else { continue };
        // spec: unparseable page is a 404, not a garbled body
        return match serde_json::from_slice::<Value>(&raw) {
            Ok(v) => {
                let bytes =
                    bytes::Bytes::from(serde_json::to_vec(&v).unwrap_or_default());
                let etag = indexes::etag_of(&bytes);
                site.page_cache.put(slug, mtime, bytes.clone(), etag.clone());
                if inm.is_some_and(|v| v.contains(etag.as_str())) {
                    return not_modified(&etag);
                }
                with_etag(cors_json_shared(bytes), &etag)
            }
            Err(_) => not_found("Error Parsing Page"),
        };
    }
    match load_plugin_page(site, slug) {
        Some(bytes) => json_bytes(bytes),
        None => not_found("Page not found"),
    }
}

/// If-None-Match check for the index routes. Contains-match handles the
/// header's list form; weak-vs-strong nuance doesn't arise (we only ever
/// mint weak tags and never serve byte ranges of these routes).
fn inm_matches(auth: &Auth, etag: &str) -> bool {
    auth.if_none_match
        .as_deref()
        .is_some_and(|v| v.contains(etag))
}

fn with_etag(mut resp: Response, etag: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(etag) {
        resp.headers_mut().insert(header::ETAG, v);
    }
    resp
}

fn not_modified(etag: &str) -> Response {
    with_etag(
        with_cors(
            Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .body(Body::empty())
                .unwrap(),
        ),
        etag,
    )
}

// ---------------------------------------------------------------- system

pub(crate) fn site_pages(site: &Site) -> Vec<(String, Value)> {
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

/// Sitemap entry per spec §8: {slug, title, date, synopsis, links} —
/// links omitted when the page coins none (sitemap.js:84-96).
pub(crate) fn sitemap_entry(slug: &str, page: &Value) -> Value {
    let mut entry = json!({
        "slug": slug,
        "title": page["title"],
        "date": last_edit_date(page),
        "synopsis": synopsis(page),
    });
    let links = links(page);
    if !links.as_object().map(|o| o.is_empty()).unwrap_or(true) {
        entry["links"] = links;
    }
    entry
}

/// Full derivation from pages/ — the fallback when a site carries no
/// prebuilt status/sitemap.json. Cached callers go through indexes::sitemap.
pub(crate) fn derive_sitemap(site: &Site) -> Value {
    Value::Array(
        site_pages(site)
            .into_iter()
            .map(|(slug, page)| sitemap_entry(&slug, &page))
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

pub(crate) fn as_slug(title: &str) -> String {
    title
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>()
        .to_lowercase()
}

fn sitemap_xml(site: &Site, host: &str, proto: &str) -> Response {
    // the reference's exact shape: standalone declaration, two-space
    // indent, absolute <proto>://<host> locs (http unless a proxy set
    // X-Forwarded-Proto: https), lastmod = entry date YYYY-MM-DD
    let urls: String = indexes::sitemap(site)
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
                        "\n  <url>\n    <loc>{proto}://{host}/{slug}.html</loc>{lastmod}\n  </url>"
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
    if path == "/theme/style.css" {
        // the site theme, plus the sweet tint marker when --tint is set —
        // theme loads after style.css so this wins over the white paper
        let mut bytes = read(&file).unwrap_or_default();
        if let Some(tint) = &site.tint {
            bytes.extend_from_slice(
                format!(
                    "\n/* served by sweet-server (--tint) */\n.page {{ background-color: {tint}; }}\n"
                )
                .as_bytes(),
            );
        }
        return serve_bytes(bytes, "text/css");
    }
    match read(&file) {
        Some(bytes) => serve_bytes(bytes, guess_mime(path)),
        None => not_found("Not found"),
    }
}

fn serve_file(path: PathBuf, mime: &str) -> Response {
    match read(&path) {
        Some(bytes) => serve_bytes(bytes, mime),
        None => not_found("Not found"),
    }
}

/// Whole files over this stream from disk instead of loading into memory —
/// a video must not cost its own size in resident memory (Speedup Phase 4).
const STREAM_OVER: u64 = 1 << 20;

/// One range spec, RFC 9110 §14: `Ok(Some)` = satisfiable single range,
/// `Ok(None)` = ignore the header and serve 200 (malformed or multipart —
/// ignoring is what the RFC allows and what keeps us simple), `Err` = a
/// syntactically valid range that nothing satisfies -> 416.
fn parse_range(h: &str, len: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = h.strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') || len == 0 {
        return Ok(None);
    }
    let Some((a, b)) = spec.split_once('-') else {
        return Ok(None);
    };
    match (a.trim(), b.trim()) {
        ("", suffix) => match suffix.parse::<u64>() {
            Ok(0) => Err(()),
            Ok(n) => Ok(Some((len.saturating_sub(n), len - 1))),
            Err(_) => Ok(None),
        },
        (start, "") => match start.parse::<u64>() {
            Ok(s) if s < len => Ok(Some((s, len - 1))),
            Ok(_) => Err(()),
            Err(_) => Ok(None),
        },
        (start, end) => match (start.parse::<u64>(), end.parse::<u64>()) {
            (Ok(s), Ok(e)) if s <= e && s < len => Ok(Some((s, e.min(len - 1)))),
            (Ok(_), Ok(_)) => Err(()),
            _ => Ok(None),
        },
    }
}

/// File serving with Accept-Ranges/206 (media seek) and streamed bodies
/// for large files. The reference's express static stack does both; the
/// asset routes are where <audio>/<video>/<img> point.
fn serve_file_ranged(path: PathBuf, mime: &str, range: Option<&str>) -> Response {
    let Ok(meta) = std::fs::metadata(&path) else {
        return not_found("Not found");
    };
    if !meta.is_file() {
        return not_found("Not found");
    }
    let len = meta.len();
    match range.map(|r| parse_range(r, len)) {
        Some(Ok(Some((start, end)))) => {
            use std::io::{Read, Seek, SeekFrom};
            let n = (end - start + 1) as usize;
            let Ok(mut f) = std::fs::File::open(&path) else {
                return not_found("Not found");
            };
            let mut buf = vec![0u8; n];
            if f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(&mut buf).is_err() {
                return not_found("Not found");
            }
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, mime)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                .body(Body::from(buf))
                .unwrap()
        }
        Some(Err(())) => Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{len}"))
            .body(Body::empty())
            .unwrap(),
        _ => {
            if len > STREAM_OVER {
                let Ok(f) = std::fs::File::open(&path) else {
                    return not_found("Not found");
                };
                let stream = tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(f));
                return Response::builder()
                    .header(header::CONTENT_TYPE, mime)
                    .header(header::ACCEPT_RANGES, "bytes")
                    .header(header::CONTENT_LENGTH, len)
                    .body(Body::from_stream(stream))
                    .unwrap();
            }
            match read(&path) {
                Some(bytes) => {
                    let mut resp = serve_bytes(bytes, mime);
                    resp.headers_mut()
                        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                    resp
                }
                None => not_found("Not found"),
            }
        }
    }
}

// Text-shaped extensions are served as text/plain so a browser DISPLAYS a
// plan's .md / .toml / .py / .rs assets instead of downloading them. This is a
// deliberate divergence from the node reference (text/markdown, application/
// toml, application/octet-stream for .py) — recorded in CONFORMANCE.md.
fn guess_mime(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("css") => "text/css",
        Some("js") | Some("mjs") => "text/javascript",
        Some("map") => "application/json",
        Some("json") => "application/json",
        Some("html") => "text/html",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("wasm") => "application/wasm",
        Some("mp3") => "audio/mpeg",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("md") | Some("markdown") | Some("toml") | Some("txt") | Some("csv") | Some("tsv")
        | Some("log") | Some("yaml") | Some("yml") | Some("ini") | Some("cfg") | Some("py")
        | Some("rs") | Some("sh") | Some("coffee") | Some("ts") => "text/plain; charset=utf-8",
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

pub(crate) fn read(path: &Path) -> Option<Vec<u8>> {
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

/// Serialize from a borrow — the cached indexes are shared Arcs and a
/// clone of a multi-megabyte Value per request would undo the memoisation.
#[allow(dead_code)]
fn cors_json_ref(v: &Value) -> Response {
    json_bytes(serde_json::to_vec(v).unwrap())
}

/// Serve pre-serialised shared bytes — a Bytes clone is a refcount bump,
/// so the big index buffers are built once per change, not per request.
fn cors_json_shared(bytes: bytes::Bytes) -> Response {
    with_cors(
        Response::builder()
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Body::from(bytes))
            .unwrap(),
    )
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
