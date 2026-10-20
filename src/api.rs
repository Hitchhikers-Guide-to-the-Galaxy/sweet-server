//! The fedwiki.api v1 door — a native port of the reference Farm Plugin's
//! serving side (wiki-plugin-farm 0.9.2: discover.js, mount.js, merge.js,
//! gate.js, mcp.js, vocabulary.js, and the farm's own assets.js
//! declaration — its handlers run in the sidecar), pinned differentially by the
//! conformance harness. Handlers themselves run in the ONE embedded Node
//! sidecar (sidecar.rs); everything else — discovery, coercion, defaults,
//! context, gating, the merged document, MCP — is Rust.
//!
//! Recorded divergences (Sweet Plugin Plan Phase 1):
//! - handler calls time out (sidecar.rs); the reference waits forever
//! - the registry is built once, lazily, on first use — the reference
//!   fills it asynchronously after boot, so a very early request there
//!   can see a smaller document
//! - api-keys.json is not yet consulted (Phase 2): where a key would
//!   have authorised a write, sweet falls through to place/person

use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

use crate::sidecar::Sidecar;

const PREFIX: &str = "/system/api";
const OPENAPI_VERSION: &str = "3.1.0";
const SUPPORTED_VERSIONS: [i64; 1] = [1];
const KNOWN_CONTEXT: [&str; 5] = ["origin", "farmRoot", "status", "url", "port"];
const METHODS: [&str; 5] = ["get", "post", "put", "delete", "patch"];

/// Phase 4 (Honesty for the Unserved): route prefixes sweet answers
/// NATIVELY — its own ports of plugin servers. A declarer's
/// served-by-plugin operation stays described in `paths` only when its
/// declared path lands on one of these; everything else is excluded and
/// recorded in `skipped`, because the plugin's own Node server does not
/// run here and sweet ships no forwarder, by decision.
const NATIVE_PLUGIN_ROUTES: [&str; 5] =
    ["/plugin/assets/", "/plugin/plugmatic/", "/plugin/shortlink/", "/plugin/table/", "/plugin/oneclick/"];

fn natively_served(plugin: &str, declared_path: &str) -> bool {
    NATIVE_PLUGIN_ROUTES
        .iter()
        .any(|prefix| declared_path.starts_with(prefix))
        // the similarity server's /system routes, ported in similarity-core
        || (plugin == "similarity" && similarity_core::PORTED.contains(&declared_path))
}

const INFO_DESCRIPTION: &str = "Every interface this farm offers, merged from what its plugins declare. A plugin declares a specification and a module of plain functions; the farm mounts them and describes them here. Only operations that are actually mounted appear — what was refused or skipped is listed under x-fedwiki-farm. A plugin whose wiki item text is a line-oriented command language publishes that language under x-fedwiki-vocabulary: each command says which parameter it produces, or that it never leaves the browser.";

pub struct Declaration {
    pub plugin: String, // full package name
    pub name: String,   // short name
    spec: Value,
    handlers_path: Option<PathBuf>,
    vocabulary_path: Option<PathBuf>,
    base: String,
}

pub struct Mounted {
    pub short: String,
    pub op_id: String,       // bare, from the plugin's spec
    pub prefixed: String,    // "<short>_<opId>", farm-unique
    pub method: String,      // lowercase
    pub route: String,
    pub(crate) op: Value,    // the raw operation from the spec
    handlers_path: PathBuf,
}

struct Described {
    method: String,
    route: String,
    op: Value,
    short: String,
}

struct Vocab {
    name: String,
    item_type: String,
    version: i64,
    fallback: Value,
    /// commands with route/reachable bound (vocabulary.js bindCommands)
    bound: Value,
}

pub struct Api {
    pub sidecar: Sidecar,
    declarations: Vec<Declaration>,
    pub(crate) registry: Vec<Mounted>,
    described: Vec<Described>,
    refused: Vec<(String, String)>,        // (plugin full, why)
    skipped: Vec<(String, String, String)>, // (short, where, why)
    vocabularies: Vec<Vocab>,
    farm_version: String,
}

// ------------------------------------------------------------- discovery

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The wiki package's own package.json — discovery reads its dependencies
/// list, exactly like plugins.startServers hands the farm (discover.js:102).
fn wiki_package_json(vendor: &Path) -> Option<Value> {
    for candidate in [
        vendor.parent().map(|p| p.join("package.json")),
        Some(vendor.join("wiki/package.json")),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(v) = read_json(&candidate) {
            if v["dependencies"].is_object() {
                return Some(v);
            }
        }
    }
    None
}

