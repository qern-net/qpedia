//! Google OAuth 2.0 helper — the credential machinery shared by the
//! SSO-aligned Google connectors.
//!
//! Firebase Auth handles *identity* (who the user is); it does not hand
//! the backend an OAuth refresh token. Durable resource access (the
//! auto-sync scheduler reading a Drive in the background) needs a
//! refresh token, obtained via a standard authorization-code flow with
//! `access_type=offline&prompt=consent`. This module:
//!
//!   * [`consent_url`]   — build the URL we redirect the user to.
//!   * [`exchange_code`] — code → { refresh_token, access_token, expiry }.
//!   * [`refresh`]       — refresh_token → { access_token, expiry }.
//!
//! Endpoints are Google's; the same shapes generalize to Microsoft and
//! GitHub later (different URLs, same grant types).

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use std::time::Duration as StdDuration;

const AUTH_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";

/// Read-only Drive scope. The minimum that lets us enumerate + download.
pub const DRIVE_READONLY_SCOPE: &str = "https://www.googleapis.com/auth/drive.readonly";

/// Tokens returned by the token endpoint.
#[derive(Debug, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    /// Present only on the authorization-code exchange (with
    /// `access_type=offline`), absent on refresh.
    pub refresh_token: Option<String>,
    pub expires_at: DateTime<Utc>,
}

/// Build the consent URL to redirect a user to. `state` is echoed back
/// to the callback unchanged — use it to carry CSRF + which tenant/user
/// initiated the grant. `redirect_uri` must exactly match one registered
/// on the Google OAuth client.
pub fn consent_url(client_id: &str, redirect_uri: &str, scope: &str, state: &str) -> String {
    let q = [
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", scope),
        ("access_type", "offline"),
        // Force the consent screen so Google re-issues a refresh token
        // even if the user already granted before (otherwise refresh_token
        // is omitted on subsequent grants).
        ("prompt", "consent"),
        ("state", state),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", urlencode(v)))
    .collect::<Vec<_>>()
    .join("&");
    format!("{AUTH_ENDPOINT}?{q}")
}

/// Exchange an authorization code for tokens. Returns a refresh token
/// (durable) plus a first access token.
pub async fn exchange_code(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    code: &str,
    redirect_uri: &str,
) -> Result<TokenResponse> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("redirect_uri", redirect_uri),
    ];
    post_token(client, &params).await
}

/// Mint a fresh access token from a refresh token. The response has no
/// new refresh token (the original stays valid), so `refresh_token` is
/// `None` here.
pub async fn refresh(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<TokenResponse> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
        ("client_secret", client_secret),
    ];
    post_token(client, &params).await
}

async fn post_token(client: &reqwest::Client, params: &[(&str, &str)]) -> Result<TokenResponse> {
    let resp = client
        .post(TOKEN_ENDPOINT)
        .form(params)
        .timeout(StdDuration::from_secs(30))
        .send()
        .await
        .context("token endpoint request")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("google token endpoint {status}: {text}"));
    }
    let raw: RawToken =
        serde_json::from_str(&text).map_err(|e| anyhow!("decode token response: {e}; body: {text}"))?;
    // Subtract a small skew so we refresh slightly before true expiry.
    let ttl = raw.expires_in.unwrap_or(3600).saturating_sub(30).max(1);
    Ok(TokenResponse {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        expires_at: Utc::now() + Duration::seconds(ttl as i64),
    })
}

#[derive(Debug, Deserialize)]
struct RawToken {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Minimal percent-encoder for query values (RFC 3986 unreserved set
/// preserved, everything else %XX). Avoids pulling a dep just for this.
fn urlencode(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consent_url_has_offline_and_state() {
        let u = consent_url("cid.apps", "https://app/cb", DRIVE_READONLY_SCOPE, "xyz");
        assert!(u.starts_with(AUTH_ENDPOINT));
        assert!(u.contains("access_type=offline"));
        assert!(u.contains("prompt=consent"));
        assert!(u.contains("state=xyz"));
        assert!(u.contains("client_id=cid.apps"));
        // redirect + scope are percent-encoded
        assert!(u.contains("redirect_uri=https%3A%2F%2Fapp%2Fcb"));
        assert!(u.contains("drive.readonly"));
    }
}
