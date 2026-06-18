//! SpacetimeAuth (OIDC) login via the browser, with Discord as the provider.
//!
//! This is a hand-rolled **Authorization Code + PKCE (S256)** flow — no React,
//! no `oidc-client-ts`. We only need to *obtain* an `id_token` to hand to the
//! SpacetimeDB SDK's `.with_token()`; SpacetimeDB validates the signature
//! server-side against the issuer's JWKS, so the client never verifies it (and
//! we don't pull in any JWT/crypto-verify dependencies).
//!
//! Endpoints are hardcoded: the OIDC discovery document at
//! `auth.spacetimedb.com/.well-known/openid-configuration` returns no CORS
//! headers on a browser GET, so we can't fetch it at runtime — but the values
//! are stable (confirmed against the live `/oidc/.well-known/openid-configuration`).
//!
//! Flow:
//!   1. `login()` — make a PKCE verifier + CSRF state, stash them in
//!      `sessionStorage` (survives the redirect, per-tab), then navigate to the
//!      authorize endpoint.
//!   2. Discord (brokered by SpacetimeAuth) redirects back to our origin with
//!      `?code=…&state=…`.
//!   3. `pending_code()` — at startup, detect that callback and validate `state`.
//!   4. `exchange()` — POST the code + verifier to the token endpoint, read the
//!      `id_token` out of the JSON response.
//! The token is then handed to the connection (see `conn.rs`).

#![cfg(target_arch = "wasm32")]

use base64::Engine;
use wasm_bindgen::JsValue;

/// Public SPA client registered in the SpacetimeAuth dashboard (no secret —
/// PKCE stands in for one).
const CLIENT_ID: &str = "client_033aGOIb0qVHD1QceoUysS";
const AUTH_ENDPOINT: &str = "https://auth.spacetimedb.com/oidc/auth";
const TOKEN_ENDPOINT: &str = "https://auth.spacetimedb.com/oidc/token";
/// `openid` is required for an id_token; `profile` gives the Discord
/// username/avatar claims.
const SCOPES: &str = "openid profile";

/// localStorage key for the persisted session token (so a reload re-connects
/// without bouncing through Discord again, until the token expires).
const TOKEN_KEY: &str = "stdb-lightshow-token";
/// localStorage key for the cached display name (from the id_token claims), so
/// the UI can show it across reloads without re-reading the token.
const USERNAME_KEY: &str = "stdb-lightshow-username";
/// sessionStorage keys for the in-flight PKCE handshake (per-tab, single-use).
const SS_VERIFIER: &str = "oidc_pkce_verifier";
const SS_STATE: &str = "oidc_state";

// ---- storage helpers -------------------------------------------------------

fn local() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}
fn session() -> Option<web_sys::Storage> {
    web_sys::window()?.session_storage().ok().flatten()
}

/// Persist the SpacetimeDB session token returned by `on_connect`.
pub fn store_token(token: &str) {
    if let Some(s) = local() {
        let _ = s.set_item(TOKEN_KEY, token);
    }
}

/// The persisted session token, if any (used for silent re-login on reload).
pub fn stored_token() -> Option<String> {
    local()?
        .get_item(TOKEN_KEY)
        .ok()
        .flatten()
        .filter(|t| !t.is_empty())
}

/// Forget the session token (logout, or after an invalid/expired token).
pub fn clear_token() {
    if let Some(s) = local() {
        let _ = s.remove_item(TOKEN_KEY);
        let _ = s.remove_item(USERNAME_KEY);
    }
}

