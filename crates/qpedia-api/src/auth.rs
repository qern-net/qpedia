//! Auth + ACL. See SPEC-v2.md §12.
//!
//! Two modes:
//!   - **Dev** (default when no OIDC issuer is configured): every request
//!     is authenticated as the synthetic `dev:admin` user with the `admin`
//!     group. Useful for local development and the existing smoke tests.
//!   - **Oidc**: real OIDC authorization-code-with-PKCE flow. Sessions
//!     stored in Postgres, opaque-token cookies (sha256-hashed at rest).
//!
//! Firebase login is orthogonal to the mode — `firebase_login_route`
//! mints a session for any user the Firebase JS SDK has authenticated.
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
use chrono::Utc;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata};
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
};
use qpedia_core::{acl::Acl, source::Source, tenant::Tenant};
use qpedia_pg_store::PgStore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::Arc;
use tracing::{info, warn};

pub const SESSION_COOKIE: &str = "qpedia_session";
pub const SESSION_TTL_SECS: i64 = 24 * 60 * 60;

/// Per-process auth state. Shared across handlers via `AppState`.
///
/// `mode` covers the original Dev / OIDC paths. `firebase` is the v2
/// addition — orthogonal to `mode`, so you can run dev mode with
/// Firebase off (smoke tests), prod mode with Firebase as the only
/// IdP, or both Firebase and OIDC enabled for migration windows.
#[derive(Clone)]
pub struct AuthState {
    pub mode: AuthMode,
    pub firebase: Option<crate::firebase::FirebaseVerifier>,
    /// Optional machine-to-machine (service-token) auth for external apps.
    /// Composes with any `mode`; `None` unless `QPEDIA_SERVICE_TOKENS` is set.
    pub service: Option<crate::m2m::ServiceTokenAuth>,
    /// Optional OAuth 2 client-credentials (JWT) auth for external apps.
    /// Composes with any `mode`; `None` unless `QPEDIA_M2M_AUDIENCE` is set.
    pub oauth: Option<crate::oauth::OAuthVerifier>,
}

#[derive(Clone)]
pub enum AuthMode {
    /// Every request is the synthetic `dev:admin` user; sessions ignored.
    /// Local dev + smoke tests.
    Dev,
    /// Full OIDC authorization-code flow against an external IdP.
    Oidc(Arc<OidcConfig>),
    /// Session-gated, but without a built-in OIDC issuer. Login happens
    /// out-of-band (Firebase ID token → `/api/v1/auth/firebase/login`
    /// mints a session); the `User` extractor enforces the session
    /// cookie on every request. This is the "Firebase is the only IdP"
    /// path — enabled by setting `QPEDIA_FIREBASE_PROJECT_ID` with no
    /// OIDC issuer.
    Session,
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
    /// Firebase is wired in independently when QPEDIA_FIREBASE_PROJECT_ID
    /// is set — it composes with either mode.
    pub async fn from_env() -> Result<Self> {
        let mode = std::env::var("QPEDIA_AUTH_MODE").ok();
        let issuer = std::env::var("QPEDIA_OIDC_ISSUER").ok();

        let firebase = std::env::var("QPEDIA_FIREBASE_PROJECT_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|pid| {
                info!(project_id = %pid, "auth: Firebase verifier enabled");
                crate::firebase::FirebaseVerifier::new(pid)
            });

        // Optional M2M auth (opt-in). Both compose with every mode below.
        let service = crate::m2m::ServiceTokenAuth::from_env()?;
        let oauth = crate::oauth::OAuthVerifier::from_env()?;

        // Validate an explicit mode up front.
        if let Some(m) = mode.as_deref() {
            if !matches!(m, "dev" | "oidc" | "firebase") {
                return Err(anyhow!(
                    "unknown QPEDIA_AUTH_MODE: {m} (expected dev|oidc|firebase)"
                ));
            }
        }

