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
//!
//! With a `cookieSecret` supplied (`--farm-config`, the farm's own config
//! file) the token becomes a STATELESS SIGNED value instead, exactly as the
//! oauth2 path does: `b64({h, exp}).hmac`. That is what a real farm needs —
//! the laptop's private farm pins `cookieSecret` precisely so logins survive
//! a restart, and re-pasting a reclaim code across sixty-odd sites after
//! every restart is not a workflow. The payload deliberately carries an
//! HMAC of the owner secret, never the secret itself: the friend secret IS
//! the reclaim code, so putting it in a cookie would hand the site away.
//! Re-claiming a site changes its secret, which invalidates old cookies for
//! free.
//!
//! `owner.json` is re-read when its stat stamp changes (the pattern
//! indexes.rs already uses). Without that a claim needs a server restart to
//! be seen — harmless single-site, useless across a farm where claiming is
//! the first thing anyone does. Sweet Farm Plan, Phase 1.

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
    /// `--security_type oauth2|social` + `--oidc-config`: the OIDC code
    /// flow (Sweet Keycloak Plan Phase 2). Sessions store ONLY the mapped
    /// identity {id, username, name} — never the provider's tokens — so
    /// the cookie stays bytes-small (BUGS.toml social-fat-token-431 is
    /// the cautionary tale). Authorisation compares the session id to
    /// owner.json's oauth2.id, the shape every real farm site carries.
    OAuth2,
}

pub struct Security {
    pub mode: Mode,
    /// owner.json content: {"name": .., "friend": {"secret": ..}} or the
    /// passportjs/social shape {"name": .., "oauth2": {"id": .., ..}}
    owner: Mutex<Option<serde_json::Value>>,
    /// session token (cookie value) -> friend secret (Friends) or the
    /// serialized mapped identity JSON (OAuth2). Unused once a
    /// `cookie_secret` makes sessions stateless.
    sessions: Mutex<HashMap<String, String>>,
    /// where `owner` was read from, so it can be re-read on change
    id_file: std::path::PathBuf,
    /// (mtime_ns, len) of id_file at the last read; None = file absent
    owner_stamp: Mutex<Option<(u128, u64)>>,
    /// farm `cookieSecret` — present turns friends sessions stateless
    cookie_secret: Option<String>,
}

const COOKIE: &str = "wikiSession";
/// oauth2-mode session cookie — named like the oracle's so the L6
/// cookie-shape assertions (and a future compact-mirror) line up
pub const OAUTH2_COOKIE: &str = "fedwiki.session_token";

const SESSION_TTL_SECS: u64 = 7 * 86400;

