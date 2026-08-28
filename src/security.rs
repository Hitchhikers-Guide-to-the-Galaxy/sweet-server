//! Security — the pluggable owner/claim model (spec §6).
//!
//! Three modes mirroring the reference:
//! - `Default`: the inert stub — every write 403s (lib/security.js).
//! - `Legacy` (`--security_legacy`): anonymous edit while the site is
//!   unclaimed (owner.json absent); read-only once claimed.
//! - `Friends` (`--security_type friends`): the claim flow the ecosystem
//!   actually uses (wiki-security-friends/server/friends.coffee) — POST
//!   /login claims, GET /logout resets, POST /auth/reclaim/ re-authorizes
//!   with the owner secret.
//!
//! Sessions ride a `wikiSession` cookie (httpOnly, SameSite=Lax) like the
//! reference's client-sessions middleware (server.js:287-315). The reference
//! stores the session *inside* an encrypted cookie with a per-boot secret;
//! we keep an in-process token map — behaviourally identical for a single
//! server lifetime (both forget sessions on restart unless configured).

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;
use serde_json::json;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

pub enum Mode {
    Default,
    Legacy,
    Friends,
}

pub struct Security {
    pub mode: Mode,
    /// owner.json content: {"name": .., "friend": {"secret": ..}}
    owner: Mutex<Option<serde_json::Value>>,
    /// session token (cookie value) -> friend secret
    sessions: Mutex<HashMap<String, String>>,
}

const COOKIE: &str = "wikiSession";

fn rand_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    getrandom::getrandom(&mut buf).expect("os randomness");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

impl Security {
    pub fn new(mode: Mode, id_file: &Path) -> Self {
        // retrieveOwner (friends.coffee:47-60): unparsable file -> name-only
        let owner = std::fs::read(id_file).ok().map(|data| {
            serde_json::from_slice(&data).unwrap_or(json!({"name": "unparsable"}))
        });
        Security {
            mode,
            owner: Mutex::new(owner),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn owner_name(&self) -> Option<String> {
        self.owner
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|o| o["name"].as_str().map(String::from))
    }

    fn owner_secret(&self) -> Option<String> {
        self.owner
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|o| o["friend"]["secret"].as_str().map(String::from))
    }

    /// The `wikiSession` value carried by a Cookie header.
    fn session_token(cookie_header: Option<&str>) -> Option<String> {
        cookie_header?.split(';').find_map(|part| {
            let (k, v) = part.trim().split_once('=')?;
            (k == COOKIE && !v.is_empty()).then(|| v.to_string())
        })
    }

    fn session_friend(&self, cookie_header: Option<&str>) -> Option<String> {
        let token = Self::session_token(cookie_header)?;
        self.sessions.lock().unwrap().get(&token).cloned()
    }

    /// getUser — friends returns `true` when a session friend exists
    /// (friends.coffee:82-86), the stub always '' (security.js:68-70).
    pub fn user(&self, cookie_header: Option<&str>) -> bool {
        matches!(self.mode, Mode::Friends) && self.session_friend(cookie_header).is_some()
    }

    /// isAuthorized — gates every write and the recycler reads.
    pub fn is_authorized(&self, cookie_header: Option<&str>) -> bool {
        match self.mode {
            Mode::Default => false,
            // stub: anonymous edit only while unclaimed (security.js:72-84)
            Mode::Legacy => self.owner.lock().unwrap().is_none(),
            Mode::Friends => match (self.session_friend(cookie_header), self.owner_secret()) {
                (Some(friend), Some(secret)) => friend == secret,
                _ => false,
            },
        }
    }

    fn grant_session(&self, secret: &str) -> String {
        let token = rand_hex(24);
        self.sessions
            .lock()
            .unwrap()
            .insert(token.clone(), secret.to_string());
        token
    }

    fn set_cookie(token: &str) -> String {
        // shape of the client-sessions cookie (server.js:287-295):
        // httpOnly, SameSite=Lax, 7-day duration
        format!("{COOKIE}={token}; Path=/; Max-Age=604800; HttpOnly; SameSite=Lax")
    }

    /// POST /login — claim an unclaimed site (friends.coffee:104-124).
    pub fn login(&self, id_file: &Path) -> Response {
        if !matches!(self.mode, Mode::Friends) {
            return plain(StatusCode::NOT_FOUND, "Page not found");
        }
        let mut owner = self.owner.lock().unwrap();
        if owner.is_some() {
            return plain(StatusCode::NOT_IMPLEMENTED, "Not Implemented");
        }
        let secret = rand_hex(32);
        let nick = nickname(&secret);
        let id = json!({"name": nick, "friend": {"secret": secret}});
        // JSON.stringify(id, null, "  ") — two-space indent
        let pretty = serde_json::to_string_pretty(&id).unwrap();
        if let Err(e) = std::fs::write(id_file, pretty) {
            eprintln!("Failed to claim wiki: {e}");
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        }
        *owner = Some(id);
        drop(owner);
        let token = self.grant_session(&secret);
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .header(header::SET_COOKIE, Self::set_cookie(&token))
            .body(Body::from(
                serde_json::to_string(&json!({"ownerName": nick})).unwrap(),
            ))
            .unwrap()
    }

    /// GET /logout — reset the session (friends.coffee:126-129).
    pub fn logout(&self, cookie_header: Option<&str>) -> Response {
        if !matches!(self.mode, Mode::Friends) {
            return plain(StatusCode::NOT_FOUND, "Page not found");
        }
        if let Some(token) = Self::session_token(cookie_header) {
            self.sessions.lock().unwrap().remove(&token);
        }
        Response::builder()
            .status(StatusCode::OK)
            // client-sessions reset() hands back a fresh empty cookie
            .header(
                header::SET_COOKIE,
                format!("{COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax"),
            )
            .body(Body::from("OK"))
            .unwrap()
    }

