//! Auth + ACL. See DESIGN.md §12.
//!
//! Two modes:
//!   - **Dev** (default when no OIDC issuer is configured): every request
//!     is authenticated as the synthetic `dev:admin` user with the `admin`
//!     group. Useful for local development and the existing smoke tests.
//!   - **Oidc**: real OIDC authorization-code-with-PKCE flow. Sessions
//!     stored in SQLite, opaque-token cookies (sha256-hashed at rest).
//!
//! ACL semantics:
//!   - `admin` group always passes.
//!   - Empty ACL ⇒ admin-only.
//!   - Non-empty ACL ⇒ pass if user.groups ∩ acl is non-empty.
//!
//! Wiki page ACL: union of ACLs of every Source listed in `frontmatter.source_ids`.
//! Computed on demand at read time (see `effective_wiki_acl`).

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{FromRef, FromRequestParts},
    http::{request::Parts, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use qpedia_core::{acl::Acl, source::Source, tenant::Tenant};
use qpedia_store::{sqlite::SourceStore, SqliteStore};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::Arc;
use tracing::{info, warn};

pub const SESSION_COOKIE: &str = "qpedia_session";
pub const SESSION_TTL_SECS: i64 = 24 * 60 * 60;

/// Per-process auth state. Shared across handlers via `AppState`.
#[derive(Clone)]
pub struct AuthState {
    pub mode: AuthMode,
}

#[derive(Clone)]
pub enum AuthMode {
    Dev,
    Oidc(Arc<OidcConfig>),
}

pub struct OidcConfig {
    pub client: CoreClient<
        openidconnect::EndpointSet,        // HasAuthUrl
        openidconnect::EndpointNotSet,     // HasDeviceAuthUrl
        openidconnect::EndpointNotSet,     // HasIntrospectionUrl
        openidconnect::EndpointNotSet,     // HasRevocationUrl
        openidconnect::EndpointMaybeSet,   // HasTokenUrl
        openidconnect::EndpointMaybeSet,   // HasUserInfoUrl
    >,
    pub http: reqwest::Client,
    pub groups_claim: String,
    pub end_session_endpoint: Option<String>,
}

impl AuthState {
    /// Build from env. Falls back to Dev mode when OIDC vars aren't all set.
    pub async fn from_env() -> Result<Self> {
        let mode = std::env::var("QPEDIA_AUTH_MODE").ok();
        let issuer = std::env::var("QPEDIA_OIDC_ISSUER").ok();

        match (mode.as_deref(), issuer) {
            (Some("dev"), _) | (None, None) => {
                info!("auth: dev mode (anonymous admin)");
                Ok(Self { mode: AuthMode::Dev })
            }
            (_, Some(issuer)) => {
                let client_id = std::env::var("QPEDIA_OIDC_CLIENT_ID")
                    .context("QPEDIA_OIDC_CLIENT_ID required for OIDC auth")?;
                let client_secret = std::env::var("QPEDIA_OIDC_CLIENT_SECRET")
                    .context("QPEDIA_OIDC_CLIENT_SECRET required for OIDC auth")?;
                let redirect_url = std::env::var("QPEDIA_OIDC_REDIRECT_URL")
                    .unwrap_or_else(|_| "http://127.0.0.1:8080/auth/callback".into());
                let groups_claim =
                    std::env::var("QPEDIA_OIDC_GROUPS_CLAIM").unwrap_or_else(|_| "groups".into());

                let http = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .context("build oidc http client")?;

                let provider = CoreProviderMetadata::discover_async(
                    IssuerUrl::new(issuer.clone())?,
                    &http,
                )
                .await
                .context("oidc discovery")?;

                // RP-Initiated Logout endpoint is an OIDC extension and not
                // in `CoreProviderMetadata`'s additional fields by default.
                // Take it from env when the IdP needs it.
                let end_session_endpoint = std::env::var("QPEDIA_OIDC_END_SESSION_URL").ok();

                let client = CoreClient::from_provider_metadata(
                    provider,
                    ClientId::new(client_id),
                    Some(ClientSecret::new(client_secret)),
                )
                .set_redirect_uri(RedirectUrl::new(redirect_url)?);

                info!(issuer = %issuer, "auth: OIDC configured");
                Ok(Self {
                    mode: AuthMode::Oidc(Arc::new(OidcConfig {
                        client,
                        http,
                        groups_claim,
                        end_session_endpoint,
                    })),
                })
            }
            (Some(other), _) => Err(anyhow!("unknown QPEDIA_AUTH_MODE: {other}")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub groups: Vec<String>,
    pub tenant: Tenant,
}

impl User {
    pub fn dev_admin() -> Self {
        let tenant = std::env::var("QPEDIA_DEV_TENANT")
            .ok()
            .map(Tenant::new)
            .unwrap_or_else(Tenant::default_tenant);
        Self {
            id: "dev:admin".into(),
            email: Some("admin@dev.local".into()),
            name: Some("dev admin".into()),
            groups: vec!["admin".into()],
            tenant,
        }
    }
    pub fn is_admin(&self) -> bool {
        self.groups.iter().any(|g| g == "admin")
    }
    pub fn can_read(&self, acl: &Acl) -> bool {
        if self.is_admin() {
            return true;
        }
        if acl.0.is_empty() {
            return false; // empty ACL = admin-only
        }
        self.groups.iter().any(|g| acl.0.contains(g))
    }
}

/// Compute the effective ACL for a wiki page — union of ACLs of every
/// Source (within the user's tenant) that the page cites in frontmatter
/// `source_ids`. Cross-tenant source references contribute nothing.
pub async fn effective_wiki_acl(db: &SqliteStore, tenant: &Tenant, source_ids: &[String]) -> Acl {
    let mut union: BTreeSet<String> = BTreeSet::new();
    for sid in source_ids {
        if let Ok(Some(src)) = db.get_source_in(tenant, &sid.clone().into()).await {
            for g in src.acl.0.iter() {
                union.insert(g.clone());
            }
        }
    }
    Acl(union)
}

/// Filter a source list down to what the user is allowed to read.
pub fn filter_sources(user: &User, sources: Vec<Source>) -> Vec<Source> {
    sources
        .into_iter()
        .filter(|s| user.can_read(&s.acl))
        .collect()
}

// ---------- cookie helpers ----------

pub fn read_session_token(headers: &HeaderMap) -> Option<String> {
    let header = headers.get(axum::http::header::COOKIE)?;
    let raw = header.to_str().ok()?;
    for piece in raw.split(';') {
        let p = piece.trim();
        if let Some(v) = p.strip_prefix(&format!("{SESSION_COOKIE}=")) {
            return Some(v.to_string());
        }
    }
    None
}

pub fn set_session_cookie(token: &str, max_age: i64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}"
    ))
    .expect("valid cookie")
}

pub fn clear_session_cookie() -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"
    ))
    .expect("valid cookie")
}

