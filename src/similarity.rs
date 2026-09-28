//! The similarity routes, native (Sweet Similarity Plan,
//! https://sweet.fedwiki.club/view/sweet-similarity-plan). The work is in
//! the similarity-core crate, shared with node farms through WebAssembly;
//! this module only finds the farm, builds one `Similarity` per farm root,
//! and turns its answers into responses.
//!
//! Present only where node would serve them: the vendor tree carries
//! wiki-plugin-similarity. Routes not yet ported fall through (a 404,
//! as before this module).

use axum::body::Body;
use axum::http::{header, HeaderName, HeaderValue, Response, StatusCode};
use serde_json::Value;
use similarity_core::{
    Caller, Config, EmbedError, Embedder, Fetched, Host, Request, Similarity, StdHost, CORS,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// --farm-config, whose wikiDomains names the farm's restricted sites.
static FARM_CONFIG: OnceLock<Option<PathBuf>> = OnceLock::new();
static FARMS: OnceLock<Mutex<HashMap<PathBuf, Option<Arc<Similarity>>>>> = OnceLock::new();

pub fn set_farm_config(path: Option<PathBuf>) {
    let _ = FARM_CONFIG.set(path);
}

fn plugin_dir(vendor: &Path) -> PathBuf {
    vendor.join("wiki-plugin-similarity")
}

/// The Similarity for a farm root, built (and warmed, off the request path)
/// on first use; None when the vendor has no similarity plugin, or when
/// SWEET_SIMILARITY=off (a farm that should not hold a second copy of the
/// vectors and the model, such as the Pi5's shadow doors).
pub fn for_farm(farm_root: &Path, vendor: Option<&Path>) -> Option<Arc<Similarity>> {
    if std::env::var("SWEET_SIMILARITY").is_ok_and(|v| v.eq_ignore_ascii_case("off")) {
        return None;
    }
    let farms = FARMS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = farms.lock().unwrap();
    if let Some(s) = g.get(farm_root) {
        return s.clone();
    }
    let built = vendor.and_then(|v| build(farm_root, v));
    if let Some(s) = &built {
        let warm = s.clone();
        std::thread::spawn(move || {
            warm.warm_up();
            // parsing every vectors file leaves the allocator holding the
            // peak; hand what is free back to the system (glibc keeps it,
            // measured on the Pi5)
            #[cfg(all(target_os = "linux", target_env = "gnu"))]
            // SAFETY: malloc_trim only releases free memory
            unsafe {
                libc::malloc_trim(0);
            }
        });
        // startServer's timers: the Site Index from peers now (forced) and
        // hourly, the graveyard's verdicts now and every six hours — each
        // a conditional GET that costs a 304 when nothing changed
        let fresh = s.clone();
        std::thread::spawn(move || {
            let mut tick = 0u64;
            loop {
                fresh.refresh_site_index(tick == 0);
                if tick % 6 == 0 {
                    fresh.refresh_verdicts();
                }
                tick += 1;
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        });
    }
    g.insert(farm_root.to_path_buf(), built.clone());
    built
}

fn build(farm_root: &Path, vendor: &Path) -> Option<Arc<Similarity>> {
    let pkg: Value = serde_json::from_slice(&std::fs::read(plugin_dir(vendor).join("package.json")).ok()?).ok()?;
    let version = pkg["version"].as_str().unwrap_or("unknown").to_string();
    // require.resolve('@xenova/transformers') from the plugin's directory
    let local_embedder = [plugin_dir(vendor).join("node_modules/@xenova/transformers"), vendor.join("@xenova/transformers")]
        .iter()
        .any(|p| p.is_dir());
    let wiki_domains: Option<Value> = FARM_CONFIG
        .get()
        .cloned()
        .flatten()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .map(|c| c["wikiDomains"].clone());
    let host: Arc<dyn Host> = Arc::new(NetHost);
    let config = Config::from_env(host.as_ref(), farm_root, wiki_domains.as_ref(), version, local_embedder);
    let embedder = BridgeEmbedder {
        path: plugin_dir(vendor).join("server/embedder.js").to_string_lossy().into_owned(),
        farm_root: farm_root.to_path_buf(),
        sidecar: crate::sidecar::Sidecar::with_script(EMBED_JS, Some(std::time::Duration::from_secs(60))),
        semindex: semindex_path(vendor),
    };
    // startServer warms the model unless an embed URL stands in for it
    if embedder.url().is_none() {
        let _ = embedder.sidecar.request("warm", serde_json::json!({ "path": embedder.path }));
    }
    Some(Arc::new(Similarity::new(host, config, Some(Arc::new(embedder)))))
}

const EMBED_JS: &str = include_str!("similarity_embed.js");

/// wiki-plugin-semindex's indexer, resolved as server.js resolves it (the
/// sibling package, then the vendor tree) — but only when this sweet's
/// environment names WIKI_SEMINDEX_MODE. On node, installing semindex makes
/// the farm a writer; sweet often shares its farm with a node farm whose
/// semindex already writes, and a second writer is exactly what the mode
/// exists to prevent, so here the operator says so explicitly.
fn semindex_path(vendor: &Path) -> Option<String> {
    std::env::var("WIKI_SEMINDEX_MODE").ok().filter(|m| !m.is_empty())?;
    let sibling = plugin_dir(vendor).join("../wiki-plugin-semindex/server/indexer.js");
    let vendored = vendor.join("wiki-plugin-semindex/server/indexer.js");
    [sibling, vendored].into_iter().find(|p| p.is_file()).map(|p| p.to_string_lossy().into_owned())
}

/// The plugin's own embedder, run in a Node bridge — or, when
/// WIKI_EMBED_URL (else the farm root's similarity.json embedUrl) names
/// one, a remote embedder over HTTP, as the plugin does.
struct BridgeEmbedder {
    path: String,
    farm_root: PathBuf,
    sidecar: crate::sidecar::Sidecar,
    /// indexer.js, when semindex runs in the bridge (see semindex_path)
    semindex: Option<String>,
}

impl BridgeEmbedder {
    /// (url, source) — env wins, then the file
    fn url(&self) -> Option<(String, &'static str)> {
        let h = StdHost;
        if let Some(u) = h.env("WIKI_EMBED_URL").and_then(|v| similarity_core::siteindex::http_url(&v)) {
            return Some((u, "env"));
        }
        similarity_core::siteindex::file_config(&h, &self.farm_root).embed_url.map(|u| (u, "file"))
    }
}

impl Embedder for BridgeEmbedder {
    fn embed(&self, text: &str) -> Result<Value, EmbedError> {
        if let Some((url, _)) = self.url() {
            let down = |m: String| EmbedError { message: format!("remote embedder at {url}: {m}"), down: true };
            let resp = ureq::post(&url)
                .timeout(std::time::Duration::from_secs(30))
                .set("Content-Type", "application/json")
                .send_string(&serde_json::json!({ "text": text }).to_string())
                .map_err(|e| down(e.to_string()))?;
            let raw = resp.into_string().map_err(|e| down(e.to_string()))?;
            let v: Value = serde_json::from_str(&raw).map_err(|e| down(format!("response parse error: {e}")))?;
            return Ok(v["vector"].clone());
        }
        self.sidecar
            .request("embed", serde_json::json!({ "path": self.path, "text": text }))
            .map_err(|e| {
                let (code, msg) = e.split_once(' ').unwrap_or(("500", e.as_str()));
                EmbedError { message: msg.to_string(), down: code == "503" || !code.chars().all(|c| c.is_ascii_digit()) }
            })
    }

    fn status(&self) -> Value {
        if let Some((url, source)) = self.url() {
            return serde_json::json!({ "via": "url", "url": url, "source": source });
        }
        self.sidecar
            .request("status", serde_json::json!({ "path": self.path }))
            .unwrap_or_else(|e| serde_json::json!({ "via": "child-process", "state": "down", "lastError": e }))
    }

    fn semindex_mode(&self) -> Option<String> {
        self.semindex.as_ref()?;
        // indexer.js MODE, from the same environment the bridge inherits
        let m = std::env::var("WIKI_SEMINDEX_MODE").unwrap_or_default();
        Some(if ["off", "query-only", "writer"].contains(&m.as_str()) { m } else { "writer".into() })
    }

    fn semindex_enqueue(&self, globs: &str, force: bool) -> Result<Value, String> {
        let path = self.semindex.as_ref().ok_or("no indexer")?;
        // argv as the farm hands it to plugins: farmRootOf(argv) is
        // dirname(argv.data), so any child of the farm root names the farm
        let data = self.farm_root.join("_");
        self.sidecar
            .request("index", serde_json::json!({
                "path": path, "argv": { "farm": true, "data": data }, "globs": globs, "force": force,
            }))
            .map_err(|e| e.split_once(' ').map(|(_, m)| m.to_string()).unwrap_or(e))
    }
}

/// The real filesystem, plus the network for galaxy search: *.localhost
/// over plain http to loopback, everything else over https.
struct NetHost;

impl Host for NetHost {
    fn stat(&self, p: &Path) -> Option<similarity_core::host::Stat> { StdHost.stat(p) }
    fn read(&self, p: &Path) -> Option<Vec<u8>> { StdHost.read(p) }
    fn read_head(&self, p: &Path, n: usize) -> Option<Vec<u8>> { StdHost.read_head(p, n) }
    fn list_dirs(&self, p: &Path) -> Vec<String> { StdHost.list_dirs(p) }
    fn list_files(&self, p: &Path) -> Vec<String> { StdHost.list_files(p) }
    fn write_file(&self, p: &Path, d: &[u8]) -> bool { StdHost.write_file(p, d) }
    fn rename(&self, a: &Path, b: &Path) -> bool { StdHost.rename(a, b) }
    fn create_dir_all(&self, p: &Path) -> bool { StdHost.create_dir_all(p) }
    fn now_ms(&self) -> f64 { StdHost.now_ms() }
    fn env(&self, k: &str) -> Option<String> { StdHost.env(k) }

    fn fetch(&self, url: &str, headers: &[(&str, &str)]) -> Option<Fetched> {
        let agent = agent(std::time::Duration::from_secs(10));
        let mut req = agent.get(url);
        for (k, v) in headers {
            req = req.set(k, v);
        }
        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => return Some(Fetched { status: code, body: vec![], last_modified: None }),
            Err(_) => return None,
        };
        let status = resp.status();
        let last_modified = resp.header("last-modified").map(String::from);
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut body).ok()?;
        Some(Fetched { status, body, last_modified })
    }

    fn post_json(&self, url: &str, body: &[u8], timeout_ms: u64) -> Result<Fetched, String> {
        let resp = agent(std::time::Duration::from_millis(timeout_ms))
            .post(url)
            .set("Content-Type", "application/json")
            .send_bytes(body);
        let resp = match resp {
            Ok(r) => r,
            // a status is an answer: its body is what the peer said
            Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(t)) => return Err(t.to_string()),
        };
        let status = resp.status();
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut out).map_err(|e| e.to_string())?;
        Ok(Fetched { status, body: out, last_modified: None })
    }

    fn touch(&self, p: &Path) -> bool {
        StdHost.touch(p)
    }

    fn get(&self, url: &str) -> Result<Fetched, String> {
        // new URL(url) and http.get's own checks, in node's words
        let (scheme, rest) = url.split_once("://").ok_or_else(|| "Invalid URL".to_string())?;
        if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
            return Err(format!("Protocol \"{}:\" not supported. Expected \"http:\"", scheme.to_ascii_lowercase()));
        }
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => (h.to_string(), p.to_string()),
            _ => (authority.to_string(), if scheme.eq_ignore_ascii_case("https") { "443".into() } else { "80".into() }),
        };
        // getJson has no timeout; a long one stands in for "never"
        let resp = match agent(std::time::Duration::from_secs(300)).get(url).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(ureq::Error::Transport(t)) => {
                return Err(match t.kind() {
                    ureq::ErrorKind::Dns => format!("getaddrinfo ENOTFOUND {host}"),
                    // node 20+ tries each address of a name (autoSelectFamily)
                    // and reports an AggregateError, whose message is empty;
                    // a bare address reports the one connect error
                    ureq::ErrorKind::ConnectionFailed if host.parse::<std::net::IpAddr>().is_err() => String::new(),
                    ureq::ErrorKind::ConnectionFailed => format!("connect ECONNREFUSED {host}:{port}"),
                    _ => t.to_string(),
                });
            }
        };
        let status = resp.status();
        let mut body = Vec::new();
        std::io::Read::read_to_end(&mut resp.into_reader(), &mut body).map_err(|e| e.to_string())?;
        Ok(Fetched { status, body, last_modified: None })
    }
}

