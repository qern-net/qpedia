//! Machine-to-machine (service-token) authentication for external applications.
//!
//! Qpedia's human auth is session-cookie based (see [`crate::auth`]). External
//! applications — the first being the RFP platform — need a non-interactive
//! credential. This is the first cut: configured **service tokens**, each bound
//! to a tenant and a set of groups, presented as a bearer token.
//!
//! Unlike a bare API key, a service token carries identity (tenant + groups), so
//! it drives the same RLS tenant scoping and ACL checks as a user session — no
//! cross-tenant leak. Tokens are compared by sha256 hash and never stored or
//! logged in plaintext.
//!
//! Config — `QPEDIA_SERVICE_TOKENS` is a JSON array:
//! ```json
//! [{"name":"rfp-app","token":"<secret>","tenant":"acme","groups":["admin"]}]
//! ```
//!
//! Future: replace static tokens with OAuth2 client-credentials JWTs validated
//! against the OIDC issuer (scopes → groups). The extractor seam in
//! [`crate::auth`] stays the same. See `TASK-external-app-auth.md`.

use crate::auth::{hash_token, User};
use anyhow::{Context, Result};
use axum::http::HeaderMap;
use qpedia_core::tenant::Tenant;
use serde::Deserialize;
use std::collections::HashMap;
use tracing::info;

#[derive(Debug, Clone, Deserialize)]
struct ServiceTokenEntry {
    name: String,
    token: String,
    tenant: String,
    #[serde(default)]
    groups: Vec<String>,
}

#[derive(Clone)]
pub struct ServicePrincipal {
    pub name: String,
    pub tenant: Tenant,
    pub groups: Vec<String>,
}

/// In-memory registry of service tokens, keyed by `sha256(token)`.
#[derive(Clone)]
pub struct ServiceTokenAuth {
    by_hash: HashMap<String, ServicePrincipal>,
}

impl ServiceTokenAuth {
    /// Build from `QPEDIA_SERVICE_TOKENS`. Returns `Ok(None)` when unset/empty,
    /// so M2M auth is strictly opt-in and changes nothing when not configured.
    pub fn from_env() -> Result<Option<Self>> {
        let raw = match std::env::var("QPEDIA_SERVICE_TOKENS") {
            Ok(s) if !s.trim().is_empty() => s,
            _ => return Ok(None),
        };
        let entries: Vec<ServiceTokenEntry> =
            serde_json::from_str(&raw).context("parse QPEDIA_SERVICE_TOKENS as a JSON array")?;

        let mut by_hash = HashMap::new();
        for e in entries {
            if e.token.trim().is_empty() {
                continue;
            }
            by_hash.insert(
                hash_token(e.token.trim()),
                ServicePrincipal {
                    name: e.name,
                    tenant: Tenant::new(e.tenant),
                    groups: e.groups,
                },
            );
        }
        if by_hash.is_empty() {
            return Ok(None);
        }
        info!(count = by_hash.len(), "auth: service-token (M2M) auth enabled");
        Ok(Some(Self { by_hash }))
    }

    /// Authenticate via `Authorization: Bearer <token>`. Returns a synthetic
    /// [`User`] for the matched service principal, or `None` to fall through to
    /// the normal (session-cookie) auth path.
    pub fn authenticate(&self, headers: &HeaderMap) -> Option<User> {
        let token = read_bearer(headers)?;
        let principal = self.by_hash.get(&hash_token(&token))?;
        Some(User {
            id: format!("service:{}", principal.name),
            email: None,
            name: Some(principal.name.clone()),
            groups: principal.groups.clone(),
            tenant: principal.tenant.clone(),
        })
    }
}

/// Extract a bearer token from the `Authorization` header (scheme is case-insensitive).
pub fn read_bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let rest = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))?;
    let token = rest.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_with(token: &str) -> ServiceTokenAuth {
        let mut by_hash = HashMap::new();
        by_hash.insert(
            hash_token(token),
            ServicePrincipal {
                name: "rfp".into(),
                tenant: Tenant::new("acme"),
                groups: vec!["admin".into()],
            },
        );
        ServiceTokenAuth { by_hash }
    }

    fn bearer(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::AUTHORIZATION, value.parse().unwrap());
        h
    }

    #[test]
    fn authenticates_valid_bearer() {
        let user = auth_with("s3cret").authenticate(&bearer("Bearer s3cret")).expect("ok");
        assert_eq!(user.tenant.as_str(), "acme");
        assert_eq!(user.id, "service:rfp");
        assert!(user.groups.contains(&"admin".to_string()));
    }

    #[test]
    fn rejects_unknown_token() {
        assert!(auth_with("s3cret").authenticate(&bearer("Bearer nope")).is_none());
    }

    #[test]
    fn ignores_missing_header() {
        assert!(auth_with("s3cret").authenticate(&HeaderMap::new()).is_none());
    }
}
