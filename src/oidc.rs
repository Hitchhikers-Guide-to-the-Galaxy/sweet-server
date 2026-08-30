//! Mode::OAuth2 — the OIDC authorization-code flow (Sweet Keycloak Plan
//! Phase 2), route-compatible with wiki-security-social (the L6 oracle)
//! so the same security client drives either server.
//!
//! Engine: the `openidconnect` crate — discovery, PKCE, nonce, and
//! id_token signature verification against the provider's JWKS, i.e.
//! strictly more validation than the oracle performs (which fetches
//! neither userinfo nor JWKS — OIDC-SECURITY-SPEC.md §wire-flow). The
//! mapped identity fields are read from the userinfo endpoint's raw JSON
//! so oauth2_IdField/UsernameField/DisplayNameField keep their exact
//! plain-field semantics.
//!
//! Sessions: security.rs stores ONLY the mapped identity behind a
//! `fedwiki.session_token` cookie — never the provider's tokens — so the
//! cookie set stays small (BUG social-fat-token-431 is what happens when
//! a security layer parks raw Keycloak JWTs in cookies).

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::{Auth, Site};

pub struct Oidc {
    pub cfg: Cfg,
    meta: Mutex<Option<CoreProviderMetadata>>,
    pending: Mutex<HashMap<String, Pending>>,
}

pub struct Cfg {
    pub client_id: String,
    client_secret: String,
    /// the full .well-known/openid-configuration URL, as the wiki config
    /// spells it (oauth2_discoveryUrl)
    discovery_url: String,
    id_field: String,
    username_field: String,
    display_field: String,
    pub use_https: bool,
    /// admin identity: the configured admin.oauth2 value (spec §authz)
    admin_id: Option<String>,
}

struct Pending {
    nonce: Nonce,
    verifier: PkceCodeVerifier,
    callback: String,
}

impl Oidc {
    /// Read the oauth2_* keys from a wiki-shaped config JSON — the same
    /// file the node farms already carry, so deployment reuses the
    /// existing mode-600 secret file rather than putting the client
    /// secret in argv.
    pub fn from_config_file(path: &std::path::Path) -> Result<Oidc, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{path:?}: {e}"))?;
        let cfg: Value =
            serde_json::from_slice(&bytes).map_err(|e| format!("{path:?}: {e}"))?;
        let need = |k: &str| {
            cfg[k]
                .as_str()
                .map(String::from)
                .ok_or_else(|| format!("--oidc-config {path:?} missing {k}"))
        };
        Ok(Oidc {
            cfg: Cfg {
                client_id: need("oauth2_clientID")?,
                client_secret: need("oauth2_clientSecret")?,
                discovery_url: need("oauth2_discoveryUrl")?,
                id_field: cfg["oauth2_IdField"].as_str().unwrap_or("sub").into(),
                username_field: cfg["oauth2_UsernameField"]
                    .as_str()
                    .unwrap_or("preferred_username")
                    .into(),
                display_field: cfg["oauth2_DisplayNameField"]
                    .as_str()
                    .unwrap_or("preferred_username")
                    .into(),
                use_https: cfg["security_useHttps"].as_bool().unwrap_or(false),
                admin_id: cfg["admin"]["oauth2"].as_str().map(String::from),
            },
            meta: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
        })
    }

    fn issuer(&self) -> String {
        self.cfg
            .discovery_url
            .trim_end_matches("/.well-known/openid-configuration")
            .trim_end_matches('/')
            .to_string()
    }

    fn metadata(&self) -> Result<CoreProviderMetadata, String> {
        if let Some(m) = self.meta.lock().unwrap().clone() {
            return Ok(m);
        }
        let issuer = IssuerUrl::new(self.issuer()).map_err(|e| e.to_string())?;
        let m = CoreProviderMetadata::discover(&issuer, &UreqHttp)
            .map_err(|e| format!("discovery: {e}"))?;
        *self.meta.lock().unwrap() = Some(m.clone());
        Ok(m)
    }

    fn client(
        &self,
        redirect: String,
    ) -> Result<
        CoreClient<
            openidconnect::EndpointSet,
            openidconnect::EndpointNotSet,
            openidconnect::EndpointNotSet,
            openidconnect::EndpointNotSet,
            openidconnect::EndpointMaybeSet,
            openidconnect::EndpointMaybeSet,
        >,
        String,
    > {
        let meta = self.metadata()?;
        Ok(CoreClient::from_provider_metadata(
            meta,
            ClientId::new(self.cfg.client_id.clone()),
            Some(ClientSecret::new(self.cfg.client_secret.clone())),
        )
        .set_redirect_uri(RedirectUrl::new(redirect).map_err(|e| e.to_string())?))
    }

    pub fn is_admin(&self, session: Option<&Value>) -> bool {
        match (&self.cfg.admin_id, session) {
            (Some(admin), Some(u)) => u["id"].as_str() == Some(admin.as_str()),
            _ => false,
        }
    }
}

