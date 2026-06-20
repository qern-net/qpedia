//! OAuth 2 client-credentials (M2M) JWT auth for external applications.
//!
//! The richer sibling of [`crate::m2m`]: instead of a static shared secret, this
//! validates a bearer **access-token JWT** minted by the OIDC provider via the
//! client-credentials grant. Validation mirrors [`crate::firebase`] (RS256 +
//! JWKS, `jsonwebtoken`). The token's tenant claim and scopes drive RLS/ACL
//! exactly as a user session would; an allowlist of client ids (`azp`) gates access.
//!
//! Config (all via env; the verifier is built only when `QPEDIA_M2M_AUDIENCE`
//! is set, so this is strictly opt-in):
//!   - `QPEDIA_M2M_OIDC_ISSUER`     issuer URL (defaults to `QPEDIA_OIDC_ISSUER`)
//!   - `QPEDIA_M2M_AUDIENCE`        expected `aud` (the Qpedia API identifier)
//!   - `QPEDIA_M2M_ALLOWED_CLIENTS` comma/space list of allowed client ids (`azp`)
//!   - `QPEDIA_M2M_REQUIRED_SCOPE`  optional scope the token must carry
//!   - `QPEDIA_M2M_TENANT_CLAIM`    claim holding the tenant (default `tenant`)
//!   - `QPEDIA_M2M_JWKS_URL`        optional explicit JWKS URL (else discovered)

use crate::auth::User;
use anyhow::{anyhow, Context, Result};
use axum::http::HeaderMap;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use qpedia_core::tenant::Tenant;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

const JWKS_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Deserialize)]
struct AccessClaims {
    azp: Option<String>,
    client_id: Option<String>,
    /// Space-delimited scope string (RFC 8693 / most IdPs).
    scope: Option<String>,
    /// Array scope form (Entra and some others).
    scp: Option<Vec<String>>,
    /// Everything else (incl. the tenant claim), captured generically.
    #[serde(flatten)]
    extra: HashMap<String, serde_json::Value>,
}

impl AccessClaims {
    fn client(&self) -> Option<&str> {
        self.azp.as_deref().or(self.client_id.as_deref())
    }
    fn scopes(&self) -> Vec<String> {
        if let Some(s) = &self.scope {
            return s.split(' ').filter(|x| !x.is_empty()).map(String::from).collect();
        }
        self.scp.clone().unwrap_or_default()
    }
}

#[derive(Clone)]
pub struct OAuthVerifier {
    issuer: String,
    audience: String,
    allowed_clients: HashSet<String>,
    required_scope: Option<String>,
    tenant_claim: String,
    jwks_url_override: Option<String>,
    http: reqwest::Client,
    cache: Arc<RwLock<Cache>>,
}

struct Cache {
    jwks_url: Option<String>,
    keys: HashMap<String, Arc<DecodingKey>>,
    fetched_at: Option<Instant>,
}

impl OAuthVerifier {
    /// Build from env. Returns `Ok(None)` unless `QPEDIA_M2M_AUDIENCE` is set.
    pub fn from_env() -> Result<Option<Self>> {
        let audience = match std::env::var("QPEDIA_M2M_AUDIENCE") {
            Ok(a) if !a.trim().is_empty() => a,
            _ => return Ok(None),
        };
        let issuer = std::env::var("QPEDIA_M2M_OIDC_ISSUER")
            .or_else(|_| std::env::var("QPEDIA_OIDC_ISSUER"))
            .context("QPEDIA_M2M_AUDIENCE set but no issuer (QPEDIA_M2M_OIDC_ISSUER / QPEDIA_OIDC_ISSUER)")?;
        let allowed_clients =
            split_list(&std::env::var("QPEDIA_M2M_ALLOWED_CLIENTS").unwrap_or_default());
        let required_scope = std::env::var("QPEDIA_M2M_REQUIRED_SCOPE")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let tenant_claim =
            std::env::var("QPEDIA_M2M_TENANT_CLAIM").unwrap_or_else(|_| "tenant".into());
        let jwks_url_override = std::env::var("QPEDIA_M2M_JWKS_URL")
            .ok()
            .filter(|s| !s.trim().is_empty());

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");

        info!(%issuer, %audience, "auth: OAuth2 M2M JWT auth enabled");
        Ok(Some(Self {
            issuer,
            audience,
            allowed_clients,
            required_scope,
            tenant_claim,
            jwks_url_override,
            http,
            cache: Arc::new(RwLock::new(Cache {
                jwks_url: None,
                keys: HashMap::new(),
                fetched_at: None,
            })),
        }))
    }