pub fn hash_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex::encode(h.finalize())
}

pub fn random_token(bytes: usize) -> String {
    use rand::RngCore;
    let mut rng = rand::thread_rng();
    let mut buf = vec![0u8; bytes];
    rng.fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(&buf)
}

// ---------- User extractor ----------

#[axum::async_trait]
impl<S> FromRequestParts<S> for User
where
    S: Send + Sync,
    AuthExtractorState: FromRef<S>,
{
    type Rejection = AuthRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let ext = AuthExtractorState::from_ref(state);
        match &ext.auth.mode {
            AuthMode::Dev => Ok(User::dev_admin()),
            AuthMode::Oidc(_) => {
                let token =
                    read_session_token(&parts.headers).ok_or(AuthRejection::Unauthorized)?;
                let session = ext
                    .db
                    .lookup_session(&hash_token(&token))
                    .await
                    .map_err(|_| AuthRejection::Unauthorized)?
                    .ok_or(AuthRejection::Unauthorized)?;
                Ok(User {
                    id: session.user_id,
                    email: session.email,
                    name: session.name,
                    groups: session.groups,
                    tenant: session.tenant,
                })
            }
        }
    }
}

#[derive(Clone)]
pub struct AuthExtractorState {
    pub auth: AuthState,
    pub db: SqliteStore,
}

