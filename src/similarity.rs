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
/// on first use; None when the vendor has no similarity plugin.
pub fn for_farm(farm_root: &Path, vendor: Option<&Path>) -> Option<Arc<Similarity>> {
    let farms = FARMS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = farms.lock().unwrap();
    if let Some(s) = g.get(farm_root) {
        return s.clone();
    }
    let built = vendor.and_then(|v| build(farm_root, v));
    if let Some(s) = &built {
        let s = s.clone();
        std::thread::spawn(move || s.warm_up());
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
    };
    // startServer warms the model unless an embed URL stands in for it
    if embedder.url().is_none() {
        let _ = embedder.sidecar.request("warm", serde_json::json!({ "path": embedder.path }));
    }
    Some(Arc::new(Similarity::new(host, config, Some(Arc::new(embedder)))))
}

const EMBED_JS: &str = include_str!("similarity_embed.js");

/// The plugin's own embedder, run in a Node bridge — or, when
/// WIKI_EMBED_URL (else the farm root's similarity.json embedUrl) names
/// one, a remote embedder over HTTP, as the plugin does.
struct BridgeEmbedder {
    path: String,
    farm_root: PathBuf,
    sidecar: crate::sidecar::Sidecar,
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
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(10))
            .redirects(0)
            .resolver(|addr: &str| -> std::io::Result<Vec<std::net::SocketAddr>> {
                let host = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(addr);
                if host == "localhost" || host.ends_with(".localhost") {
                    let port: u16 = addr.rsplit_once(':').and_then(|(_, p)| p.parse().ok()).unwrap_or(80);
                    return Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))]);
                }
                std::net::ToSocketAddrs::to_socket_addrs(addr).map(|i| i.collect())
            })
            .build();
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
