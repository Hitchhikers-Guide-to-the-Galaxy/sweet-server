//! The write path — "the journal is the contract".
//!
//! PUT /page/:slug/action applies the six-verb grammar exactly as
//! wiki-server 0.27.0 does (server.js:847-965), including the traps the
//! spec's §11 lists: the form-encoded body whose `action` field is a JSON
//! string; add-with-unknown-`after` splicing at index 0; the implicit fork
//! (any action carrying `action.fork`) prepending a fork journal entry
//! dated action.date - 1; fork saving the current page to the recycler
//! before replacing it; create answering 409 against default-data pages
//! too; error bodies of the exact "Server <err>" shape.

use serde_json::{json, Value};
use std::path::PathBuf;

use crate::Site;

pub enum Outcome {
    Ok,                          // 200 "ok"
    Error(u16, String),          // res.e(): "Server <err>", or 409 body
}

fn err(status: u16, msg: &str) -> Outcome {
    Outcome::Error(status, format!("Server {msg}"))
}

/// Percent-decode one application/x-www-form-urlencoded value.
fn form_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(
                    std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
                {
                    out.push(b);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Extract the `action` field from a form-encoded body.
pub fn parse_action(body: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(body).ok()?;
    for pair in text.split('&') {
        if let Some(v) = pair.strip_prefix("action=") {
            return serde_json::from_str(&form_decode(v)).ok();
        }
    }
    None
}

fn page_file(site: &Site, slug: &str) -> PathBuf {
    site.data.join("pages").join(slug)
}

fn recycle_dir(site: &Site) -> PathBuf {
    site.data.join("recycle")
}

fn save_page(site: &Site, slug: &str, page: &Value) -> std::io::Result<()> {
    std::fs::create_dir_all(site.data.join("pages"))?;
    std::fs::write(page_file(site, slug), serde_json::to_vec_pretty(page)?)
}

/// Copy the current page (if any) into the recycler — the step every fork
/// takes before replacing the page.
pub fn save_to_recycler(site: &Site, slug: &str) -> std::io::Result<()> {
    let src = page_file(site, slug);
    if src.exists() {
        std::fs::create_dir_all(recycle_dir(site))?;
        std::fs::copy(&src, recycle_dir(site).join(slug))?;
    }
    Ok(())
}

fn remote_get(host: &str, slug: &str) -> Option<Value> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(2))
        .build();
    let resp = agent.get(&format!("http://{host}/{slug}.json")).call().ok()?;
    let mut body = Vec::new();
    use std::io::Read;
    resp.into_reader().read_to_end(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}

/// The read chain for a page about to be written: site copy first, then
/// default-data (copy-on-write — the action materialises the default).
fn load_for_write(site: &Site, slug: &str) -> Option<Value> {
    let site_file = page_file(site, slug);
    let bytes = std::fs::read(&site_file).ok().or_else(|| {
        site.defaults
            .as_ref()
            .and_then(|d| std::fs::read(d.join("pages").join(slug)).ok())
    })?;
    serde_json::from_slice(&bytes).ok()
}

pub fn apply(site: &Site, slug: &str, mut action: Value) -> Outcome {
    // ---- resolve the page the action operates on (server.js:920-963)
    let implicit_fork = action.get("fork").and_then(|f| f.as_str()).map(String::from);
    let atype = action["type"].as_str().unwrap_or("").to_string();

    let mut page: Value = if let Some(fork_site) = &implicit_fork {
        let _ = save_to_recycler(site, slug);
        if action.get("forkPage").is_some() {
            let fp = action["forkPage"].clone();
            action.as_object_mut().unwrap().remove("forkPage");
            fp
        } else {
            match remote_get(fork_site, slug) {
                Some(p) => p,
                None => return err(404, "Page not found"),
            }
        }
    } else if atype == "create" {
        if load_for_write(site, slug).is_some() {
            return Outcome::Error(409, "Page already exists.".into());
        }
        action["item"].clone()
    } else if atype == "fork" {
        let _ = save_to_recycler(site, slug);
        if action.get("forkPage").is_some() {
            let fp = action["forkPage"].clone();
            action.as_object_mut().unwrap().remove("forkPage");
            fp
        } else {
            let fork_site = action["site"].as_str().unwrap_or("");
            match remote_get(fork_site, slug) {
                Some(p) => p,
                None => return err(404, "Page not found"),
            }
        }
    } else {
        match load_for_write(site, slug) {
            Some(p) => p,
            None => return err(404, "Page not found"),
        }
    };

    // ---- apply the verb to the story (server.js:858-895)
    let story = page["story"].as_array().cloned().unwrap_or_default();
    let new_story: Result<Vec<Value>, &'static str> = match atype.as_str() {
        "move" => {
            let order: Vec<&str> = action["order"]
                .as_array()
                .map(|o| o.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let mut out = Vec::with_capacity(order.len());
            let mut ok = true;
            for id in order {
                match story.iter().find(|p| p["id"] == id) {
                    Some(item) => out.push(item.clone()),
                    None => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok { Ok(out) } else { Err("Ignoring move. Try reload.") }
        }
        "add" => {
            // indexOf(after) + 1: unknown after -> index 0 (spec trap 4)
            let idx = story
                .iter()
                .position(|p| p["id"] == action["after"])
                .map(|i| i + 1)
                .unwrap_or(0);
            let mut out = story.clone();
            out.insert(idx.min(out.len()), action["item"].clone());
            Ok(out)
        }
        "remove" => Ok(story
            .iter()
            .filter(|p| p["id"] != action["id"])
            .cloned()
            .collect()),
        "edit" => Ok(story
            .iter()
            .map(|p| {
                if p["id"] == action["id"] {
                    action["item"].clone()
                } else {
                    p.clone()
                }
            })
            .collect()),
        "create" | "fork" => Ok(story),
        _ => Err("Unfamiliar action ignored"),
    };
    let new_story = match new_story {
        Ok(s) => s,
        Err(e) => return err(500, e),
    };
    page["story"] = Value::Array(new_story);

    // ---- journal (server.js:899-908)
    if !page["journal"].is_array() {
        page["journal"] = json!([]);
    }
    if let Some(fork_site) = implicit_fork {
        let date = action["date"].as_i64().unwrap_or(0) - 1;
        page["journal"].as_array_mut().unwrap().push(
            json!({"type": "fork", "site": fork_site, "date": date}));
        action.as_object_mut().unwrap().remove("fork");
    }
    page["journal"].as_array_mut().unwrap().push(action);

    match save_page(site, slug, &page) {
        Ok(()) => {
            // server.js:912-917 — sitemaphandler/searchhandler.update after
            // every action; without it a prebuilt status index goes stale
            crate::indexes::write_update(site, slug, Some(&page));
            Outcome::Ok
        }
        Err(e) => err(500, &e.to_string()),
    }
}

// ---------------------------------------------------------------- recycler

pub fn delete_page(site: &Site, slug: &str) -> Result<(), (u16, String)> {
    let src = page_file(site, slug);
    if !src.exists() {
        return Err((404, "Page not found".into()));
    }
    std::fs::create_dir_all(recycle_dir(site))
        .and_then(|_| std::fs::rename(&src, recycle_dir(site).join(slug)))
        .map_err(|e| (500, format!("Server {e}")))
        .map(|()| crate::indexes::write_update(site, slug, None))
}

pub fn recycler_slugs(site: &Site) -> Value {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(recycle_dir(site)) {
        let mut names: Vec<_> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        for slug in names {
            if let Ok(bytes) = std::fs::read(recycle_dir(site).join(&slug)) {
                if let Ok(p) = serde_json::from_slice::<Value>(&bytes) {
                    out.push(json!({"slug": slug, "title": p["title"]}));
                }
            }
        }
    }
    Value::Array(out)
}

pub fn recycler_page(site: &Site, slug: &str) -> Option<Vec<u8>> {
    std::fs::read(recycle_dir(site).join(slug)).ok()
}

pub fn recycler_purge(site: &Site, slug: &str) -> Result<(), (u16, String)> {
    std::fs::remove_file(recycle_dir(site).join(slug))
        .map_err(|_| (404, "Page not found".into()))
}