/// discover.js readDeclaration — exact refusal strings.
fn read_declaration(vendor: &Path, pkg_name: &str) -> Result<Option<Declaration>, String> {
    let dir = vendor.join(pkg_name);
    let Some(pkg) = read_json(&dir.join("package.json")) else {
        return Ok(None);
    };
    let api = &pkg["fedwiki"]["api"];
    if api.is_null() {
        return Ok(None);
    }
    let version = &api["version"];
    if !version
        .as_i64()
        .map(|v| SUPPORTED_VERSIONS.contains(&v))
        .unwrap_or(false)
    {
        return Err(format!(
            "declaration version {} is not understood (this farm understands {})",
            version,
            SUPPORTED_VERSIONS.map(|v| v.to_string()).join(", ")
        ));
    }
    let Some(spec_rel) = api["spec"].as_str().filter(|s| !s.is_empty()) else {
        return Err("declaration names no spec".into());
    };
    let spec_bytes = std::fs::read(dir.join(spec_rel))
        .map_err(|e| format!("{e}"))?;
    let spec: Value = serde_json::from_str(
        std::str::from_utf8(&spec_bytes).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    if !spec["paths"].is_object() {
        return Err(format!("{spec_rel} declares no paths"));
    }
    let handlers_path = api["handlers"].as_str().map(|h| dir.join(h));
    if handlers_path.is_none() {
        let all_plugin_served = spec["paths"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|ops| ops.as_object().into_iter().flatten())
            .all(|(_, op)| op["x-fedwiki-served-by"] == "plugin");
        if !all_plugin_served {
            return Err(
                "declaration names no handlers module, and not every operation is \
                 served by the plugin itself"
                    .into(),
            );
        }
    }
    if let Some(h) = &handlers_path {
        if !h.exists() {
            return Err(format!(
                "handlers module missing: {}",
                api["handlers"].as_str().unwrap()
            ));
        }
    }
    let vocabulary_path = api["vocabulary"].as_str().map(|v| dir.join(v));
    if let Some(v) = &vocabulary_path {
        if !v.exists() {
            return Err(format!(
                "vocabulary module missing: {}",
                api["vocabulary"].as_str().unwrap()
            ));
        }
    }
    let name = pkg_name.trim_start_matches("wiki-plugin-").to_string();
    Ok(Some(Declaration {
        plugin: pkg_name.to_string(),
        base: format!("{PREFIX}/{name}"),
        name,
        spec,
        handlers_path,
        vocabulary_path,
    }))
}

// ---------------------------------------------------------------- build

impl Api {
    pub fn build(vendor: &Path, _port: u16) -> Api {
        let sidecar = Sidecar::new();
        let farm_version = read_json(&vendor.join("wiki-plugin-farm/package.json"))
            .and_then(|p| p["version"].as_str().map(String::from))
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").into());
        let mut api = Api {
            sidecar,
            declarations: Vec::new(),
            registry: Vec::new(),
            described: Vec::new(),
            refused: Vec::new(),
            skipped: Vec::new(),
            vocabularies: Vec::new(),
            farm_version,
        };
        let Some(pkg) = wiki_package_json(vendor) else {
            return api; // an empty farm still answers the doors
        };
        let mut siblings: Vec<String> = pkg["dependencies"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|n| n.starts_with("wiki-plugin-") && *n != "wiki-plugin-farm")
            .cloned()
            .collect();
        siblings.sort();
        // server.js (farm 0.9.0): the farm's own operations (assets),
        // declared the way a sibling's are and mounted by the same code —
        // gated, documented and offered to agents with no special case
        match read_declaration(vendor, "wiki-plugin-farm") {
            Ok(None) => {}
            Ok(Some(d)) => api.mount(d),
            Err(why) => api
                .refused
                .push(("wiki-plugin-farm".into(), format!("its own operations — {why}"))),
        }
        for pkg_name in siblings {
            match read_declaration(vendor, &pkg_name) {
                Ok(None) => {}
                Ok(Some(d)) => api.mount(d),
                Err(why) => api.refused.push((pkg_name, why)),
            }
        }
        api
    }

    /// mount.js mountDeclaration — the skip rules, in order, exact strings.
    fn mount(&mut self, d: Declaration) {
        // handlers module exports, via the sidecar (import-time failure
        // refuses the declaration, like the reference's promise chain)
        let exports: Vec<String> = if let Some(h) = &d.handlers_path {
            match self
                .sidecar
                .request("handlers", json!({"path": h.to_string_lossy()}))
            {
                Ok(v) => v["exports"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.as_str().map(String::from))
                    .collect(),
                Err(e) => {
                    self.refused.push((d.plugin.clone(), strip_status(&e)));
                    return;
                }
            }
        } else {
            Vec::new()
        };

        let paths: Vec<(String, Value)> = d.spec["paths"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (spec_path, ops) in &paths {
            let Some(ops) = ops.as_object() else { continue };
            for (method, op) in ops {
                if !METHODS.contains(&method.as_str()) {
                    continue;
                }
                let where_ = format!("{} {} {}", d.name, method.to_uppercase(), spec_path);
                let op_id = op["operationId"].as_str().unwrap_or("");
                let served_by_plugin = op["x-fedwiki-served-by"] == "plugin";
                if served_by_plugin {
                    if op_id.is_empty() {
                        self.skip(&d.name, &where_,
                            "no operationId, so it cannot be named in the document");
                        continue;
                    }
                    if !natively_served(&d.name, spec_path) {
                        // the node farm describes these because the
                        // plugin's own server answers there; here it does
                        // not, and a description of a door that is not
                        // there is a lie — recorded divergence
                        self.skip(&d.name, &where_, &format!(
                            "served by {} itself, and that server does not                              run here — no forwarder, by decision",
                            d.plugin));
                        continue;
                    }
                    self.described.push(Described {
                        method: method.clone(),
                        route: spec_path.clone(),
                        op: op.clone(),
                        short: d.name.clone(),
                    });
                    continue;
                }
                if op_id.is_empty() {
                    self.skip(&d.name, &where_, "no operationId, so no handler can be named");
                    continue;
                }
                if !exports.iter().any(|e| e == op_id) {
                    self.skip(&d.name, &where_,
                        &format!("handlers module exports no function {op_id}"));
                    continue;
                }
                let route = format!("{}{}", d.base, spec_path);
                if !route_has_extension(&route) {
                    self.skip(&d.name, &where_,
                        &format!("route {route} has no extension and would be shadowed by the page route"));
                    continue;
                }
                let unmet: Vec<&str> = op["x-fedwiki-context"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.as_str())
                    .filter(|c| !KNOWN_CONTEXT.contains(c))
                    .collect();
                if !unmet.is_empty() {
                    self.skip(&d.name, &where_,
                        &format!("declares context this farm cannot supply: {}", unmet.join(", ")));
                    continue;
                }
                let read_only = op["x-fedwiki-read-only"] == true;
                let writes = op["x-fedwiki-writes"] == true;
                if read_only && writes {
                    self.skip(&d.name, &where_,
                        "declares itself both read-only and writing — one of the two is wrong");
                    continue;
                }
                if !read_only && !writes {
                    self.skip(&d.name, &where_,
                        "declares neither x-fedwiki-read-only nor x-fedwiki-writes, and the farm mounts nothing whose nature it has not been told");
                    continue;
                }
                if read_only && method != "get" {
                    self.skip(&d.name, &where_,
                        &format!("declared read-only but is a {} — read-only means GET", method.to_uppercase()));
                    continue;
                }
                self.registry.push(Mounted {
                    short: d.name.clone(),
                    op_id: op_id.to_string(),
                    prefixed: format!("{}_{}", d.name, op_id),
                    method: method.clone(),
                    route,
                    op: op.clone(),
                    handlers_path: d.handlers_path.clone().unwrap(),
                });
            }
        }

        // vocabulary — never fatal (vocabulary.js; a bad table is skipped)
        if let Some(vpath) = &d.vocabulary_path {
            let where_ = format!("{} vocabulary", d.name);
            match self.sidecar.request(
                "vocabulary",
                json!({"path": vpath.to_string_lossy(), "name": d.name}),
            ) {
                Ok(v) => {
                    let problems = check_vocabulary(&v, &d.spec);
                    if problems.is_empty() {
                        let mounted: Vec<&Mounted> = self
                            .registry
                            .iter()
                            .filter(|m| m.short == d.name)
                            .collect();
                        let bound = bind_commands(&v["commands"], &mounted);
                        self.vocabularies.push(Vocab {
                            name: d.name.clone(),
                            item_type: v["itemType"].as_str().unwrap_or(&d.name).into(),
                            version: v["version"].as_i64().unwrap_or(1),
                            fallback: v["fallback"].clone(),
                            bound,
                        });
                    } else {
                        self.skip(&d.name, &where_, &problems.join("; "));
                    }
                }
                Err(e) => self.skip(&d.name, &where_, &strip_status(&e)),
            }
        }
        self.declarations.push(d);
    }

    fn skip(&mut self, short: &str, where_: &str, why: &str) {
        self.skipped.push((short.into(), where_.into(), why.into()));
    }

    pub fn find(&self, method: &str, path: &str) -> Option<&Mounted> {
        self.registry
            .iter()
            .find(|m| m.method.eq_ignore_ascii_case(method) && m.route == path)
    }

    pub fn find_tool(&self, name: &str) -> Option<&Mounted> {
        self.registry.iter().find(|m| m.prefixed == name)
    }
}

fn route_has_extension(route: &str) -> bool {
    let last = route.rsplit('/').next().unwrap_or("");
    match last.rsplit_once('.') {
        Some((_, ext)) => !ext.is_empty()
            && ext.chars().all(|c| c.is_ascii_alphanumeric()),
        None => false,
    }
}

fn strip_status(e: &str) -> String {
    // sidecar errors arrive as "<status> <message>"
    match e.split_once(' ') {
        Some((code, rest)) if code.chars().all(|c| c.is_ascii_digit()) => rest.into(),
        _ => e.into(),
    }
}

// ----------------------------------------------------- vocabulary checks

const KINDS: [&str; 7] = ["operation", "parameter", "payload", "presentation",
                          "client-data", "macro", "alias"];
const UNBOUND: [&str; 2] = ["presentation", "client-data"];

fn parameters_of(spec: &Value) -> Map<String, Value> {
    let mut found = Map::new();
    for ops in spec["paths"].as_object().into_iter().flatten() {
        for (_, op) in ops.1.as_object().into_iter().flatten() {
            for p in op["parameters"].as_array().into_iter().flatten() {
                if p["in"] == "query" {
                    if let Some(n) = p["name"].as_str() {
                        found.insert(n.into(), p.clone());
                    }
                }
            }
        }
    }
    found
}

fn derive_enum(commands: &Value, parameter: &str) -> Vec<Value> {
    commands
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, c)| {
            c["kind"] == "parameter" && c["parameter"] == parameter && !c["value"].is_null()
        })
        .map(|(_, c)| c["value"].clone())
        .collect()
}

