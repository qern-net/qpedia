//! Tenant scoping. Every user-visible resource (Source, wiki page,
//! Weaviate object, folder ACL) lives inside exactly one tenant.
//!
//! - Dev mode: tenant defaults to "default" or the env override
//!   `QPEDIA_DEV_TENANT`.
//! - OIDC mode: tenant comes from a claim — `QPEDIA_OIDC_TENANT_CLAIM`,
//!   default `tenant_id` — and falls back to "default" if missing.
//!
//! No tenant admin yet; tenants are configured by the IdP and created
//! implicitly the first time a user from a new tenant signs in.

use serde::{Deserialize, Serialize};
use std::fmt;

pub const DEFAULT_TENANT: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tenant(pub String);

impl Tenant {
    pub fn new(s: impl Into<String>) -> Self {
        let raw = s.into();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            Self(DEFAULT_TENANT.into())
        } else {
            // Restrict to safe characters so the value can be used as
            // a directory name (per-tenant wiki repo) and a Weaviate
            // property value without escaping.
            let sanitized: String = trimmed
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
                .collect();
            Self(sanitized.to_lowercase())
        }
    }

    pub fn default_tenant() -> Self { Self(DEFAULT_TENANT.into()) }

    pub fn as_str(&self) -> &str { &self.0 }
}

impl Default for Tenant {
    fn default() -> Self { Self::default_tenant() }
}

impl fmt::Display for Tenant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for Tenant {
    fn from(s: String) -> Self { Self::new(s) }
}

impl From<&str> for Tenant {
    fn from(s: &str) -> Self { Self::new(s) }
}