    /// Authenticate a request via `Authorization: Bearer <jwt>`. Returns `None`
    /// (fall through) on any validation failure.
    pub async fn authenticate(&self, headers: &HeaderMap) -> Option<User> {
        let token = crate::m2m::read_bearer(headers)?;
        match self.verify(&token).await {
            Ok(user) => Some(user),
            Err(e) => {
                debug!(error = %e, "M2M JWT rejected");
                None
            }
        }
    }

    async fn verify(&self, token: &str) -> Result<User> {
        let header = decode_header(token).context("decode JWT header")?;
        let kid = header.kid.ok_or_else(|| anyhow!("JWT header missing 'kid'"))?;

        let key: Arc<DecodingKey> = match self.cached_key(&kid).await {
            Some(k) => k,
            None => {
                debug!(kid = %kid, "M2M JWKS miss — refreshing");
                self.refresh_jwks().await.context("refresh JWKS")?;
                self.cached_key(&kid)
                    .await
                    .ok_or_else(|| anyhow!("kid {kid} absent after JWKS refresh"))?
            }
        };

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.leeway = 30;

        let claims = decode::<AccessClaims>(token, &key, &validation)
            .context("verify access token")?
            .claims;

        // Client allowlist (azp / client_id).
        let client = claims
            .client()
            .ok_or_else(|| anyhow!("token has no azp/client_id"))?
            .to_string();
        if !self.allowed_clients.is_empty() && !self.allowed_clients.contains(&client) {
            return Err(anyhow!("client {client} not in allowlist"));
        }

        // Required scope.
        let scopes = claims.scopes();
        if let Some(req) = &self.required_scope {
            if !scopes.iter().any(|s| s == req) {
                return Err(anyhow!("missing required scope {req}"));
            }
        }

        // Tenant claim drives RLS scoping.
        let tenant = claims
            .extra
            .get(&self.tenant_claim)
            .and_then(|v| v.as_str())
            .map(Tenant::new)
            .ok_or_else(|| anyhow!("token missing tenant claim {:?}", self.tenant_claim))?;

        // Scopes double as groups so wiki/source ACLs can match scope names.
        Ok(User {
            id: format!("service:{client}"),
            email: None,
            name: Some(client),
            groups: scopes,
            tenant,
        })
    }

    async fn cached_key(&self, kid: &str) -> Option<Arc<DecodingKey>> {
        let g = self.cache.read().await;
        let fresh = g.fetched_at.map(|t| t.elapsed() < JWKS_TTL).unwrap_or(false);
        if !fresh {
            return None;
        }
        g.keys.get(kid).cloned()
    }

    async fn jwks_url(&self) -> Result<String> {
        if let Some(u) = &self.jwks_url_override {
            return Ok(u.clone());
        }
        {
            let g = self.cache.read().await;
            if let Some(u) = &g.jwks_url {
                return Ok(u.clone());
            }
        }
        let disco = format!(
            "{}/.well-known/openid-configuration",
            self.issuer.trim_end_matches('/')
        );
        let meta: serde_json::Value = self
            .http
            .get(&disco)
            .send()
            .await
            .context("fetch oidc discovery")?
            .error_for_status()
            .context("discovery status")?
            .json()
            .await
            .context("parse discovery")?;
        let url = meta
            .get("jwks_uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("discovery missing jwks_uri"))?
            .to_string();
        self.cache.write().await.jwks_url = Some(url.clone());
        Ok(url)
    }

    async fn refresh_jwks(&self) -> Result<()> {
        let url = self.jwks_url().await?;
        let body: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .context("fetch JWKS")?
            .error_for_status()
            .context("JWKS status")?
            .json()
            .await
            .context("parse JWKS")?;

        let mut new_keys: HashMap<String, Arc<DecodingKey>> = HashMap::new();
        let arr = body
            .get("keys")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("JWKS missing 'keys'"))?;
        for k in arr {
            let kid = k.get("kid").and_then(|v| v.as_str());
            let n = k.get("n").and_then(|v| v.as_str());
            let e = k.get("e").and_then(|v| v.as_str());
            if let (Some(kid), Some(n), Some(e)) = (kid, n, e) {
                match DecodingKey::from_rsa_components(n, e) {
                    Ok(dk) => {
                        new_keys.insert(kid.to_string(), Arc::new(dk));
                    }
                    Err(err) => warn!(kid = %kid, error = %err, "bad RSA components in JWKS"),
                }
            }
        }
        if new_keys.is_empty() {
            return Err(anyhow!("JWKS had no usable RSA keys"));
        }

        let mut g = self.cache.write().await;
        g.keys = new_keys;
        g.fetched_at = Some(Instant::now());
        Ok(())
    }
}

fn split_list(s: &str) -> HashSet<String> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}