    /// POST /auth/reclaim/ — raw body is the reclaim code
    /// (friends.coffee:131-145): match -> session; miss -> 401;
    /// unclaimed (owner.friend throws) -> 500.
    pub fn reclaim(&self, body: &[u8]) -> Response {
        if !matches!(self.mode, Mode::Friends) {
            return plain(StatusCode::NOT_FOUND, "Page not found");
        }
        let Some(secret) = self.owner_secret() else {
            return plain(StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error");
        };
        let code = String::from_utf8_lossy(body);
        if code == secret {
            let token = self.grant_session(&secret);
            Response::builder()
                .status(StatusCode::OK)
                .header(header::SET_COOKIE, Self::set_cookie(&token))
                .body(Body::empty())
                .unwrap()
        } else {
            plain(StatusCode::UNAUTHORIZED, "Unauthorized")
        }
    }
}

fn plain(status: StatusCode, text: &str) -> Response {
    Response::builder()
        .status(status)
        .body(Body::from(text.to_string()))
        .unwrap()
}

pub fn forbidden() -> Response {
    // express res.sendStatus(403) (server.js:535)
    plain(StatusCode::FORBIDDEN, "Forbidden")
}

// ------------------------------------------------- nickname (friends.coffee:35-40)
//
// A six-letter CVCVCV name derived deterministically from the secret via
// David Bau's seedrandom (ARC4-based). Ported exactly so a claim performed
// by this server names the owner identically to the reference.

/// seedrandom's mixkey (seedrandom.js:178-186): JS `^=` coerces the unset
/// slot (undefined * 19 = NaN) to 0 via ToInt32.
fn mixkey(seed: &str, key: &mut Vec<i64>) {
    let chars: Vec<u16> = seed.encode_utf16().collect();
    let mut smear: i32 = 0;
    for (j, &code) in chars.iter().enumerate() {
        let slot = j & 255;
        while key.len() <= slot {
            key.push(-1); // JS: undefined
        }
        let prior = if key[slot] < 0 { 0i32 } else { (key[slot] * 19) as i32 };
        smear ^= prior;
        key[slot] = (smear.wrapping_add(code as i32) & 255) as i64;
    }
}

struct Arc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Arc4 {
    fn new(key: &[i64]) -> Self {
        let key: Vec<u8> = if key.is_empty() {
            vec![0]
        } else {
            key.iter().map(|&k| (k.max(0) & 255) as u8).collect()
        };
        let mut s = [0u8; 256];
        for (i, v) in s.iter_mut().enumerate() {
            *v = i as u8;
        }
        let mut j: u8 = 0;
        for i in 0..256usize {
            let t = s[i];
            j = j.wrapping_add(key[i % key.len()]).wrapping_add(t);
            s[i] = s[j as usize];
            s[j as usize] = t;
        }
        let mut me = Arc4 { s, i: 0, j: 0 };
        me.g(256); // RC4-drop[256]
        me
    }

    fn g(&mut self, count: usize) -> f64 {
        let mut r = 0f64;
        for _ in 0..count {
            self.i = self.i.wrapping_add(1);
            let t = self.s[self.i as usize];
            self.j = self.j.wrapping_add(t);
            self.s[self.i as usize] = self.s[self.j as usize];
            self.s[self.j as usize] = t;
            let idx = self.s[self.i as usize].wrapping_add(self.s[self.j as usize]);
            r = r * 256.0 + self.s[idx as usize] as f64;
        }
        r
    }
}

/// seedrandom(seed)() — a double in [0,1) (seedrandom.js:58-74).
struct SeedRandom(Arc4);

impl SeedRandom {
    fn new(seed: &str) -> Self {
        let mut key = Vec::new();
        mixkey(seed, &mut key);
        SeedRandom(Arc4::new(&key))
    }

    fn next(&mut self) -> f64 {
        let significance = 2f64.powi(52);
        let overflow = significance * 2.0;
        let mut n = self.0.g(6);
        let mut d = 256f64.powi(6);
        let mut x = 0u32;
        while n < significance {
            n = (n + x as f64) * 256.0;
            d *= 256.0;
            x = self.0.g(1) as u32;
        }
        while n >= overflow {
            n /= 2.0;
            d /= 2.0;
            x >>= 1;
        }
        (n + x as f64) / d
    }
}

pub fn nickname(seed: &str) -> String {
    let c: Vec<char> = "bcdfghjklmnprstvwy".chars().collect();
    let v: Vec<char> = "aeiou".chars().collect();
    let mut rn = SeedRandom::new(seed);
    let mut ch = |set: &[char]| set[(rn.next() * set.len() as f64) as usize];
    let mut out = String::new();
    for _ in 0..3 {
        out.push(ch(&c));
        out.push(ch(&v));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::nickname;

    #[test]
    fn nickname_matches_reference_vectors() {
        // first vector observed live from wiki-security-friends on the
        // vendored reference; the rest generated with the vendored
        // seedrandom via node (see conformance harness notes)
        assert_eq!(
            nickname("384a2d48a9e0d60c1cdf06d7304f6b11426e0dcf50701fded8ec6cbd274e2f5b"),
            "tapisu"
        );
        assert_eq!(nickname(&"0".repeat(64)), "tamivo");
        assert_eq!(nickname(&"deadbeef".repeat(8)), "cekudi");
        assert_eq!(nickname("a"), "kijino");
    }
}