fn check_vocabulary(voc: &Value, spec: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    let declared = parameters_of(spec);
    let commands = &voc["commands"];
    let cmd_re = regex::Regex::new(r"^[A-Z][A-Z0-9_]*$").unwrap();
    for (name, command) in commands.as_object().into_iter().flatten() {
        if !cmd_re.is_match(name) {
            problems.push(format!("{name} is not an UPPERCASE command word"));
        }
        let kind = command["kind"].as_str().unwrap_or("");
        if kind.is_empty() {
            problems.push(format!(
                "{name} declares no kind — every command must say what it is"
            ));
            continue;
        }
        if !KINDS.contains(&kind) {
            problems.push(format!("{name} has unknown kind {kind}"));
        }
        if kind == "alias" {
            let target = &commands[command["alias"].as_str().unwrap_or("")];
            if target.is_null() {
                problems.push(format!(
                    "{name} is an alias of {}, which is not a command",
                    command["alias"].as_str().unwrap_or("undefined")
                ));
            } else if target["kind"] == "alias" {
                problems.push(format!("{name} aliases an alias"));
            }
        }
        if kind == "macro" {
            let expands = command["expands"].as_array();
            for target in expands.into_iter().flatten() {
                let t = target.as_str().unwrap_or("");
                if commands[t].is_null() {
                    problems.push(format!("{name} expands to {t}, which is not a command"));
                } else if commands[t]["kind"] == "macro" {
                    problems.push(format!("{name} expands to another macro, {t}"));
                }
            }
            if expands.map(|a| a.is_empty()).unwrap_or(true) {
                problems.push(format!("{name} is a macro expanding to nothing"));
            }
        }
        if kind == "parameter" {
            match command["parameter"].as_str() {
                None => problems.push(format!("{name} names no parameter")),
                Some(p) if !declared.contains_key(p) => problems.push(format!(
                    "{name} names parameter {p}, which no operation declares"
                )),
                _ => {}
            }
            if command["value"].is_null() {
                problems.push(format!(
                    "{name} supplies no value for {}",
                    command["parameter"].as_str().unwrap_or("undefined")
                ));
            }
        }
        if UNBOUND.contains(&kind)
            && (!command["parameter"].is_null() || !command["operationId"].is_null())
        {
            problems.push(format!(
                "{name} is {kind} but names a parameter or operation — those never reach a server"
            ));
        }
        if kind == "client-data" && command["why"].is_null() {
            problems.push(format!(
                "{name} is client-data but does not say why a server cannot supply it"
            ));
        }
    }
    for (pname, parameter) in &declared {
        let derived = derive_enum(commands, pname);
        let schema = &parameter["schema"];
        let stated = if schema["enum"].is_array() {
            schema["enum"].as_array()
        } else {
            schema["items"]["enum"].as_array()
        };
        let Some(stated) = stated else { continue };
        if derived.is_empty() {
            continue;
        }
        for value in &derived {
            if !stated.contains(value) {
                problems.push(format!(
                    "a command emits {pname}={}, which the spec forbids",
                    value.as_str().unwrap_or("?")
                ));
            }
        }
    }
    problems
}

/// vocabulary.js bindCommands.
fn bind_commands(commands: &Value, mounted: &[&Mounted]) -> Value {
    let mut bound = Map::new();
    for (name, command) in commands.as_object().into_iter().flatten() {
        let mut entry = command.clone();
        let kind = command["kind"].as_str().unwrap_or("");
        if ["parameter", "operation", "payload"].contains(&kind) {
            let target = command["operationId"]
                .as_str()
                .map(String::from)
                .or_else(|| mounted.first().map(|m| m.op_id.clone()));
            let route = target
                .and_then(|t| mounted.iter().find(|m| m.op_id == t))
                .map(|m| Value::String(m.route.clone()))
                .unwrap_or(Value::Null);
            entry["reachable"] = Value::Bool(!route.is_null());
            entry["route"] = route;
        } else if kind == "macro" || kind == "alias" {
            entry["reachable"] = Value::Null;
        } else {
            entry["reachable"] = Value::Bool(false);
        }
        bound.insert(name.clone(), entry);
    }
    Value::Object(bound)
}

// ----------------------------------------------------------- the document

fn namespace_refs(value: &mut Value, prefix: &str) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get_mut("$ref") {
                if let Some(rest) = r.strip_prefix("#/components/") {
                    let parts: Vec<&str> = rest.splitn(2, '/').collect();
                    if parts.len() == 2 {
                        *r = format!("#/components/{}/{prefix}{}", parts[0], parts[1]);
                    }
                }
            }
            for (_, v) in map.iter_mut() {
                namespace_refs(v, prefix);
            }
        }
        Value::Array(a) => {
            for v in a {
                namespace_refs(v, prefix);
            }
        }
        _ => {}
    }
}

