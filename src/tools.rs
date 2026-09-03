//! Customisable tool pages — the search page and the page-actions page.
//!
//! The house farms get these today from `wiki-plugin-hitchhiker`, which can
//! only reach the client by forking the security module: a security module
//! is the one client hook that auto-runs on every page, so
//! `wiki-security-hitchhiker` exists purely so its `client/security.js` can
//! inject `<script src=/plugins/hitchhiker/wiki-ux.js>`. The tool pages are
//! then BUILT IN JAVASCRIPT, so customising them means editing plugin code.
//!
//! Sweet needs none of that. It writes its own shell (so it can carry the
//! script itself) and it already has the three-rung page fallback — site
//! `pages/`, then `--defaults`, then a plugin's `pages/`. So a tool page is
//! simply a WIKI PAGE: author `pages/page-tools` on a site and it wins over
//! the default, edited in the wiki like anything else, no restart.
//!
//! One authored page has to serve whichever page it was opened from, so a
//! small, explicit placeholder set is substituted on the way out.

use crate::{as_slug, load_page, load_plugin_page, Site};
use serde_json::Value;

pub const DEFAULT_SEARCH_SLUG: &str = "search-tool";
pub const DEFAULT_PAGE_SLUG: &str = "page-tools";

/// Fourth rung, below the plugin pages: templates baked into the binary so
/// the tools work on a bare site with no defaults dir and no plugins. A site
/// overrides either by authoring a page of the same slug.
/// The client half, served at /system/sweet-ux.js and injected into the
/// shell — the whole reason no security-module fork is needed.
pub const SCRIPT: &str = include_str!("../client/sweet-ux.js");

const BUILTIN_SEARCH: &str = include_str!("../pages/search-tool");
const BUILTIN_PAGE: &str = include_str!("../pages/page-tools");

fn builtin(kind: &str) -> Option<&'static str> {
    match kind {
        "search" => Some(BUILTIN_SEARCH),
        "page" => Some(BUILTIN_PAGE),
        _ => None,
    }
}

/// `--tool-pages` and the two slug overrides. Off by default: with `enabled`
/// false the shell is byte-for-byte what it has always been and
/// `/system/tool/...` 404s exactly as it did before this module existed.
#[derive(Clone, Default)]
pub struct Config {
    pub enabled: bool,
    pub search: Option<String>,
    pub page: Option<String>,
}

impl Config {
    /// The template slug for a tool kind, or None if the kind is unknown.
    pub fn slug_for(&self, kind: &str) -> Option<&str> {
        match kind {
            "search" => Some(self.search.as_deref().unwrap_or(DEFAULT_SEARCH_SLUG)),
            "page" => Some(self.page.as_deref().unwrap_or(DEFAULT_PAGE_SLUG)),
            _ => None,
        }
    }
}

/// Substitute the placeholder set through every string in the value —
/// titles, item text, and any field a plugin item happens to carry, so an
/// author can put {{slug}} wherever that item type needs it.
fn substitute(v: &mut Value, subs: &[(&str, &str)]) {
    match v {
        Value::String(s) => {
            if s.contains("{{") {
                for (k, val) in subs {
                    *s = s.replace(k, val);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| substitute(x, subs)),
        Value::Object(o) => o.iter_mut().for_each(|(_, x)| substitute(x, subs)),
        _ => {}
    }
}

/// Title and item ids of the page the tool was opened from. The ids feed
/// {{ids}}, which is what lets the default search template reproduce the
/// client's own `SEARCH ANY ITEMS` (legacy.js:385).
fn source_facts(site: &Site, slug: &str) -> (String, String) {
    let page = load_page(site, slug)
        .or_else(|| load_plugin_page(site, slug))
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let title = page
        .as_ref()
        .and_then(|p| p["title"].as_str())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| slug.replace('-', " "));
    let ids = page
        .as_ref()
        .and_then(|p| p["story"].as_array())
        .map(|a| {
            a.iter()
                .filter_map(|i| i["id"].as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    (title, ids)
}

/// Resolve a tool page for `kind`, opened from `from_slug`.
///
/// The template walks the same fallback chain as any other read, so a site
/// overrides the farm default simply by authoring the page. Returns None for
/// an unknown kind or when no template exists anywhere — the caller 404s and
/// the client falls back to its built-in behaviour.
pub fn resolve(site: &Site, kind: &str, from_slug: &str, host: &str, proto: &str) -> Option<Value> {
    if from_slug.is_empty() || from_slug != as_slug(from_slug) {
        return None;
    }
    let template_slug = site.tools.slug_for(kind)?;
    let mut page: Value = match load_page(site, template_slug)
        .or_else(|| load_plugin_page(site, template_slug))
    {
        Some(bytes) => serde_json::from_slice(&bytes).ok()?,
        None => serde_json::from_str(builtin(kind)?).ok()?,
    };

    let (title, ids) = source_facts(site, from_slug);
    let origin = format!("{proto}://{host}");
    substitute(
        &mut page,
        &[
            ("{{title}}", &title),
            ("{{slug}}", from_slug),
            ("{{site}}", host),
            ("{{origin}}", &origin),
            ("{{ids}}", &ids),
        ],
    );
    Some(page)
}