fn redirect_uri(auth: &Auth) -> String {
    format!("{}://{}/auth/oauth2/callback/oauth2", auth.proto, auth.host)
}

// ---------------------------------------------------------------- routes

/// POST /auth/sign-in/oauth2 — the shape the security client's
/// better-auth call sends: JSON {providerId, callbackURL?}; answered
/// with {url, redirect:true} exactly like the oracle.
pub fn sign_in(oidc: &Oidc, auth: &Auth, body: &[u8]) -> Response {
    let req: Value = serde_json::from_slice(body).unwrap_or(json!({}));
    let callback = req["callbackURL"].as_str().unwrap_or("/").to_string();
    let client = match oidc.client(redirect_uri(auth)) {
        Ok(c) => c,
        Err(e) => return oidc_error(&e),
    };
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("profile".into()))
        .add_scope(Scope::new("email".into()))
        .set_pkce_challenge(challenge)
        .url();
    oidc.pending.lock().unwrap().insert(
        state.secret().clone(),
        Pending {
            nonce,
            verifier,
            callback,
        },
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .body(Body::from(
            json!({"url": url.to_string(), "redirect": true}).to_string(),
        ))
        .unwrap()
}

/// GET /auth/oauth2/callback/oauth2?code&state — exchange, verify,
/// map the identity from userinfo, set the session, bounce to the
/// callbackURL the sign-in asked for.
pub fn callback(oidc: &Oidc, site: &Site, auth: &Auth, query: &str) -> Response {
    let q: HashMap<String, String> = url_query(query);
    let (Some(code), Some(state)) = (q.get("code"), q.get("state")) else {
        return plain(400, "Missing code or state");
    };
    let Some(pending) = oidc.pending.lock().unwrap().remove(state) else {
        // the oracle's better-auth logs "State mismatch" here
        return plain(400, "State mismatch");
    };
    let client = match oidc.client(redirect_uri(auth)) {
        Ok(c) => c,
        Err(e) => return oidc_error(&e),
    };
    let exchange = match client.exchange_code(AuthorizationCode::new(code.clone())) {
        Ok(r) => r,
        Err(e) => return oidc_error(&format!("token endpoint: {e}")),
    };
    let tokens = match exchange.set_pkce_verifier(pending.verifier).request(&UreqHttp) {
        Ok(t) => t,
        Err(e) => return oidc_error(&format!("code exchange: {e}")),
    };
    // verify the id_token (signature vs JWKS, issuer, audience, nonce) —
    // the oracle skips this; we don't
    let id_token = match openidconnect::TokenResponse::id_token(&tokens) {
        Some(t) => t,
        None => return oidc_error("provider returned no id_token"),
    };
    if let Err(e) = id_token.claims(&client.id_token_verifier(), &pending.nonce) {
        return oidc_error(&format!("id_token verification: {e}"));
    }
    // profile fields from userinfo's raw JSON — exact IdField semantics
    let meta = match oidc.metadata() {
        Ok(m) => m,
        Err(e) => return oidc_error(&e),
    };
    let Some(userinfo_url) = meta.userinfo_endpoint() else {
        return oidc_error("provider has no userinfo endpoint");
    };
    let profile = match fetch_userinfo(userinfo_url.as_str(), tokens.access_token().secret())
    {
        Ok(p) => p,
        Err(e) => return oidc_error(&e),
    };
    let field = |name: &str, fallback: &str| {
        let v = &profile[name];
        if v.is_null() {
            profile[fallback].clone()
        } else {
            v.clone()
        }
    };
    let identity = json!({
        "id": field(&oidc.cfg.id_field, "sub"),
        "username": field(&oidc.cfg.username_field, "preferred_username"),
        "name": field(&oidc.cfg.display_field, "preferred_username"),
    });
    let cookie = site.security.oauth2_grant(&identity, oidc.cfg.use_https);
    Response::builder()
        .status(StatusCode::FOUND)
        .header(header::LOCATION, pending.callback)
        .header(header::SET_COOKIE, cookie)
        .body(Body::empty())
        .unwrap()
}