/// context.js sitesOf: directories under farmRoot holding a pages/ dir.
fn sites_of(farm_root: &Path) -> Vec<String> {
    let mut sites: Vec<String> = std::fs::read_dir(farm_root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("pages").is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    sites.sort();
    sites
}

/// context.js farmIdentity: fewest dot-labels, ties by shorter string.
fn farm_identity(sites: &[String], farm_root: &Path) -> String {
    sites
        .iter()
        .min_by_key(|s| (s.split('.').count(), s.len()))
        .cloned()
        .or_else(|| {
            farm_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

/// Per-site context values (context.js contextFor): sweet derives them
/// from the resolved Site + --url + the listening port.
pub struct SiteContext {
    pub origin: String,
    pub farm_root: PathBuf,
    pub status: String,
    pub url: String,
    pub port: u16,
}

impl SiteContext {
    pub fn for_site(site: &crate::Site, port: u16) -> SiteContext {
        SiteContext {
            origin: site
                .data
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            farm_root: site
                .data
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| site.data.clone()),
            status: site.data.join("status").to_string_lossy().into_owned(),
            url: site.url.clone(),
            port,
        }
    }

    fn value(&self, name: &str) -> Value {
        match name {
            "origin" => json!(self.origin),
            "farmRoot" => json!(self.farm_root.to_string_lossy()),
            "status" => json!(self.status),
            "url" => json!(self.url),
            "port" => json!(self.port),
            _ => Value::Null,
        }
    }
}

fn context_meaning(sites: &[String]) -> Value {
    let mut origin = json!({
        "description": "The site this question was put to. The same operation asked at two sites of one farm has two different right answers.",
        "addressable": true,
        "chosenBy": "the host you send the request to, not an argument you pass",
    });
    if !sites.is_empty() {
        origin["values"] = json!(sites);
    }
    json!({
        "origin": origin,
        "farmRoot": {
            "description": "Where this farm keeps its sites on disk.",
            "addressable": false,
            "chosenBy": "the farm — it is a filesystem path and is never taken from a caller"
        },
        "status": {
            "description": "The origin site's status directory.",
            "addressable": false,
            "chosenBy": "the farm"
        },
        "url": {
            "description": "The base address the origin site was started with.",
            "addressable": false,
            "chosenBy": "the farm"
        },
        "port": {
            "description": "The TCP port this farm's server listens on. For a handler calling back into its own farm: site names resolve for browsers, not for this process — 127.0.0.1 and this port is the only address that is always true here.",
            "addressable": false,
            "chosenBy": "the farm"
        }
    })
}

// keys.js — the API-key door (Phase 2): a site opts in by writing
// {status}/api-keys.json; scopes are "*" or "<plugin>.<operation>";
// cached by mtime so an edit needs no restart; constant-time compare.

type KeyCache = std::collections::HashMap<PathBuf, (std::time::SystemTime, Option<Value>)>;

fn key_cache() -> &'static std::sync::Mutex<KeyCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<KeyCache>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(KeyCache::new()))
}

fn load_keys(status_dir: &Path) -> Option<Value> {
    let file = status_dir.join("api-keys.json");
    let Ok(meta) = std::fs::metadata(&file) else {
        key_cache().lock().unwrap().remove(status_dir);
        return None;
    };
    let mtime = meta.modified().ok()?;
    let mut cache = key_cache().lock().unwrap();
    if let Some((cached_mtime, keys)) = cache.get(status_dir) {
        if *cached_mtime == mtime {
            return keys.clone();
        }
    }
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
    cache.insert(status_dir.to_path_buf(), (mtime, keys.clone()));
    keys
}

/// keys.js same(): length check then XOR accumulate — a wrong key cannot
/// be found a character at a time.
fn same_secret(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn find_in(bucket: &Value, presented: &str) -> Option<Value> {
    for (secret, holder) in bucket.as_object().into_iter().flatten() {
        if same_secret(secret, presented) {
            return Some(if holder.is_object() {
                holder.clone()
            } else {
                json!({"id": "unnamed"})
            });
        }
    }
    None
}

/// keys.js authorize(): the scope bucket first, then "*". Returns the
/// holder record so the change can be attributed.
pub fn authorize_key(
    status_dir: &Path,
    presented: Option<&str>,
    scope: &str,
) -> Option<Value> {
    let presented = presented?;
    let keys = load_keys(status_dir)?;
    find_in(&keys[scope], presented).or_else(|| find_in(&keys["*"], presented))
}

// gate.js — place and person; a valid key short-circuits both.

/// gate.js isPrivateSite (farm 0.9.1): a bare host, a host:port, or a full
/// origin with its scheme — the document route hands over the last of
/// these. localhost, *.localhost and *.local, plus the operator's
/// FARM_API_PRIVATE_SUFFIXES.
pub fn is_private_site(origin: &str) -> bool {
    static SCHEME: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let scheme = SCHEME.get_or_init(|| regex::Regex::new(r"(?i)^[a-z][a-z0-9+.-]*://").unwrap());
    let rest = scheme.replace(origin, "");
    let host = rest
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_lowercase();
    if host.is_empty() {
        return false;
    }
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    private_suffixes()
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

/// gate.js privateSuffixes (farm 0.9.0): FARM_API_PRIVATE_SUFFIXES is a
/// comma list (`.private.fish,lan`); a leading dot is optional.
fn private_suffixes() -> Vec<String> {
    std::env::var("FARM_API_PRIVATE_SUFFIXES")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_lowercase().trim_start_matches('.').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn public_writes_setting() -> String {
    std::env::var("FARM_API_PUBLIC_WRITES").unwrap_or_default().trim().to_string()
}

/// gate.js publicWritesAllowed (farm 0.9.1): either a switch that opens
/// every write, or a comma list of `<plugin>.<operationId>` scopes that
/// opens only those.
pub fn public_writes_allowed(scope: Option<&str>) -> bool {
    let setting = public_writes_setting();
    if is_on(&setting) {
        return true;
    }
    let Some(scope) = scope.filter(|s| !s.is_empty()) else {
        return false;
    };
    if setting.is_empty() {
        return false;
    }
    setting.split(',').map(str::trim).any(|s| s == scope)
}

fn is_on(setting: &str) -> bool {
    regex::Regex::new(r"(?i)^(1|yes|true|on|all)$").unwrap().is_match(setting)
}
fn is_off(setting: &str) -> bool {
    regex::Regex::new(r"(?i)^(0|no|false|off)$").unwrap().is_match(setting)
}
fn public_refuse_setting() -> String {
    std::env::var("FARM_API_PUBLIC_REFUSE").unwrap_or_default().trim().to_string()
}

/// gate.js publicRefuseScopes (farm 0.10.0).
fn public_refuse_scopes() -> Vec<String> {
    let setting = public_refuse_setting();
    if setting.is_empty() || is_on(&setting) || is_off(&setting) {
        return Vec::new();
    }
    setting.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// gate.js publicWritesRefused (farm 0.10.0): a public site refuses only
/// what FARM_API_PUBLIC_REFUSE names (or `all`); FARM_API_PUBLIC_WRITES=off
/// still refuses everything.
fn public_writes_refused(scope: Option<&str>) -> bool {
    if is_off(&public_writes_setting()) {
        return true;
    }
    if is_on(&public_refuse_setting()) {
        return true;
    }
    scope.filter(|s| !s.is_empty()).is_some_and(|s| public_refuse_scopes().iter().any(|x| x == s))
}

/// gate.js spendPolicy (farm 0.10.0): FARM_API_SPEND is private (the
/// default), authors, or none — the farm owner's one decision for the farm.
fn spend_policy() -> &'static str {
    match std::env::var("FARM_API_SPEND").unwrap_or_default().trim().to_lowercase().as_str() {
        "authors" => "authors",
        "none" => "none",
        _ => "private",
    }
}

fn spend_refused(origin: &str) -> bool {
    match spend_policy() {
        "none" => true,
        "authors" => false,
        _ => !is_private_site(origin),
    }
}

fn spends(op: &Value) -> bool {
    op["x-fedwiki-spends"] == true
}

fn private_only(op: &Value) -> bool {
    op["x-fedwiki-private-only"] == true
}

/// gate.js describeGate — merge.js calls it with no status, so apiKey is
/// the general sentence; `op` carries x-fedwiki-gate, -spends, -private-only.
fn describe_gate(origin: &str, op: &Value) -> Value {
    let place = gate_of(op) == "place";
    let scopes = public_refuse_scopes();
    let mut d = Map::new();
    d.insert("asks".into(), json!(if place {
        "the place only on a private site — any caller; on a public site, the caller too"
    } else {
        "the caller (and, on a public site, whether the farm marks this operation refused)"
    }));
    d.insert("rights".into(), json!(if place {
        "not asked on a private site — this operation changes no page; on a public site, may edit this site, by the wiki's own test"
    } else {
        "may edit this site, by the wiki's own test"
    }));
    d.insert("apiKey".into(), json!(
        "an X-Api-Key listed in the site's status/api-keys.json is accepted where one is configured"));
    d.insert("publicSites".into(), json!(if public_writes_refused(None) {
        "refused — FARM_API_PUBLIC_REFUSE (or FARM_API_PUBLIC_WRITES=off) marks every write".to_string()
    } else if !scopes.is_empty() {
        format!("allowed to the site owner, except {} — FARM_API_PUBLIC_REFUSE lists them", scopes.join(", "))
    } else {
        "allowed to the site owner, by the wiki's own test".to_string()
    }));
    if private_only(op) {
        d.insert("privateOnly".into(), json!(
            "refused on every public site — this operation acts on the farm's own machine"));
    }
    if spends(op) {
        let policy = spend_policy();
        d.insert("spends".into(), json!({
            "policy": policy,
            "means": match policy {
                "authors" => "the owner of any site on this farm may run it (FARM_API_SPEND=authors)",
                "none" => "nobody may run it (FARM_API_SPEND=none), api keys aside",
                _ => "only on this farm's private sites (FARM_API_SPEND unset or private)",
            },
        }));
    }
    if !origin.is_empty() {
        d.insert("here".into(), json!(if is_private_site(origin) { "private" } else { "public" }));
    }
    d.insert("offeredToAgents".into(), json!(false));
    Value::Object(d)
}

/// mount.js — how much the gate asks of a write: 'place' or 'rights'.
pub fn gate_of(op: &Value) -> &'static str {
    if op["x-fedwiki-gate"] == "place" { "place" } else { "rights" }
}

/// gate.js refuseWrite, first step (farm 0.10.0): an x-fedwiki-private-only
/// operation is refused on every public site, BEFORE the key check — key or
/// no key. Returns Some((status, msg)).
pub fn private_only_refusal(origin: &str, scope: &str, op: &Value) -> Option<(u16, String)> {
    if private_only(op) && !is_private_site(origin) {
        let what = if scope.is_empty() { "this operation" } else { scope };
        return Some((403, format!(
            "{what} acts on the farm's own machine and is offered only on private sites; {origin} is public."
        )));
    }
    None
}

/// gate.js refuseWrite, after the key check (farm 0.10.0): the public mark,
/// the spend policy, then the person — except that a place-gated operation
/// on a private site skips the person. Returns Some((status, msg)).
pub fn write_refusal(
    origin: &str,
    authorized: bool,
    scope: &str,
    op: &Value,
) -> Option<(u16, String)> {
    let what = if scope.is_empty() { "this operation" } else { scope };
    if !is_private_site(origin) && public_writes_refused(Some(scope)) {
        return Some((403, format!(
            "{origin} is a public site and this farm is marked to refuse {what} there \
             (FARM_API_PUBLIC_REFUSE, or FARM_API_PUBLIC_WRITES=off). Lift that mark, \
             or give the caller an API key in this site's status/api-keys.json."
        )));
    }
    if spends(op) && spend_refused(origin) {
        let tail = if spend_policy() == "none" {
            "lets nobody spend them (FARM_API_SPEND=none). ".to_string()
        } else {
            format!(
                "keeps that to its private sites, which {origin} is not. The farm owner can let \
                 site authors spend them with FARM_API_SPEND=authors. "
            )
        };
        return Some((403, format!(
            "{what} spends this farm's keys or machine (a model, a transcription), and this farm \
             {tail}An API key in this site's status/api-keys.json also opens it."
        )));
    }
    if gate_of(op) == "place" && is_private_site(origin) {
        return None;
    }
    if !authorized {
        return Some((403, format!(
            "changing a page on {origin} needs the rights to edit it — sign in to this \
             wiki as its owner, or present an X-Api-Key listed in its status/api-keys.json"
        )));
    }
    None
}

/// gate.js crossOriginAllowed (farm 0.9.1): an operation that declares
/// x-fedwiki-cross-origin is answered to a page from another origin only
/// where the site is private, or public writes are allowed for it.
pub fn cross_origin_allowed(origin: &str, scope: &str) -> bool {
    is_private_site(origin) || public_writes_allowed(Some(scope))
}

impl Api {
    /// merge.js mergeDocument — built per request (origin varies).
    pub fn document(&self, origin_url: &str, ctx: &SiteContext) -> Value {
        let mut paths = Map::new();
        let mut components: Map<String, Value> = Map::new();
        let sites = sites_of(&ctx.farm_root);

        let mut order: Vec<&Declaration> = self.declarations.iter().collect();
        order.sort_by(|a, b| a.name.cmp(&b.name));

        for d in &order {
            let prefix = format!("{}_", d.name);
            // components, re-keyed
            for (kind, entries) in d.spec["components"].as_object().into_iter().flatten() {
                let bucket = components
                    .entry(kind.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
                for (cname, cval) in entries.as_object().into_iter().flatten() {
                    let mut v = cval.clone();
                    namespace_refs(&mut v, &prefix);
                    bucket[&format!("{prefix}{cname}")] = v;
                }
            }
            let voc = self.vocabularies.iter().find(|v| v.name == d.name);
            // mounted first, then described — both keyed by real route
            for m in self.registry.iter().filter(|m| m.short == d.name) {
                let merged = self.merge_operation(
                    &m.op, d, &prefix, &m.route, &m.op_id, false, origin_url, voc,
                );
                paths
                    .entry(m.route.clone())
                    .or_insert_with(|| Value::Object(Map::new()))
                    [&m.method] = merged;
            }
            for desc in self.described.iter().filter(|x| x.short == d.name) {
                let op_id = desc.op["operationId"].as_str().unwrap_or("");
                let merged = self.merge_operation(
                    &desc.op, d, &prefix, &desc.route, op_id, true, origin_url, voc,
                );
                paths
                    .entry(desc.route.clone())
                    .or_insert_with(|| Value::Object(Map::new()))
                    [&desc.method] = merged;
            }
        }

        let tags: Vec<Value> = order
            .iter()
            .map(|d| {
                json!({
                    "name": d.name,
                    "description": d.spec["info"]["description"]
                        .as_str()
                        .or(d.spec["info"]["title"].as_str())
                        .unwrap_or("")
                })
            })
            .collect();

        let writes_mounted = self
            .registry
            .iter()
            .filter(|m| m.op["x-fedwiki-writes"] == true)
            .count();
        let mut declaring: Vec<&Declaration> = self.declarations.iter().collect();
        declaring.sort_by(|a, b| a.name.cmp(&b.name));

        let mut doc = Map::new();
        doc.insert("openapi".into(), json!(OPENAPI_VERSION));
        doc.insert("info".into(), json!({
            "title": "Federated Wiki Farm",
            "version": self.farm_version,
            "description": INFO_DESCRIPTION,
        }));
        doc.insert("paths".into(), Value::Object(paths));
        if !origin_url.is_empty() {
            doc.insert("servers".into(), json!([{"url": origin_url}]));
        }
        if !tags.is_empty() {
            doc.insert("tags".into(), Value::Array(tags));
        }
        if !components.is_empty() {
            doc.insert("components".into(), Value::Object(components));
        }
        if !self.vocabularies.is_empty() {
            let mut voc = Map::new();
            for v in &self.vocabularies {
                voc.insert(v.name.clone(), json!({
                    "itemType": v.item_type,
                    "version": v.version,
                    "fallback": v.fallback,
                    "commands": v.bound,
                }));
            }
            doc.insert("x-fedwiki-vocabulary".into(), Value::Object(voc));
        }
        doc.insert("x-fedwiki-farm".into(), json!({
            "id": farm_identity(&sites, &ctx.farm_root),
            "writes": {
                "mounted": writes_mounted,
                "publicSites": if public_writes_refused(None) {
                    "refused — marked by FARM_API_PUBLIC_REFUSE".to_string()
                } else if !public_refuse_scopes().is_empty() {
                    format!("allowed to the site owner, except {}", public_refuse_scopes().join(", "))
                } else {
                    "allowed to the site owner".to_string()
                },
                "spending": spend_policy(),
                "offeredToAgents": false,
            },
            "sites": sites,
            "context": context_meaning(&sites_of(&ctx.farm_root)),
            "declaring": declaring.iter().map(|d| d.plugin.clone()).collect::<Vec<_>>(),
            "served": {
                "byTheFarm": self.registry.len(),
                "byThePluginItself": self.described.len(),
            },
            "refused": self.refused.iter()
                .map(|(p, w)| json!({"plugin": p, "why": w})).collect::<Vec<_>>(),
            "skipped": self.skipped.iter()
                .map(|(p, o, w)| json!({"plugin": p, "operation": o, "why": w}))
                .collect::<Vec<_>>(),
            "render": {"provider": "builtin", "version": 1},
        }));
        Value::Object(doc)
    }

    fn merge_operation(
        &self,
        op: &Value,
        d: &Declaration,
        prefix: &str,
        _route: &str,
        op_id: &str,
        served_by_plugin: bool,
        origin_url: &str,
        voc: Option<&Vocab>,
    ) -> Value {
        let mut merged = op.clone();
        namespace_refs(&mut merged, prefix);
        merged["tags"] = json!([d.name]);
        merged["operationId"] = json!(format!("{}_{op_id}", d.name));
        merged["x-fedwiki-plugin"] = json!(d.plugin);
        merged["x-fedwiki-declared-path"] = json!(declared_path_of(&d.spec, op_id));
        if served_by_plugin {
            merged.as_object_mut().unwrap().remove("x-fedwiki-base");
        } else {
            merged["x-fedwiki-base"] = json!(d.base);
        }
        if merged["x-fedwiki-writes"] == true {
            // describeGate receives the full origin URL, which isPrivateSite
            // reads past its scheme since farm 0.9.1
            merged["x-fedwiki-gated"] = describe_gate(origin_url, op);
        }
        if let Some(voc) = voc {
            if let Some(params) = merged["parameters"].as_array_mut() {
                for p in params {
                    // `from` is the parameter NAME the values come from —
                    // merge.js:122-133. The six plugins author `true`
                    // here, which deriveEnum compares against
                    // c.parameter and never matches: the reference
                    // silently skips, and parity means so do we.
                    let from = p["x-fedwiki-from-commands"].clone();
                    if from.is_null() || from == false {
                        continue;
                    }
                    let derived: Vec<Value> = voc
                        .bound
                        .as_object()
                        .into_iter()
                        .flatten()
                        .filter(|(_, c)| {
                            c["kind"] == "parameter"
                                && c["parameter"] == from
                                && !c["value"].is_null()
                        })
                        .map(|(_, c)| c["value"].clone())
                        .collect();
                    if derived.is_empty() {
                        continue;
                    }
                    if p["schema"]["type"] == "array" {
                        p["schema"]["items"]["enum"] = json!(derived);
                    } else {
                        p["schema"]["enum"] = json!(derived);
                    }
                    let mut cmds = Map::new();
                    for (word, c) in voc.bound.as_object().into_iter().flatten() {
                        if c["kind"] == "parameter"
                            && c["parameter"] == from
                        {
                            if let Some(v) = c["value"].as_str() {
                                cmds.insert(v.into(), json!(word));
                            }
                        }
                    }
                    p["x-fedwiki-commands"] = Value::Object(cmds);
                }
            }
        }
        merged
    }
}

/// The spec path that declared this operationId (for x-fedwiki-declared-path).
fn declared_path_of(spec: &Value, op_id: &str) -> String {
    for (path, ops) in spec["paths"].as_object().into_iter().flatten() {
        for (_, op) in ops.as_object().into_iter().flatten() {
            if op["operationId"] == op_id {
                return path.clone();
            }
        }
    }
    String::new()
}

// -------------------------------------------------------- calling handlers

/// mount.js coerce — port the table precisely, quirks included.
fn coerce(raw: Option<&str>, schema: &Value) -> Option<Value> {
    let raw = raw?;
    match schema["type"].as_str() {
        Some("integer") => match raw.trim().parse::<i64>() {
            Ok(n) => Some(json!(n)),
            Err(_) => {
                // parseInt semantics: leading digits win; NaN -> raw string
                let lead: String = raw
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == '+')
                    .collect();
                match lead.parse::<i64>() {
                    Ok(n) => Some(json!(n)),
                    Err(_) => Some(json!(raw)),
                }
            }
        },
        Some("number") => match raw.trim().parse::<f64>() {
            Ok(n) => Some(json!(n)),
            Err(_) => Some(json!(raw)),
        },
        Some("boolean") => Some(json!(raw == "true" || raw == "1")),
        Some("array") => {
            let items: Vec<String> = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if items.is_empty() {
                None // the ?kinds= rule: fall through to the default
            } else {
                Some(json!(items))
            }
        }
        _ => Some(json!(raw)),
    }
}

/// mount.js takesBinaryBody (farm 0.9.0) — an operation whose body is a
/// file. Such a body is never parsed: it is streamed to a temporary file
/// and the handler is told where (main.rs receive_body).
pub fn takes_binary_body(op: &Value) -> bool {
    op["requestBody"]["content"]
        .as_object()
        .map(|c| {
            c.keys()
                .any(|t| t == "application/octet-stream" || t.ends_with("/*"))
        })
        .unwrap_or(false)
}

/// mount.js bodyLimit — the operator's setting, else the operation's own,
/// else 500 MB.
pub fn body_limit(op: &Value) -> u64 {
    std::env::var("FARM_API_MAX_BODY_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .or_else(|| op["x-fedwiki-max-bytes"].as_u64())
        .unwrap_or(500 * 1024 * 1024)
}

/// mount.js bodyParameters (farm 0.9.0) — a JSON request body's fields,
/// read as parameters under the same default/coerce/required rules as a
/// query parameter.
fn body_parameters(op: &Value) -> Vec<(String, Value, bool)> {
    let schema = &op["requestBody"]["content"]["application/json"]["schema"];
    let Some(props) = schema["properties"].as_object() else {
        return Vec::new();
    };
    let required: Vec<&str> = schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str())
        .collect();
    props
        .iter()
        .map(|(name, s)| (name.clone(), s.clone(), required.contains(&name.as_str())))
        .collect()
}

/// A body field through coerce: strings exactly as a query value would be;
/// values already typed by JSON pass through. (Recorded divergence: the
/// reference's coerce would turn a JSON `true` into false for a boolean.)
fn coerce_body(raw: &Value, schema: &Value) -> Option<Value> {
    match raw {
        Value::Null => None,
        Value::String(s) => coerce(Some(s), schema),
        other => Some(other.clone()),
    }
}

/// mount.js argumentsFor + missingRequired. Query is the raw query string;
/// `body` is the parsed JSON body, whose fields win over the query's
/// (`{ ...req.query, ...req.body }`).
pub fn arguments_for(
    op: &Value,
    query: &str,
    body: Option<&Value>,
    ctx: &SiteContext,
) -> Result<Value, (u16, String)> {
    let q: std::collections::HashMap<String, String> = crate::oidc::url_query_pairs(query);
    let mut args = Map::new();
    let mut missing = Vec::new();
    let body_params = body_parameters(op);
    // the reference merges the body only for an operation that declares one
    let from_body = body
        .filter(|b| b.is_object())
        .filter(|_| !body_params.is_empty());
    for (name, schema, required) in body_params {
        let raw = from_body.map(|b| &b[&name]).filter(|v| !v.is_null());
        let coerced = match raw {
            Some(v) => coerce_body(v, &schema),
            None => coerce(q.get(&name).map(String::as_str), &schema),
        };
        match coerced {
            Some(v) => {
                if v.as_str() == Some("") && required {
                    missing.push(name);
                } else {
                    args.insert(name, v);
                }
            }
            None => {
                if !schema["default"].is_null() {
                    args.insert(name, schema["default"].clone());
                } else if required {
                    missing.push(name);
                }
            }
        }
    }
    for p in op["parameters"].as_array().into_iter().flatten() {
        if p["in"] != "query" {
            continue;
        }
        let Some(name) = p["name"].as_str() else { continue };
        let raw = from_body
            .and_then(|b| b.get(name))
            .filter(|v| !v.is_null());
        let coerced = match raw {
            Some(v) => coerce_body(v, &p["schema"]),
            None => coerce(q.get(name).map(String::as_str), &p["schema"]),
        };
        match coerced {
            Some(v) => {
                let empty = v.as_str() == Some("");
                if empty && p["required"] == true {
                    missing.push(name.to_string());
                } else {
                    args.insert(name.into(), v);
                }
            }
            None => {
                if !p["schema"]["default"].is_null() {
                    args.insert(name.into(), p["schema"]["default"].clone());
                } else if p["required"] == true {
                    missing.push(name.to_string());
                }
            }
        }
    }
    if !missing.is_empty() {
        return Err((400, format!(
            "missing required parameter{}: {}",
            if missing.len() > 1 { "s" } else { "" },
            missing.join(", ")
        )));
    }
    for name in op["x-fedwiki-context"].as_array().into_iter().flatten() {
        let Some(name) = name.as_str() else { continue };
        let v = ctx.value(name);
        if !v.is_null() {
            args.insert(name.into(), v);
        }
    }
    Ok(Value::Object(args))
}

impl Mounted {
    /// `body` is the parsed JSON body; `file` is what only the route can
    /// know — the temporary file a binary body was streamed into. It is
    /// kept apart from caller-supplied values so no caller can name a path.
    pub fn call(
        &self,
        api: &Api,
        query: &str,
        body: Option<&Value>,
        file: Option<&Value>,
        ctx: &SiteContext,
    ) -> Result<Value, (u16, String)> {
        let binary = takes_binary_body(&self.op);
        if binary && file.is_none() {
            return Err((400, "this operation takes a file as its request body, so it can only be called over HTTP".into()));
        }
        let mut args = arguments_for(&self.op, query, body, ctx)?;
        if let (true, Some(f), Some(obj)) = (binary, file, args.as_object_mut()) {
            obj.insert("body".into(), f.clone());
        }
        // shortlink's expand shares the native resolver's failure breaker
        // (one breaker, not two — the reference reaches one singleton
        // through the CJS require cache), so it is answered natively
        if self.short == "shortlink" && self.op_id == "expand" {
            return crate::shortlink::expand(&args);
        }
        api.sidecar
            .request(
                "call",
                json!({
                    "path": self.handlers_path.to_string_lossy(),
                    "fn": self.op_id,
                    "args": args,
                }),
            )
            .map_err(|e| {
                let (status, msg) = match e.split_once(' ') {
                    Some((code, rest)) if code.chars().all(|c| c.is_ascii_digit()) => {
                        (code.parse().unwrap_or(500), rest.to_string())
                    }
                    _ => (500, e),
                };
                (status, msg)
            })
    }

    pub fn writes(&self) -> bool {
        self.op["x-fedwiki-writes"] == true
    }

    pub fn error_body(&self, msg: &str) -> Value {
        json!({"error": msg, "plugin": self.short, "operation": self.op_id})
    }
}

// ---------------------------------------------------------------- MCP door

const PROTOCOL_VERSIONS: [&str; 2] = ["2025-06-18", "2025-03-26"];
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

pub fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn rpc_result(id: &Value, value: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": value})
}

fn offerable(op: &Value) -> bool {
    op["x-fedwiki-writes"] != true && op["x-fedwiki-served-by"] != "plugin"
}

const CONTEXT_SENTENCE: &str = "The answer depends on which site of the wiki farm this was asked at — the same call put to two sites has two different right answers. That is taken from the address you call, not from an argument.";

fn description_for(op: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(s) = op["summary"].as_str() {
        parts.push(s.to_string());
    }
    if let Some(d) = op["description"].as_str() {
        if op["summary"].as_str() != Some(d) {
            parts.push(d.to_string());
        }
    }
    let has_origin = op["x-fedwiki-context"]
        .as_array()
        .map(|a| a.iter().any(|c| c == "origin"))
        .unwrap_or(false);
    if has_origin {
        parts.push(CONTEXT_SENTENCE.into());
    }
    parts.join("\n\n")
}

fn input_schema_for(op: &Value) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for p in op["parameters"].as_array().into_iter().flatten() {
        if p["in"] != "query" {
            continue;
        }
        let Some(name) = p["name"].as_str() else { continue };
        let mut schema = p["schema"].clone();
        if schema.is_null() {
            schema = json!({});
        }
        if let Some(d) = p["description"].as_str() {
            schema["description"] = json!(d);
        }
        if p["required"] == true {
            required.push(name.to_string());
        }
        properties.insert(name.into(), schema);
    }
    let mut out = json!({"type": "object", "properties": properties});
    if !required.is_empty() {
        out["required"] = json!(required);
    }
    out
}

fn tools_from(doc: &Value) -> Vec<Value> {
    let mut tools = Vec::new();
    for (route, ops) in doc["paths"].as_object().into_iter().flatten() {
        for (method, op) in ops.as_object().into_iter().flatten() {
            if !offerable(op) {
                continue;
            }
            tools.push(json!({
                "name": op["operationId"],
                "description": description_for(op),
                "inputSchema": input_schema_for(op),
                "_meta": {"fedwiki/route": route, "fedwiki/method": method.to_uppercase()},
            }));
        }
    }
    tools.sort_by(|a, b| {
        a["name"].as_str().unwrap_or("").cmp(b["name"].as_str().unwrap_or(""))
    });
    tools
}

/// mcp.js handleMessage. Returns None for notifications (HTTP 202 empty).
pub fn handle_mcp(
    api: &Api,
    doc: &Value,
    ctx: &SiteContext,
    message: &Value,
) -> Option<Value> {
    let id = &message["id"];
    let notification = id.is_null();
    match message["method"].as_str().unwrap_or("") {
        "initialize" => {
            let asked = message["params"]["protocolVersion"].as_str().unwrap_or("");
            let version = if PROTOCOL_VERSIONS.contains(&asked) {
                asked
            } else {
                PROTOCOL_VERSIONS[0]
            };
            Some(rpc_result(id, json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "federated-wiki-farm", "version": api.farm_version},
                "instructions": "Every interface this federated wiki farm offers, from the plugins installed on it. Tools are named plugin_operation and only ever read — nothing offered here changes a page. The farm may also mount operations that DO change pages; those are deliberately not offered as tools, because they are gated on the rights to edit the site and a request arriving here carries no session to judge. They are still described, along with the rule that governs them, at /system/api.json — where the same read operations are also available as ordinary URLs.",
            })))
        }
        "notifications/initialized" | "notifications/cancelled" => None,
        "ping" => {
            if notification {
                None
            } else {
                Some(rpc_result(id, json!({})))
            }
        }
        "tools/list" => Some(rpc_result(id, json!({"tools": tools_from(doc)}))),
        "tools/call" => {
            let name = message["params"]["name"].as_str().unwrap_or("");
            let Some(entry) = api.find_tool(name) else {
                return Some(rpc_error(id, INVALID_PARAMS,
                                      &format!("no tool named {name}")));
            };
            // MCP arguments arrive typed; re-serialize to the query shape
            // the shared argument path expects
            let args = &message["params"]["arguments"];
            let query = mcp_args_to_query(args);
            // body parameters are read from the typed arguments directly
            match entry.call(api, &query, Some(args), None, ctx) {
                Ok(value) => Some(rpc_result(id, json!({
                    "content": [{"type": "text",
                                 "text": serde_json::to_string_pretty(&value).unwrap_or_default()}],
                    "structuredContent": value,
                }))),
                Err((_, msg)) => Some(rpc_result(id, json!({
                    "content": [{"type": "text", "text": format!("{name} failed: {msg}")}],
                    "isError": true,
                }))),
            }
        }
        other => {
            if notification {
                None
            } else {
                Some(rpc_error(id, METHOD_NOT_FOUND, &format!("unknown method {other}")))
            }
        }
    }
}

