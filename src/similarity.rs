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
use similarity_core::{Caller, Config, Request, Similarity, StdHost, CORS};
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
    let host: Arc<dyn similarity_core::Host> = Arc::new(StdHost);
    let config = Config::from_env(host.as_ref(), farm_root, wiki_domains.as_ref(), version, local_embedder);
    Some(Arc::new(Similarity::new(host, config)))
}

pub struct Incoming<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub if_modified_since: Option<&'a str>,
    pub caller: Caller,
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