fn hmac_hex(secret: &str, payload: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .expect("hmac accepts any key length");
    mac.update(payload.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).ok()
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn rand_hex(n_bytes: usize) -> String {
    let mut buf = vec![0u8; n_bytes];
    getrandom::getrandom(&mut buf).expect("os randomness");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// (mtime_ns, len) — cheap identity for a file, the same shape indexes.rs
/// uses to decide whether a cached index is still the truth.
fn stamp_of(path: &Path) -> Option<(u128, u64)> {
    let md = std::fs::metadata(path).ok()?;
    let ns = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((ns, md.len()))
}

fn read_owner(id_file: &Path) -> Option<serde_json::Value> {
    // retrieveOwner (friends.coffee:47-60): unparsable file -> name-only
    std::fs::read(id_file)
        .ok()
        .map(|data| serde_json::from_slice(&data).unwrap_or(json!({"name": "unparsable"})))
}

impl Security {
    pub fn new(mode: Mode, id_file: &Path) -> Self {
        Self::with_cookie_secret(mode, id_file, None)
    }

    /// As `new`, plus the farm's `cookieSecret` — which makes Friends
    /// sessions stateless (see the module header).
    pub fn with_cookie_secret(mode: Mode, id_file: &Path, cookie_secret: Option<String>) -> Self {
        Security {
            mode,
            owner: Mutex::new(read_owner(id_file)),
            sessions: Mutex::new(HashMap::new()),
            id_file: id_file.to_path_buf(),
            owner_stamp: Mutex::new(stamp_of(id_file)),
            cookie_secret,
        }
    }

    /// Re-read owner.json when its stat stamp has moved. Called from every
    /// reader, so a claim, a reclaim or a hand-edit is visible without a
    /// restart. A file that appears or vanishes counts as a change.
    fn refresh_owner(&self) {
        let now = stamp_of(&self.id_file);
        let mut stamp = self.owner_stamp.lock().unwrap();
        if *stamp == now {
            return;
        }
        *stamp = now;
        *self.owner.lock().unwrap() = read_owner(&self.id_file);
    }

    pub fn owner_name(&self) -> Option<String> {
        self.refresh_owner();
        self.owner
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|o| o["name"].as_str().map(String::from))
    }

    fn owner_secret(&self) -> Option<String> {
        self.refresh_owner();
        self.owner
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|o| o["friend"]["secret"].as_str().map(String::from))
    }

    fn cookie_value(cookie_header: Option<&str>, name: &str) -> Option<String> {
        cookie_header?.split(';').find_map(|part| {
            let (k, v) = part.trim().split_once('=')?;
            (k == name && !v.is_empty()).then(|| v.to_string())
        })
    }

    /// The `wikiSession` value carried by a Cookie header.
    fn session_token(cookie_header: Option<&str>) -> Option<String> {
        Self::cookie_value(cookie_header, COOKIE)
    }

    fn session_friend(&self, cookie_header: Option<&str>) -> Option<String> {
        let token = Self::session_token(cookie_header)?;
        match &self.cookie_secret {
            // stateless: the cookie proves it was minted for THIS site's
            // current owner secret, and the secret itself never left here
            Some(key) => {
                let (body, sig) = token.rsplit_once('.')?;
                if hmac_hex(key, body) != sig {
                    return None;
                }
                let claim: serde_json::Value =
                    serde_json::from_slice(&b64_decode(body)?).ok()?;
                if claim["exp"].as_u64()? < now_secs() {
                    return None;
                }
                let secret = self.owner_secret()?;
                (claim["h"].as_str()? == hmac_hex(key, &secret)).then_some(secret)
            }
            None => self.sessions.lock().unwrap().get(&token).cloned(),
        }
    }

    /// getUser — friends returns `true` when a session friend exists
    /// (friends.coffee:82-86), the stub always '' (security.js:68-70).
    /// (OAuth2 identity is decoded from the signed cookie in main.rs —
    /// oidc::decode_session — not through this seam.)
    pub fn user(&self, cookie_header: Option<&str>) -> bool {
        matches!(self.mode, Mode::Friends) && self.session_friend(cookie_header).is_some()
    }

    /// The mode as the Sweet Client's padlock spells it — the client
    /// cannot draw a login door without knowing which door it is
    /// (`/api/me`). oauth2 is reported by sweetmode from `site.oidc`,
    /// since Mode::OAuth2 authorises through oauth2_is_authorized.
    pub fn mode_name(&self) -> &'static str {
        match self.mode {
            Mode::Default => "default",
            Mode::Legacy => "legacy",
            Mode::Friends => "friends",
            Mode::OAuth2 => "oauth2",
        }
    }

    /// isAuthorized — gates every write and the recycler reads.
    pub fn is_authorized(&self, cookie_header: Option<&str>) -> bool {
        self.refresh_owner();
        match self.mode {
            Mode::Default => false,
            // stub: anonymous edit only while unclaimed (security.js:72-84)
            Mode::Legacy => self.owner.lock().unwrap().is_none(),
            Mode::Friends => match (self.session_friend(cookie_header), self.owner_secret()) {
                (Some(friend), Some(secret)) => friend == secret,
                _ => false,
            },
            // oauth2 authorisation goes through oauth2_is_authorized with
            // the identity main.rs decoded from the signed cookie
            Mode::OAuth2 => false,
        }
    }

    // ------------------------------------------------------- oauth2 mode

    /// social.js isAuthorized with a pre-decoded identity: unclaimed ->
    /// open for everyone; claimed -> the identity must match
    /// owner.oauth2.id (string compare both sides, the oracle's
    /// toString()).
    pub fn oauth2_is_authorized(&self, identity: Option<&serde_json::Value>) -> bool {
        let owner = self.owner.lock().unwrap().clone();
        match owner {
            None => true,
            Some(o) => match identity {
                Some(u) => {
                    let oid = json_as_string(&o["oauth2"]["id"]);
                    !oid.is_empty() && oid == json_as_string(&u["id"])
                }
                None => false,
            },
        }
    }

    /// GET /auth/claim-wiki (social.js): owner absent -> write owner.json
    /// in the passportjs shape and answer the owner name; present -> 403.
    pub fn oauth2_claim(
        &self,
        id_file: &Path,
        identity: &serde_json::Value,
    ) -> Result<String, u16> {
        let mut owner = self.owner.lock().unwrap();
        if owner.is_some() {
            return Err(403);
        }
        let name = identity["name"]
            .as_str()
            .or(identity["username"].as_str())
            .unwrap_or("")
            .to_string();
        let id = json!({
            "name": name,
            "oauth2": {"id": identity["id"], "username": identity["username"]},
        });
        let pretty = serde_json::to_string_pretty(&id).unwrap();
        // the node server pre-creates status/ at boot; a fresh sweet site
        // may not have it yet (claim 500'd on a bare data dir — live find)
        if let Some(dir) = id_file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(id_file, pretty).is_err() {
            return Err(500);
        }
        *owner = Some(id);
        Ok(name)
    }


    fn grant_session(&self, secret: &str) -> String {
        let Some(key) = self.cookie_secret.as_deref() else {
            let token = rand_hex(24);
            self.sessions
                .lock()
                .unwrap()
                .insert(token.clone(), secret.to_string());
            return token;
        };
        let claim = json!({"h": hmac_hex(key, secret), "exp": now_secs() + SESSION_TTL_SECS});
        let body = b64(claim.to_string().as_bytes());
        format!("{body}.{}", hmac_hex(key, &body))
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
            // stateless sessions have nothing to forget here — the cleared
            // cookie below IS the logout, exactly as client-sessions reset()
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

/// JS toString() for the id comparison: strings bare, numbers printed.
fn json_as_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        _ => String::new(),
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
