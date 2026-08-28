//! The assets plugin's server routes (wiki-plugin-assets 0.7.4
//! server/server.js) — implemented natively rather than loaded from a
//! plugin package. Four routes on /plugin/assets/*: list and index are
//! CORS-open reads; upload and delete are owner-gated (403 "must be
//! owner"). Uploads land under <data>/assets/<subdir>/ and are served by
//! the core /assets static route.

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;
use serde_json::json;
use std::path::{Path, PathBuf};

/// The plugin's subdir sanitizer: first match of /([\w/-]*)/ — everything
/// up to the first character outside [A-Za-z0-9_/-].
fn sanitize(assets_param: &str) -> String {
    assets_param
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '/' || *c == '-')
        .collect()
}

fn query_param(query: &str, name: &str) -> String {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == name {
            // percent-decode just enough for path-ish values
            return v.replace("%2F", "/").replace("%2f", "/").replace('+', " ");
        }
    }
    String::new()
}

fn cors_json(value: serde_json::Value) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(Body::from(value.to_string()))
        .unwrap()
}

fn plain(status: StatusCode, text: String) -> Response {
    Response::builder()
        .status(status)
        .body(Body::from(text))
        .unwrap()
}

/// GET /plugin/assets/list?assets=<subdir> — file names, non-recursive.
pub fn list(assets_root: &Path, query: &str) -> Response {
    let dir = assets_root.join(sanitize(&query_param(query, "assets")));
    match std::fs::read_dir(&dir) {
        Ok(entries) => {
            let mut files: Vec<String> = entries
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            files.sort();
            cors_json(json!({"error": null, "files": files}))
        }
        // node: s.json({error: err}) — the fs error object
        Err(_) => cors_json(json!({"error": {
            "errno": -2, "code": "ENOENT", "syscall": "scandir",
            "path": dir.to_string_lossy(),
        }})),
    }
}

/// GET /plugin/assets/index — recursive walk, dot entries skipped,
/// entries shaped {file: "/sub/name", size} (leading separator included).
pub fn index(assets_root: &Path) -> Response {
    fn walk(root: &Path, rel: &Path, out: &mut Vec<serde_json::Value>) {
        let Ok(entries) = std::fs::read_dir(root.join(rel)) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let rel_path = rel.join(&name);
            match entry.file_type() {
                Ok(t) if t.is_file() => {
                    let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                    out.push(json!({
                        "file": format!("/{}", rel_path.to_string_lossy()),
                        "size": size,
                    }));
                }
                Ok(t) if t.is_dir() => walk(root, &rel_path, out),
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(assets_root, Path::new(""), &mut out);
    cors_json(serde_json::Value::Array(out))
}

/// POST /plugin/assets/upload — multipart form: `assets` field names the
/// subdir, every file part is saved under it by its original filename.
pub async fn upload(assets_root: &Path, content_type: Option<&str>, body: bytes::Bytes) -> Response {
    let Some(boundary) = content_type.and_then(|ct| multer::parse_boundary(ct).ok()) else {
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "bad multipart".into());
    };
    let stream = futures_util::stream::once(async move {
        Ok::<bytes::Bytes, std::io::Error>(body)
    });
    let mut multipart = multer::Multipart::new(stream, boundary);
    let mut subdir = String::new();
    let mut files: Vec<(String, bytes::Bytes)> = Vec::new();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        let filename = field.file_name().map(String::from);
        let Ok(data) = field.bytes().await else {
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "read failed".into());
        };
        match filename {
            Some(f) => files.push((f, data)),
            None if name == "assets" => {
                subdir = sanitize(&String::from_utf8_lossy(&data));
            }
            None => {}
        }
    }
    let dir: PathBuf = assets_root.join(&subdir);
    if std::fs::create_dir_all(&dir).is_err() {
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "mkdir failed".into());
    }
    let mut errors = Vec::new();
    for (filename, data) in files {
        // basename only — originalname must not traverse
        let safe = Path::new(&filename)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if safe.is_empty() || std::fs::write(dir.join(&safe), &data).is_err() {
            errors.push(format!("rename failed for {filename}"));
        }
    }
    if errors.is_empty() {
        plain(StatusCode::OK, "success".into())
    } else {
        plain(StatusCode::INTERNAL_SERVER_ERROR, errors.join("\n"))
    }
}

/// POST /plugin/assets/delete?assets=<subdir>&file=<name>
pub fn delete(assets_root: &Path, query: &str) -> Response {
    let file = query_param(query, "file");
    let base = Path::new(&file)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if base.is_empty() {
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "No file specified".into());
    }
    let target = assets_root
        .join(sanitize(&query_param(query, "assets")))
        .join(&base);
    match std::fs::remove_file(&target) {
        Ok(()) => plain(StatusCode::OK, "ok".into()),
        Err(e) => plain(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}