/// An agent that resolves *.localhost (and localhost) to loopback, as the
/// plugin's lookup override does; no redirects followed.
fn agent(timeout: std::time::Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(timeout)
        .redirects(0)
        .resolver(|addr: &str| -> std::io::Result<Vec<std::net::SocketAddr>> {
            let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
            if host == "localhost" || host.ends_with(".localhost") {
                let port: u16 = addr.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(80);
                return Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))]);
            }
            std::net::ToSocketAddrs::to_socket_addrs(addr).map(|i| i.collect())
        })
        .build()
}

pub struct Incoming<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub if_modified_since: Option<&'a str>,
    pub caller: Caller,
    pub body: &'a [u8],
}

/// Some(response) when this is a ported similarity route.
pub fn route(site_data: &Path, vendor: Option<&Path>, inc: &Incoming) -> Option<Response<Body>> {
    if !similarity_core::PORTED.contains(&inc.path) {
        return None;
    }
    // startServer: argv.status = {farm}/{site}/status, farm root two up
    let farm_root = site_data.parent()?;
    let sim = for_farm(farm_root, vendor)?;
    let answer = sim.route(&Request {
        method: inc.method,
        path: inc.path,
        query: inc.query,
        if_modified_since: inc.if_modified_since,
        caller: &inc.caller,
        body: inc.body,
    })?;
    let mut resp = Response::builder()
        .status(StatusCode::from_u16(answer.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .body(Body::from(if inc.method == "HEAD" { Vec::new() } else { answer.body }))
        .unwrap();
    let h = resp.headers_mut();
    for (k, v) in CORS.iter() {
        h.insert(HeaderName::from_static(lower(k)), HeaderValue::from_static(v));
    }
    for (k, v) in answer.headers {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(k.to_ascii_lowercase()), HeaderValue::try_from(v)) {
            h.insert(name, value);
        }
    }
    if !h.contains_key(header::CONTENT_TYPE) && answer.status != 304 && answer.status != 204 {
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json; charset=utf-8"));
    }
    Some(resp)
}

fn lower(k: &'static str) -> &'static str {
    match k {
        "Access-Control-Allow-Origin" => "access-control-allow-origin",
        "Access-Control-Allow-Methods" => "access-control-allow-methods",
        _ => "access-control-allow-headers",
    }
}
