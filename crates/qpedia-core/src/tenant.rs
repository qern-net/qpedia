//! Tenant scoping. Every user-visible resource (Source, wiki page,
//! folder, ACL, audit row) lives inside exactly one tenant. Postgres
//! RLS enforces isolation server-side; per-request the app calls
//! `SET LOCAL ROLE qpedia_app` + `set_config('qpedia.tenant', ...)`
//! and every RLS policy compares row.tenant_id to the GUC.
//!
//! - Dev mode: tenant defaults to "default" or the env override
//!   `QPEDIA_DEV_TENANT`.
//! - Firebase: tenant comes from a custom claim, then falls back to
//!   the user's email domain (`tenants.email_domain` lookup), then
//!   `QPEDIA_DEV_TENANT`, then "default".
//! - Legacy OIDC: tenant comes from a claim — `QPEDIA_OIDC_TENANT_CLAIM`,
//!   default `tenant_id` — and falls back to "default" if missing.

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
            // a directory name (per-tenant wiki repo) and an unescaped
            // Postgres GUC value.
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
