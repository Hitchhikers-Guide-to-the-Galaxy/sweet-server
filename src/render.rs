//! Server-side story rendering for the generated page route (`/:slug.html`).
//!
//! The reference builds this in `wiki-server/lib/server.js:89` — but its
//! `page.story.map()` constructs an `f.div(...)` in every branch and
//! **returns none of them**, so `.join('\n')` yields only newlines and the
//! server-generated story is empty. That is the "0.27.0 story rendering is a
//! spec BUG" the watermark records; registered here as
//! `static-html-story-discarded` and deliberately NOT reproduced — we render
//! what the reference meant to.
//!
//! `resolve_links` is a faithful port of `wiki-client/lib/resolve.js:27-66`,
//! including its four-phase marker dance, so the anchors are byte-identical
//! to the ones the client produces.

use crate::as_slug;
use regex::{Captures, Regex};
use serde_json::Value;
use std::sync::OnceLock;

fn marker() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"〖(\d+)〗").unwrap())
}

fn internal_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"\[\[([^\]]+)\]\]").unwrap())
}

fn external_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    // resolve.js: /\[((?:(?:https?|ftp):|\/).*?) (.*?)\]/gi
    R.get_or_init(|| Regex::new(r"\[((?:(?:https?|ftp):|/).*?) (.*?)\]").unwrap())
}

/// resolve.escape — ampersand and angle brackets only, nothing else.
pub fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// resolve.resolveLinks with the default (escape) sanitizer.
///
/// Four phases, in the reference's order: adulterate any markers already in
/// the text, convert links and stash them away already-escaped, escape what
/// remains, then put the links back. The stash is what keeps generated
/// anchors from being escaped along with the prose.
///
/// `resolutionContext` is a client-side lineup concept, so the `title`
/// attribute renders empty here — which is what the reference emits when
/// nothing has been pushed onto the context.
pub fn resolve_links(text: &str) -> String {
    let mut stashed: Vec<String> = Vec::new();

    let s = marker().replace_all(text, "〖 $1 〗").into_owned();

    let s = internal_re()
        .replace_all(&s, |c: &Captures| {
            let name = &c[1];
            let slug = as_slug(name);
            if slug.is_empty() {
                return c[0].to_string(); // reference returns the raw match
            }
            let styling = if name == name.trim() {
                "internal"
            } else {
                "internal spaced"
            };
            let here = stashed.len();
            stashed.push(format!(
                "<a class=\"{styling}\" href=\"/{slug}.html\" data-page-name=\"{slug}\" title=\"\">{}</a>",
                escape(name)
            ));
            format!("〖{here}〗")
        })
        .into_owned();

    let s = external_re()
        .replace_all(&s, |c: &Captures| {
            let href = &c[1];
            let rest = &c[2];
            let here = stashed.len();
            stashed.push(format!(
                "<a class=\"external\" target=\"_blank\" href=\"{href}\" title=\"{href}\" rel=\"noopener\">{} <img src=\"/images/external-link-ltr-icon.png\"></a>",
                escape(rest)
            ));
            format!("〖{here}〗")
        })
        .into_owned();

    let escaped = escape(&s);
    marker()
        .replace_all(&escaped, |c: &Captures| {
            c[1].parse::<usize>()
                .ok()
                .and_then(|i| stashed.get(i).cloned())
                .unwrap_or_default()
        })
        .into_owned()
}

fn text_of(item: &Value) -> &str {
    item["text"].as_str().unwrap_or("")
}

/// One story item, in the shape server.js:89 builds (and drops). Only
/// paragraph, image and html carry a type in the class — everything else is
/// a bare `item`, exactly as the reference's else-branch writes it.
fn render_item(item: &Value) -> String {
    if !item.is_object() {
        return String::new(); // reference: `if (!story) return ''`
    }
    match item["type"].as_str().unwrap_or("") {
        "paragraph" => format!(
            "<div class=\"item paragraph\"><p>{}</p></div>",
            resolve_links(text_of(item))
        ),
        "image" => {
            let caption = item["text"]
                .as_str()
                .filter(|s| !s.is_empty())
                .or_else(|| item["caption"].as_str().filter(|s| !s.is_empty()))
                .unwrap_or("uploaded image");
            format!(
                "<div class=\"item image\"><img class=\"thumbnail\" src=\"{}\"><p>{}</p></div>",
                item["url"].as_str().unwrap_or(""),
                resolve_links(caption)
            )
        }
        // The reference sanitizes html items with DOMPurify. There is no
        // DOMPurify here and an allowlist sanitizer is out of scope, so an
        // html item is escaped like any other text — never emitted raw.
        // Deliberate narrowing, recorded in BUGS.toml.
        "html" => format!(
            "<div class=\"item html\"><p>{}</p></div>",
            resolve_links(text_of(item))
        ),
        _ => format!(
            "<div class=\"item\"><p>{}</p></div>",
            resolve_links(text_of(item))
        ),
    }
}

/// The full generated `.paper` contents: twins, header and story, joined the
/// way server.js:89 joins them.
pub fn render_story(page: &Value, title: &str) -> String {
    let items: Vec<String> = page["story"]
        .as_array()
        .map(|a| a.iter().map(render_item).collect())
        .unwrap_or_default();
    format!(
        "<div class=\"twins\"><p></p></div>\n\
         <div class=\"header\"><h1><a href=\"/\" style=\"text-decoration: none\">\
         <img height=\"32px\" src=\"/favicon.png\"></a> {}</h1></div>\n\
         <div class=\"story\">{}</div>",
        escape(title),
        items.join("\n")
    )
}