/// The cached Discord display name (from the last login's id_token), if any.
pub fn stored_username() -> Option<String> {
    local()?
        .get_item(USERNAME_KEY)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

/// Pull a human display name out of an id_token's claims (Discord username).
fn extract_username(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    ["preferred_username", "nickname", "name", "given_name"]
        .iter()
        .find_map(|k| {
            claims
                .get(k)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        })
}

/// Drop the single-use PKCE handshake values.
pub fn clear_session() {
    if let Some(s) = session() {
        let _ = s.remove_item(SS_VERIFIER);
        let _ = s.remove_item(SS_STATE);
    }
}

// ---- the flow --------------------------------------------------------------

/// The exact redirect URI registered on the SpacetimeAuth client: the app's
/// origin with a trailing slash (matches both prod and `localhost:8123`).
fn redirect_uri() -> String {
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .map(|o| format!("{o}/"))
        .unwrap_or_default()
}

/// Begin login: build the PKCE challenge + CSRF state, then redirect the whole
/// page to SpacetimeAuth. Navigates away on success.
pub fn login() {
    let verifier = random_b64url(32);
    let challenge = sha256_b64url(&verifier);
    let state = random_b64url(16);
    if let Some(ss) = session() {
        let _ = ss.set_item(SS_VERIFIER, &verifier);
        let _ = ss.set_item(SS_STATE, &state);
    } else {
        log::error!("no sessionStorage — cannot start login");
        return;
    }
    let url = format!(
        "{AUTH_ENDPOINT}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}\
         &code_challenge={}&code_challenge_method=S256",
        enc(CLIENT_ID),
        enc(&redirect_uri()),
        enc(SCOPES),
        enc(&state),
        enc(&challenge),
    );
    if let Some(w) = web_sys::window() {
        let _ = w.location().set_href(&url);
    }
}

/// Clear the token, end the local session, and reload (→ login screen).
pub fn logout() {
    clear_token();
    clear_session();
    if let Some(w) = web_sys::window() {
        let _ = w.location().reload();
    }
}

/// If we're returning from an OIDC redirect, return the authorization `code`
/// (after validating the CSRF `state` against what we stored). `None` otherwise.
pub fn pending_code() -> Option<String> {
    let w = web_sys::window()?;
    let search = w.location().search().ok()?;
    if search.is_empty() {
        return None;
    }
    let q = search.strip_prefix('?').unwrap_or(&search);
    let mut code = None;
    let mut state = None;
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        match k {
            "code" => code = Some(dec(v)),
            "state" => state = Some(dec(v)),
            _ => {}
        }
    }
    let code = code?;
    let state = state?;
    let stored = session().and_then(|s| s.get_item(SS_STATE).ok().flatten());
    if stored.as_deref() != Some(state.as_str()) {
        log::error!("OIDC state mismatch — ignoring callback");
        return None;
    }
    Some(code)
}

/// Exchange an authorization `code` for tokens at the token endpoint and return
/// the `id_token`. Public client: PKCE verifier in place of a client secret.
pub async fn exchange(code: String) -> Result<String, String> {
    let verifier = session()
        .and_then(|s| s.get_item(SS_VERIFIER).ok().flatten())
        .ok_or("missing PKCE verifier (session expired?)")?;
    let body = format!(
        "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
        enc(&code),
        enc(&redirect_uri()),
        enc(CLIENT_ID),
        enc(&verifier),
    );
    let resp = gloo_net::http::Request::post(TOKEN_ENDPOINT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .map_err(|e| format!("build token request: {e}"))?
        .send()
        .await
        .map_err(|e| format!("token request failed: {e}"))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("read token response: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(format!("token endpoint {status}: {text}"));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parse token json: {e}"))?;
    let id_token = v
        .get("id_token")
        .and_then(|x| x.as_str())
        .ok_or("token response missing id_token")?;
    // Cache the display name for the UI (survives reloads via localStorage).
    if let (Some(name), Some(s)) = (extract_username(id_token), local()) {
        let _ = s.set_item(USERNAME_KEY, &name);
    }
    Ok(id_token.to_string())
}

/// Strip the `?code=…&state=…` from the address bar so a reload doesn't re-run
/// the (single-use) exchange.
pub fn clear_callback_url() {
    if let Some(w) = web_sys::window() {
        if let Ok(history) = w.history() {
            let path = w.location().pathname().unwrap_or_else(|_| "/".into());
            let _ = history.replace_state_with_url(&JsValue::NULL, "", Some(&path));
        }
    }
}

// ---- crypto / encoding helpers --------------------------------------------

/// `n` random bytes, base64url (no padding) — used for the PKCE verifier and
/// the CSRF state. RNG is the browser backend (see `.cargo/config.toml`).
fn random_b64url(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("browser RNG");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&buf)
}

/// SHA-256(input), base64url (no padding) — the PKCE S256 `code_challenge`.
fn sha256_b64url(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h.finalize())
}

/// Percent-encode a query/form component (RFC 3986 unreserved set kept as-is).
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
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

/// Decode a percent-encoded query component (`+` → space).
fn dec(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(h) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(h);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
