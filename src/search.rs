//! MiniSearch-compatible site index — a faithful port of wiki-server
//! lib/search.js createIndex + minisearch's toJSON (serializationVersion 2),
//! so wiki-client's `MiniSearch.loadJSON` accepts what we serve.
//!
//! The quirks are deliberate replications, not accidents:
//! - `fieldLength` stores the count of *distinct raw tokens* per field
//!   (pre-lowercasing, and including the empty token a leading separator
//!   produces — extracted content always starts with a space).
//! - `averageFieldLength` is built by the same incremental formula
//!   minisearch uses on sequential add().
//! - the tokenizer splits on /[\n\r\p{Z}\p{P}]+/u exactly.

use regex::Regex;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::OnceLock;

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

/// search.js extractItemText — the regex chain, in order.
fn extract_item_text(text: &str) -> String {
    static R1: OnceLock<Regex> = OnceLock::new();
    static R2: OnceLock<Regex> = OnceLock::new();
    static R4: OnceLock<Regex> = OnceLock::new();
    static R5A: OnceLock<Regex> = OnceLock::new();
    static R5B: OnceLock<Regex> = OnceLock::new();
    static R6: OnceLock<Regex> = OnceLock::new();
    static R7: OnceLock<Regex> = OnceLock::new();
    static R8: OnceLock<Regex> = OnceLock::new();

    let t = re(&R1, r"\[([^\]]*?)\][\[(].*?[\])]").replace_all(text, " $1 ");
    let t = re(&R2, r"\[\{2}|\[(?:\S+)|\]{1,2}").replace_all(&t, " ");
    let t = t.replace('\n', " ");
    let t = re(&R4, r"(?s)<style.*?</style>").replace_all(&t, " ");
    let t = re(&R5A, r#"<(?:"[^"]*"['"]*|'[^']*'['"]*|[^'">])+>"#).replace_all(&t, " ");
    let t = re(&R5B, r"<[^>]+>").replace_all(&t, " ");
    let t = re(&R6, r#"(https?:[^\p{White_Space}\p{Quotation_Mark}]*)"#)
        .replace_all(&t, |c: &regex::Captures| hostname(&c[1]));
    let t = re(&R7, r"[\p{P}\p{Emoji}\p{S}}]+").replace_all(&t, " ");
    let t = re(&R8, r"[\p{White_Space}\n\t]+").replace_all(&t, " ");
    t.into_owned()
}

fn hostname(url: &str) -> String {
    url.split("://")
        .nth(1)
        .map(|rest| {
            rest.split(['/', '?', '#'])
                .next()
                .unwrap_or("")
                .split('@')
                .next_back()
                .unwrap_or("")
                .split(':')
                .next()
                .unwrap_or("")
                .to_string()
        })
        .unwrap_or_else(|| " ".into())
}

/// search.js extractPageText — which item types contribute, and how the
/// media types (audio/video/frame) strip their markup lines first.
pub fn extract_page_text(page: &Value) -> String {
    let mut out = String::new();
    let Some(story) = page["story"].as_array() else {
        return out;
    };
    for item in story {
        let Some(text) = item["text"].as_str() else {
            continue;
        };
        match item["type"].as_str().unwrap_or("") {
            "paragraph" | "markdown" | "html" | "reference" | "image" | "pagefold" | "math"
            | "mathjax" | "code" => {
                out.push(' ');
                out.push_str(&extract_item_text(text));
            }
            "audio" | "video" | "frame" => {
                let kept: Vec<&str> = text
                    .split(['\r', '\n'])
                    .map(|line| {
                        let first = line.split_whitespace().next().unwrap_or("");
                        if first.starts_with("http")
                            || (!first.is_empty() && first == first.to_uppercase())
                            || first.starts_with("//")
                        {
                            ""
                        } else {
                            line
                        }
                    })
                    .collect();
                out.push(' ');
                out.push_str(&extract_item_text(&kept.join(" ")));
            }
            _ => {}
        }
    }
    out
}

/// minisearch's default tokenizer: String.split(/[\n\r\p{Z}\p{P}]+/u) —
/// JS split semantics, so leading/trailing separators yield empty tokens.
fn tokenize(s: &str) -> Vec<String> {
    static SEP: OnceLock<Regex> = OnceLock::new();
    let sep = re(&SEP, r"[\n\r\p{Z}\p{P}]+");
    sep.split(s).map(String::from).collect()
}

/// Build the serialized index for a site's pages — (slug, page) pairs,
/// added in the order given.
pub fn site_index(pages: &[(String, Value)]) -> Value {
    let mut document_ids = Map::new();
    let mut field_length = Map::new();
    let mut avg = [0f64, 0f64];
    // term -> fieldId -> shortId -> freq (BTreeMap: deterministic order)
    let mut index: BTreeMap<String, BTreeMap<usize, BTreeMap<usize, u64>>> = BTreeMap::new();

    for (short_id, (slug, page)) in pages.iter().enumerate() {
        document_ids.insert(short_id.to_string(), Value::String(slug.clone()));
        let title = page["title"].as_str().unwrap_or("").to_string();
        let content = extract_page_text(page);
        let mut lengths = [0u64, 0u64];
        for (field_id, value) in [title, content].into_iter().enumerate() {
            let tokens = tokenize(&value);
            let unique: std::collections::HashSet<&String> = tokens.iter().collect();
            lengths[field_id] = unique.len() as u64;
            // incremental average, minisearch addFieldLength
            let count = short_id as f64;
            avg[field_id] = (avg[field_id] * count + unique.len() as f64) / (count + 1.0);
            for token in &tokens {
                let term = token.to_lowercase();
                if term.is_empty() {
                    continue;
                }
                *index
                    .entry(term)
                    .or_default()
                    .entry(field_id)
                    .or_default()
                    .entry(short_id)
                    .or_insert(0) += 1;
            }
        }
        field_length.insert(
            short_id.to_string(),
            json!([lengths[0], lengths[1]]),
        );
    }

    let index_json: Vec<Value> = index
        .into_iter()
        .map(|(term, fields)| {
            let mut data = Map::new();
            for (field_id, postings) in fields {
                let mut freqs = Map::new();
                for (doc, freq) in postings {
                    freqs.insert(doc.to_string(), json!(freq));
                }
                data.insert(field_id.to_string(), Value::Object(freqs));
            }
            json!([term, data])
        })
        .collect();

    json!({
        "documentCount": pages.len(),
        "nextId": pages.len(),
        "documentIds": document_ids,
        "fieldIds": {"title": 0, "content": 1},
        "fieldLength": field_length,
        "averageFieldLength": [avg[0], avg[1]],
        "storedFields": {},
        "dirtCount": 0,
        "index": index_json,
        "serializationVersion": 2
    })
}