fn mcp_args_to_query(args: &Value) -> String {
    let mut parts = Vec::new();
    for (k, v) in args.as_object().into_iter().flatten() {
        let s = match v {
            Value::String(s) => s.clone(),
            Value::Array(a) => a
                .iter()
                .map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string()))
                .collect::<Vec<_>>()
                .join(","),
            other => other.to_string(),
        };
        parts.push(format!("{}={}", url_encode(k), url_encode(&s)));
    }
    parts.join("&")
}

fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// mcp.js originAllowed: no Origin (not a browser) -> allowed; else the
/// Origin host must equal the request host or be localhost-family.
pub fn origin_allowed(origin: Option<&str>, host: &str) -> bool {
    let Some(origin) = origin else { return true };
    let Some(ohost) = origin.split("://").nth(1).map(|r| r.split('/').next().unwrap_or(r))
    else {
        return false;
    };
    if ohost == host {
        return true;
    }
    regex::Regex::new(r"^(localhost|127\.0\.0\.1|\[::1\])(:\d+)?$")
        .unwrap()
        .is_match(ohost)
}

// ------------------------------------------------- the try door (Phase 3)
//
// try.js: calling an operation from the page that describes it. ALWAYS
// answers 200 with page JSON — wiki-plugin-html registers no .fail, so a
// status outside 2xx is a button that appears to do nothing. Same-origin
// by omission: no CORS headers on this route.