/// POST /auth/sign-out — better-auth's CSRF stance, observed on the
/// oracle: no Origin header -> 403 MISSING_OR_NULL_ORIGIN.
pub fn sign_out(site: &Site, auth: &Auth) -> Response {
    match &auth.origin {
        None => Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Body::from(
                json!({"message": "Missing or null Origin",
                       "code": "MISSING_OR_NULL_ORIGIN"})
                .to_string(),
            ))
            .unwrap(),
        Some(origin) if !origin_trusted(origin, &auth.host) => Response::builder()
            .status(StatusCode::FORBIDDEN)
            .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
            .body(Body::from(
                json!({"message": "Invalid origin", "code": "INVALID_ORIGIN"})
                    .to_string(),
            ))
            .unwrap(),
        Some(_) => {
            let cookie = site.security.oauth2_signout(auth.cookie.as_deref());
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                .header(header::SET_COOKIE, cookie)
                .body(Body::from(json!({"success": true}).to_string()))
                .unwrap()
        }
    }
}

/// GET /auth/client-settings.json (social.js): {useHttps, isOwner}.
pub fn client_settings(oidc: &Oidc, site: &Site, auth: &Auth) -> Response {
    let is_owner = auth.authorized && site.security.owner_name().is_some();
    crate::cors_json(json!({"useHttps": oidc.cfg.use_https, "isOwner": is_owner}))
}

/// GET /auth/claim-wiki (social.js): signed-in user claims an ownerless
/// site; owner present -> express sendStatus(403) "Forbidden".
pub fn claim_wiki(site: &Site, auth: &Auth) -> Response {
    let Some(identity) = site.security.oauth2_session(auth.cookie.as_deref()) else {
        return plain(403, "Forbidden");
    };
    match site
        .security
        .oauth2_claim(&site.data.join("status/owner.json"), &identity)
    {
        Ok(name) => crate::cors_json(json!({"ownerName": name})),
        Err(403) => plain(403, "Forbidden"),
        Err(_) => plain(500, "Internal Server Error"),
    }
}

/// GET /logout (social.js): req.session.reset() -> "OK".
pub fn logout(site: &Site, auth: &Auth) -> Response {
    let cookie = site.security.oauth2_signout(auth.cookie.as_deref());
    Response::builder()
        .status(StatusCode::OK)
        .header(header::SET_COOKIE, cookie)
        .body(Body::from("OK"))
        .unwrap()
}

/// GET /auth/loginDialog — the oracle's rendered dialog, OAuth2 scheme
/// only (grabbed from a live render; the stray wikiName cookie plumbing
/// is dropped, the signIn("oauth2") contract kept — auth-dialog.js then
/// POSTs /auth/sign-in/oauth2 at us).
pub fn login_dialog(auth: &Auth) -> Response {
    let host = &auth.host;
    let body = format!(
        r#"<!doctype html>
<html>
  <head>
    <link href="https://fonts.googleapis.com/css?family=Open+Sans:400,300,700" rel="stylesheet" type="text/css" />
    <title>Federated Wiki: Site Owner Sign-on</title>
    <link id="favicon" href="/favicon.png" rel="icon" type="image/png" />
    <link rel="stylesheet" href="/security/dialog.css" />
    <script type="text/javascript" src="/security/auth-dialog.js"></script>
  </head>
  <body>
    <div id="content">
      <div class="wikiinfo">
        <div class="table">
          <div class="vertical">
            <img id="wiki_logo" src="/favicon.png" width="32px" height="32px" />
            <h2 id="wiki_name">{host}</h2>
            <h3 id="wiki_hostname">a federated wiki site</h3>
          </div>
        </div>
      </div>
      <div id="signIn">
        <div class="table">
          <div class="vertical">
            <div class="contents">
              <div class="scheme_section vcenter" style="width: 249px">
                <h2>Sign in to {host} with...</h2>
                <p><a href='#' onclick='window.resizeBy(0,+300); signIn("oauth2")'  class='scheme-button'><span>OAuth2</span></a></p>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
    <footer>
      <span id="footerText">
        <strong>Federated Wiki.</strong>
        Sharing ideas through federation.
        <a href="http://fed.wiki/view/welcome-visitors/view/federated-wiki" target="_blank">Learn more→</a>
      </span>
    </footer>
  </body>
</html>"#
    );
    html(body)
}