#[derive(Debug)]
pub enum AuthRejection {
    Unauthorized,
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

// ---------- OIDC route handlers ----------

#[derive(Deserialize)]
pub struct LoginQuery {
    pub redirect_after: Option<String>,
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    pub code: String,
    pub state: String,
}

pub async fn oidc_login(
    auth: &AuthState,
    db: &SqliteStore,
    query: LoginQuery,
) -> Result<Redirect, Response> {
    let cfg = match &auth.mode {
        AuthMode::Oidc(c) => c.clone(),
        AuthMode::Dev => {
            return Err((StatusCode::BAD_REQUEST, "auth is in dev mode").into_response())
        }
    };

    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let (auth_url, csrf, nonce) = cfg
        .client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("openid".into()))
        .add_scope(Scope::new("email".into()))
        .add_scope(Scope::new("profile".into()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    db.save_pending(
        csrf.secret(),
        pkce_verifier.secret(),
        nonce.secret(),
        query.redirect_after.as_deref(),
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("save pending: {e}")).into_response())?;

    Ok(Redirect::to(auth_url.as_str()))
}

pub async fn oidc_callback(
    auth: &AuthState,
    db: &SqliteStore,
    query: CallbackQuery,
) -> Result<(HeaderMap, Redirect), Response> {
    let cfg = match &auth.mode {
        AuthMode::Oidc(c) => c.clone(),
        AuthMode::Dev => {
            return Err((StatusCode::BAD_REQUEST, "auth is in dev mode").into_response())
        }
    };

    let pending = db
        .take_pending(&query.state)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("take pending: {e}")).into_response())?
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "unknown or expired state").into_response())?;

    let token_response = cfg
        .client
        .exchange_code(AuthorizationCode::new(query.code))
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("exchange config: {e}")).into_response())?
        .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier))
        .request_async(&cfg.http)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("exchange code: {e}")).into_response())?;

    let id_token = token_response
        .id_token()
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "no id_token in response").into_response())?;

    let id_verifier = cfg.client.id_token_verifier();
    let claims = id_token
        .claims(&id_verifier, &Nonce::new(pending.nonce))
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("verify id_token: {e}")).into_response())?;

    let user_id = claims.subject().as_str().to_string();
    let email = claims.email().map(|e| e.as_str().to_string());
    let name = claims
        .name()
        .and_then(|n| n.iter().next().map(|(_, v)| v.as_str().to_string()));

    // Groups extraction: read configured claim from `additional_claims` if
    // possible, otherwise look in standard claims by JSON-roundtrip.
    let groups = extract_groups(claims, &cfg.groups_claim);

    // Tenant: read from a configurable claim (default 'tenant_id'); fall
    // back to 'default' if missing.
    let tenant_claim = std::env::var("QPEDIA_OIDC_TENANT_CLAIM")
        .unwrap_or_else(|_| "tenant_id".into());
    let tenant = extract_tenant(claims, &tenant_claim);

    // Mint a session.
    let token = random_token(32);
    let token_hash = hash_token(&token);
    db.create_session(
        &token_hash,
        &tenant,
        &user_id,
        email.as_deref(),
        name.as_deref(),
        &groups,
        SESSION_TTL_SECS,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("session: {e}")).into_response())?;

    let mut headers = HeaderMap::new();
    headers.append(axum::http::header::SET_COOKIE, set_session_cookie(&token, SESSION_TTL_SECS));

    let dest = pending.redirect_after.unwrap_or_else(|| "/".into());
    info!(user = %user_id, tenant = %tenant, groups = ?groups, "oidc login");
    Ok((headers, Redirect::to(&dest)))
}

fn extract_tenant(
    claims: &openidconnect::IdTokenClaims<openidconnect::EmptyAdditionalClaims, openidconnect::core::CoreGenderClaim>,
    claim: &str,
) -> Tenant {
    let v = match serde_json::to_value(claims) {
        Ok(v) => v,
        Err(_) => return Tenant::default_tenant(),
    };
    if let Some(s) = v.get(claim).and_then(|x| x.as_str()) {
        return Tenant::new(s);
    }
    Tenant::default_tenant()
}

pub async fn oidc_logout(
    auth: &AuthState,
    db: &SqliteStore,
    headers: &HeaderMap,
) -> (HeaderMap, Redirect) {
    if let Some(token) = read_session_token(headers) {
        let _ = db.delete_session(&hash_token(&token)).await;
    }

    let mut out = HeaderMap::new();
    out.append(axum::http::header::SET_COOKIE, clear_session_cookie());

    let redirect = match &auth.mode {
        AuthMode::Oidc(cfg) => cfg
            .end_session_endpoint
            .clone()
            .unwrap_or_else(|| "/".into()),
        AuthMode::Dev => "/".into(),
    };
    (out, Redirect::to(&redirect))
}

fn extract_groups(
    claims: &openidconnect::IdTokenClaims<openidconnect::EmptyAdditionalClaims, openidconnect::core::CoreGenderClaim>,
    claim: &str,
) -> Vec<String> {
    // Roundtrip claims to JSON and pluck the configured key. Robust against
    // provider differences in custom claim placement.
    let v = match serde_json::to_value(claims) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    if let Some(arr) = v.get(claim).and_then(|x| x.as_array()) {
        return arr
            .iter()
            .filter_map(|g| g.as_str().map(|s| s.to_string()))
            .collect();
    }
    if let Some(s) = v.get(claim).and_then(|x| x.as_str()) {
        // Some providers send space- or comma-separated strings.
        return s
            .split(|c: char| c == ' ' || c == ',')
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
            .collect();
    }
    // Fallback: look in `roles` if the configured claim missed.
    if claim != "roles" {
        if let Some(arr) = v.get("roles").and_then(|x| x.as_array()) {
            warn!("groups claim {claim:?} missing; falling back to 'roles'");
            return arr
                .iter()
                .filter_map(|g| g.as_str().map(|s| s.to_string()))
                .collect();
        }
    }
    Vec::new()
}