/// raw.js idFor: sha1 of parts joined with '/', first 16 hex chars.
fn id_for(parts: &[&str]) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(parts.join("/").as_bytes());
    let out = h.finalize();
    out.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()[..16]
        .to_string()
}

/// URLSearchParams-style form encoding (spaces become '+').
fn form_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn render_problem(title: &str, why: &str, hint: Option<&str>, asked: Option<&str>) -> Value {
    let mut story = vec![json!({
        "type": "markdown", "id": id_for(&["problem", why]), "text": why
    })];
    if let Some(hint) = hint {
        story.push(json!({
            "type": "markdown", "id": id_for(&["hint", hint]), "text": hint
        }));
    }
    if let Some(asked) = asked {
        story.push(json!({
            "type": "code", "id": id_for(&["asked", asked]), "text": asked
        }));
    }
    json!({
        "title": title,
        "story": story,
        "journal": [{
            "type": "create",
            "item": {"title": title, "story": []},
            "date": 0,
            "provenance": "Generated by the Farm Plugin in answer to a form. Not stored — fork it to keep it.",
        }],
    })
}

fn render_answer(wanted: &str, operation: &Value, result: &Value, asked: &str) -> Value {
    let title = operation["summary"].as_str().unwrap_or(wanted);
    let code = json!({
        "type": "code",
        "id": id_for(&[wanted, "json", asked]),
        "text": serde_json::to_string_pretty(result).unwrap_or_default(),
        "language": "json",
    });
    let terminal = json!({
        "type": "terminal", "id": id_for(&[wanted, "curl", asked]), "text": asked
    });
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    json!({
        "title": title,
        "story": [code.clone(), terminal],
        "journal": [{
            "type": "create",
            "item": {"title": title, "story": [code]},
            "date": now,
            "provenance": format!(
                "Generated by the Farm Plugin by calling {wanted}. Not stored — fork it to keep it."),
        }],
    })
}