/// GET /auth/loginDone — the popup's landing page; the postMessage
/// contract with the opener (security.js) is kept verbatim.
pub fn login_done(site: &Site, auth: &Auth) -> Response {
    let host = &auth.host;
    let (title, auth_message) = if site.security.owner_name().is_some() {
        ("Wiki Site Owner Sign-on",
         "You are now logged in. If this window hasn't closed, you can close it.")
    } else {
        ("Sign-on to claim Wiki site",
         "You are now logged in. If this window hasn't closed, you can close it.")
    };
    let body = format!(
        r#"<!doctype html>
<html>
  <head>
    <link href="https://fonts.googleapis.com/css?family=Open+Sans:400,300,700" rel="stylesheet" type="text/css" />
    <title>{title}</title>
    <link id="favicon" href="/favicon.png" rel="icon" type="image/png" />
    <link rel="stylesheet" href="/security/dialog.css" />
  </head>
  <body>
    <div id="content">
      <div class="wikiinfo">
        <div class="table">
          <div class="vertical">
            <img id="wiki_logo" src="//{host}/favicon.png" width="32px" height="32px" />
            <h2 id="wiki_name">{host}</h2>
            <h3 id="wiki_hostname">a federated wiki site</h3>
          </div>
        </div>
      </div>
      <div id="signIn">
        <div class="table">
          <div class="vertical">
            <div class="contents">
              <div class="scheme_section vcenter" style="width: 249px">
                <div id="message">
                  <p>{auth_message}</p>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
    <footer>
      <span id="footerText">
        <strong>Federated Wiki.</strong>
        Sharing ideas through federation.
        <a href="http://fed.wiki/view/welcome-visitors/view/federated-wiki" target="_blank">Learn more→</a>
      </span>
    </footer>
  </body>
  <script type="module">
    const wikiName = window.location.host
    const targetOrigin = `${{window.location.protocol}}//${{wikiName}}`
    if (window.opener) {{
      window.opener.postMessage(
        {{
          type: 'AUTHENTICATION_RESPONSE',
          payload: {{
            done: true,
            timestamp: new Date().toString(),
          }},
        }},
        targetOrigin,
      )
    }}
  </script>
</html>"#
    );
    html(body)
}

// ------------------------------------------------------------- plumbing

fn origin_trusted(origin: &str, host: &str) -> bool {
    origin
        .split("://")
        .nth(1)
        .map(|h| h == host)
        .unwrap_or(false)
}

fn url_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((k.to_string(), url_decode(v)))
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                if let Ok(b) = u8::from_str_radix(
                    std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""),
                    16,
                ) {
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

fn fetch_userinfo(url: &str, bearer: &str) -> Result<Value, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let resp = agent
        .get(url)
        .set("Authorization", &format!("Bearer {bearer}"))
        .call()
        .map_err(|e| format!("userinfo: {e}"))?;
    let mut body = Vec::new();
    use std::io::Read;
    resp.into_reader()
        .read_to_end(&mut body)
        .map_err(|e| format!("userinfo read: {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("userinfo parse: {e}"))
}

fn plain(status: u16, text: &str) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap())
        .body(Body::from(text.to_string()))
        .unwrap()
}

fn html(body: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(Body::from(body))
        .unwrap()
}

fn oidc_error(e: &str) -> Response {
    eprintln!("oidc: {e}");
    plain(502, "OIDC flow failed; see server log")
}

/// openidconnect's SyncHttpClient bridged onto ureq (which carries the
/// TLS stack the rest of sweet already ships).
pub struct UreqHttp;

impl openidconnect::SyncHttpClient for UreqHttp {
    type Error = UreqBridgeError;

    fn call(
        &self,
        request: openidconnect::HttpRequest,
    ) -> Result<openidconnect::HttpResponse, Self::Error> {
        let agent = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(10))
            .build();
        let mut req = agent.request(request.method().as_str(), &request.uri().to_string());
        for (name, value) in request.headers() {
            if let Ok(v) = value.to_str() {
                req = req.set(name.as_str(), v);
            }
        }
        let result = if request.body().is_empty() {
            req.call()
        } else {
            req.send_bytes(request.body())
        };
        // 4xx/5xx bodies must round-trip: the oauth2 crate parses error
        // JSON out of them (ureq turns them into Error::Status)
        let resp = match result {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => return Err(UreqBridgeError(e.to_string())),
        };
        let mut builder = axum::http::Response::builder().status(resp.status());
        for name in resp.headers_names() {
            if let Some(value) = resp.header(&name) {
                builder = builder.header(name.as_str(), value);
            }
        }
        let mut body = Vec::new();
        use std::io::Read;
        resp.into_reader()
            .read_to_end(&mut body)
            .map_err(|e| UreqBridgeError(e.to_string()))?;
        builder
            .body(body)
            .map_err(|e| UreqBridgeError(e.to_string()))
    }
}

#[derive(Debug)]
pub struct UreqBridgeError(String);

impl std::fmt::Display for UreqBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UreqBridgeError {}