        // 1. Explicit dev always wins (even with Firebase configured —
        //    useful for exercising the login UI without enforcement).
        if mode.as_deref() == Some("dev") {
            info!("auth: dev mode (anonymous admin)");
            return Ok(Self { mode: AuthMode::Dev, firebase, service, oauth });
        }

        // 2. Session/Firebase mode: requested explicitly, or implied by a
        //    Firebase project being configured with no OIDC issuer.
        if mode.as_deref() == Some("firebase")
            || (issuer.is_none() && firebase.is_some())
        {
            info!("auth: session mode (Firebase login enforced)");
            return Ok(Self { mode: AuthMode::Session, firebase, service, oauth });
        }

        match (mode.as_deref(), issuer) {
            (Some("oidc"), None) => {
                Err(anyhow!("QPEDIA_AUTH_MODE=oidc requires QPEDIA_OIDC_ISSUER"))
            }
            (_, None) => {
                info!("auth: dev mode (anonymous admin)");
                Ok(Self { mode: AuthMode::Dev, firebase, service, oauth })
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
                    firebase,
                    service,
                    oauth,
                })
            }
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
pub async fn effective_wiki_acl(db: &PgStore, tenant: &Tenant, source_ids: &[String]) -> Acl {
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

/// Bootstrap admin access by email. `QPEDIA_ADMIN_EMAILS` is a comma- or
/// space-separated allowlist; a login whose verified email matches (case-
/// insensitive) gets the `admin` group added if it isn't already present.
///
/// This is how the *first* admin exists on a fresh Firebase deployment —
/// before that, a fresh Google sign-in has no `groups` custom claim and
/// would land with zero privileges. For finer control, set a `groups`
/// custom claim via the Firebase Admin SDK and leave this unset.
pub fn augment_admin_by_email(email: Option<&str>, groups: Vec<String>) -> Vec<String> {
    let allow = std::env::var("QPEDIA_ADMIN_EMAILS").unwrap_or_default();
    augment_admin_with_allow(email, groups, &allow)
}

/// Pure core of [`augment_admin_by_email`] — the allowlist is passed in so
/// it's deterministic to test (no process-global env).
fn augment_admin_with_allow(
    email: Option<&str>,
    mut groups: Vec<String>,
    allow: &str,
) -> Vec<String> {
    let Some(email) = email else { return groups };
    let email_lc = email.trim().to_ascii_lowercase();
    let is_admin_email = allow
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .any(|allowed| allowed == email_lc);
    if is_admin_email && !groups.iter().any(|g| g == "admin") {
        groups.push("admin".to_string());
    }
    groups
}

/// Convenience wrapper that turns a TTL (seconds) into a `session_expires_at`
/// timestamp and bridges to `PgStore::create_session`.
pub async fn mint_session(
    db: &PgStore,
    token_hash: &str,
    tenant: &Tenant,
    user_id: &str,
    email: Option<&str>,
    name: Option<&str>,
    provider: Option<&str>,
    groups: &[String],
    ttl_secs: i64,
) -> Result<()> {
    let expires_at = Utc::now() + chrono::Duration::seconds(ttl_secs);
    db.create_session(
        token_hash, tenant, user_id, email, name, provider, groups, None, expires_at,
    )
    .await
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

        // Machine-to-machine: a valid service token (Authorization: Bearer)
        // authenticates in any mode and resolves its own tenant + groups, so
        // RLS scoping is identical to a user session. Falls through when absent.
        if let Some(svc) = &ext.auth.service {
            if let Some(user) = svc.authenticate(&parts.headers) {
                tracing::Span::current().record("tenant", user.tenant.as_str());
                return Ok(user);
            }
        }

        // OAuth 2 client-credentials JWT — validated against the OIDC issuer's
        // JWKS, with client allowlist + scope/tenant claims driving authz/RLS.
        if let Some(oauth) = &ext.auth.oauth {
            if let Some(user) = oauth.authenticate(&parts.headers).await {
                tracing::Span::current().record("tenant", user.tenant.as_str());
                return Ok(user);
            }
        }

        let user = match &ext.auth.mode {
            AuthMode::Dev => User::dev_admin(),
            // Both real modes are session-cookie gated; OIDC and Firebase
            // differ only in how the session is *minted*, not read.
            AuthMode::Oidc(_) | AuthMode::Session => {
                let token =
                    read_session_token(&parts.headers).ok_or(AuthRejection::Unauthorized)?;
                let session = ext
                    .db
                    .lookup_session(&hash_token(&token))
                    .await
                    .map_err(|_| AuthRejection::Unauthorized)?
                    .ok_or(AuthRejection::Unauthorized)?;
                User {
                    id: session.user_id,
                    email: session.email,
                    name: session.name,
                    groups: session.groups,
                    tenant: session.tenant,
                }
            }
        };

        // Now that the request is authenticated and the tenant is resolved,
        // record it on the active `HTTP_Span` so the HTTP-backed views are
        // per-tenant scopable (Req 4.9). The `tenant` field is declared
        // (Empty) on the span by the HTTP tracing layer; recording here is a
        // no-op when no span is active (excluded paths) or telemetry is
        // disabled. Unauthenticated requests never reach this line, so the
        // attribute is simply left unset for them (Req 4.10).
        tracing::Span::current().record("tenant", user.tenant.as_str());

        Ok(user)
    }
}