/// try.js tryOperation — the whole ladder, answered as page JSON.
pub fn try_operation(
    api: &Api,
    doc: &Value,
    ctx: &SiteContext,
    params: &Map<String, Value>,
    origin_url: &str,
    authorized: bool,
    api_key: Option<&str>,
) -> Value {
    let wanted = params
        .get("operation")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if wanted.is_empty() {
        return render_problem(
            "Nothing to try",
            "The form did not say which operation to call.",
            Some("This usually means the form lost its hidden `operation` field while being edited."),
            None,
        );
    }
    // operationsOf: every operation in the document, in path order
    let mut found: Option<(String, Value)> = None;
    let mut offered: Vec<String> = Vec::new();
    for (route, methods) in doc["paths"].as_object().into_iter().flatten() {
        for (_, operation) in methods.as_object().into_iter().flatten() {
            let op_id = operation["operationId"].as_str().unwrap_or("");
            offered.push(op_id.to_string());
            if op_id == wanted {
                found = Some((route.clone(), operation.clone()));
            }
        }
    }
    let Some((route, operation)) = found else {
        let hint = if offered.is_empty() {
            "This farm mounts no operations at all.".to_string()
        } else {
            format!(
                "It offers {}. An operation that a plugin declares but the farm did not \
                 mount will not appear here — see the farm's own description at \
                 `/system/api.json` for why.",
                offered
                    .iter()
                    .map(|o| format!("`{o}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        return render_problem(
            "No such operation",
            &format!("This farm has no operation called `{wanted}`."),
            Some(&hint),
            None,
        );
    };
    // the callable behind it, the same way the agent door finds it
    let Some(entry) = api.find_tool(&wanted).or_else(|| {
        api.registry.iter().find(|m| m.prefixed == wanted)
    }) else {
        return render_problem(
            "Not mounted here",
            &format!("`{wanted}` is described by this farm but is not mounted, so it cannot be called."),
            Some("See the farm's own description at `/system/api.json`, which records why each one was left out."),
            None,
        );
    };
    if entry.writes() {
        let scope = format!("{}.{}", entry.short, entry.op_id);
        if let Some((_, why)) = private_only_refusal(&ctx.origin, &scope, &entry.op) {
            return render_problem(
                "Not allowed to change that",
                &why,
                Some("This operation changes a page, so it is gated. Reading is offered \
                      freely; writing asks who you are, and where."),
                None,
            );
        }
        let keyed = authorize_key(Path::new(&ctx.status), api_key, &scope).is_some();
        if !keyed {
            // try.js (farm 0.10.0) passes the entry's gate, spends and privateOnly
            if let Some((_, why)) = write_refusal(&ctx.origin, authorized, &scope, &entry.op) {
                return render_problem(
                    "Not allowed to change that",
                    &why,
                    Some("This operation changes a page, so it is gated. Reading is offered \
                          freely; writing asks who you are, and where."),
                    None,
                );
            }
        }
    }
    // only declared query parameters cross; empty strings stay behind
    let mut values: Vec<(String, String)> = Vec::new();
    for p in entry.op["parameters"].as_array().into_iter().flatten() {
        if p["in"] != "query" {
            continue;
        }
        let Some(name) = p["name"].as_str() else { continue };
        if let Some(raw) = params.get(name) {
            let s = match raw {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            if !s.trim().is_empty() {
                values.push((name.to_string(), s));
            }
        }
    }
    let query: String = values
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let asked = format!(
        "curl -s '{origin_url}{route}{}'",
        if query.is_empty() {
            String::new()
        } else {
            format!("?{query}")
        }
    );
    match entry.call(api, &query, None, None, ctx) {
        Ok(result) => render_answer(&wanted, &operation, &result, &asked),
        Err((status, msg)) => render_problem(
            "That did not work",
            &format!("`{wanted}` could not answer: **{msg}**"),
            Some(if status == 400 {
                "Something required was left empty. Go back to the form, fill it in, and try again."
            } else {
                "The same call is below, if you want to run it yourself and see more."
            }),
            Some(&asked),
        ),
    }
}