#[derive(Clone)]
pub struct AuthExtractorState {
    pub auth: AuthState,
    pub db: PgStore,
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
    db: &PgStore,
    query: LoginQuery,
) -> Result<Redirect, Response> {
    let cfg = match &auth.mode {
        AuthMode::Oidc(c) => c.clone(),
        // The /auth/login + /auth/callback OIDC routes only apply in OIDC
        // mode. Firebase (Session mode) logs in via
        // /api/v1/auth/firebase/login instead.
        AuthMode::Dev | AuthMode::Session => {
            return Err((StatusCode::BAD_REQUEST, "OIDC routes not active in this auth mode").into_response())
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
    db: &PgStore,
    query: CallbackQuery,
) -> Result<(HeaderMap, Redirect), Response> {
    let cfg = match &auth.mode {
        AuthMode::Oidc(c) => c.clone(),
        // The /auth/login + /auth/callback OIDC routes only apply in OIDC
        // mode. Firebase (Session mode) logs in via
        // /api/v1/auth/firebase/login instead.
        AuthMode::Dev | AuthMode::Session => {
            return Err((StatusCode::BAD_REQUEST, "OIDC routes not active in this auth mode").into_response())
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

    let groups = extract_groups(claims, &cfg.groups_claim);

    let tenant_claim = std::env::var("QPEDIA_OIDC_TENANT_CLAIM")
        .unwrap_or_else(|_| "tenant_id".into());
    let tenant = extract_tenant(claims, &tenant_claim);

    // Mint a session.
    let token = random_token(32);
    let token_hash = hash_token(&token);
    mint_session(
        db,
        &token_hash,
        &tenant,
        &user_id,
        email.as_deref(),
        name.as_deref(),
        Some("oidc"),
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
    db: &PgStore,
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
        AuthMode::Dev | AuthMode::Session => "/".into(),
    };
    (out, Redirect::to(&redirect))
}

#[cfg(test)]
mod tests {
    use super::augment_admin_with_allow;

    #[test]
    fn admin_email_grants_admin() {
        // case-insensitive, comma + space separated
        let g = augment_admin_with_allow(Some("B@Y.com"), vec![], "a@x.com, b@y.com");
        assert!(g.contains(&"admin".to_string()));
    }

    #[test]
    fn non_admin_email_unchanged() {
        let g = augment_admin_with_allow(Some("c@z.com"), vec!["staff".into()], "a@x.com");
    