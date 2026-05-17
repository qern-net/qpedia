//! Firebase Auth ID-token verification. See SPEC-v2.md §4.
//!
//! Firebase issues short-lived RS256-signed JWTs from a rotating key
//! set published at the Google-hosted JWKS endpoint below. We fetch
//! the JWKS lazily, cache it for an hour, and refresh on cache miss
//! (e.g. after Google rotates a key).
//!
//! The verifier is pure Rust — no Firebase Admin SDK required. The
//! backend never sees a client secret; Firebase config (apiKey,
//! authDomain, providers) lives in the Firebase console.

use anyhow::{anyhow, Context, Result};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, warn};

const FIREBASE_JWKS_URL: &str =
    "https://www.googleapis.com/service_accounts/v1/jwk/securetoken@system.gserviceaccount.com";
const JWKS_TTL: Duration = Duration::from_secs(3600);

/// Claims we read out of a verified Firebase ID token. Standard JWT
/// fields plus Firebase's nested `firebase.sign_in_provider` and any
/// custom claims an admin set via the Firebase Admin SDK
/// (`tenant_id`, `groups`).
#[derive(Debug, Clone, Deserialize)]
pub struct FirebaseClaims {
    pub sub: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub email_verified: Option<bool>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub picture: Option<String>,
    pub firebase: FirebaseProviderClaims,
    /// Custom claim set via Firebase Admin SDK during user provisioning.
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// Custom claim, list of group ids. May be a JSON array or absent.
    #[serde(default)]
    pub groups: Vec<String>,
    pub iss: String,
    pub aud: String,
    pub exp: u64,
    pub iat: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FirebaseProviderClaims {
    pub sign_in_provider: String,
}

/// Verifier handle. Cheap to clone (internal Arc-RwLock).
#[derive(Clone)]
pub struct FirebaseVerifier {
    project_id: String,
    expected_issuer: String,
    http: reqwest::Client,
    keys: Arc<RwLock<KeysCache>>,
}

struct KeysCache {
    keys: HashMap<String, Arc<DecodingKey>>,
    fetched_at: Option<Instant>,
}

impl FirebaseVerifier {
    pub fn new(project_id: impl Into<String>) -> Self {
        let project_id = project_id.into();
        let expected_issuer = format!("https://securetoken.google.com/{project_id}");
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        Self {
            project_id,
            expected_issuer,
            http,
            keys: Arc::new(RwLock::new(KeysCache {
                keys: HashMap::new(),
                fetched_at: None,
            })),
        }
    }

    pub fn project_id(&self) -> &str { &self.project_id }

    /// Verify a Firebase ID token. Returns the claims on success.
    pub async fn verify(&self, id_token: &str) -> Result<FirebaseClaims> {
        let header = decode_header(id_token).context("decode JWT header")?;
        let kid = header.kid.ok_or_else(|| anyhow!("JWT header missing 'kid'"))?;

        let key: Arc<DecodingKey> = match self.cached_key(&kid).await {
            Some(k) => k,
            None => {
                debug!(kid = %kid, "JWKS miss — refreshing");
                self.refresh_jwks().await.context("refresh JWKS")?;
                self.cached_key(&kid)
                    .await
                    .ok_or_else(|| anyhow!("kid {kid} not present after JWKS refresh"))?
            }
        };

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.project_id]);
        validation.set_issuer(&[&self.expected_issuer]);
        // Firebase tokens always carry exp; leeway covers small clock drift.
        validation.leeway = 30;

        let token = decode::<FirebaseClaims>(id_token, &key, &validation)
            .context("verify Firebase ID token")?;
        Ok(token.claims)
    }

    async fn cached_key(&self, kid: &str) -> Option<Arc<DecodingKey>> {
        let guard = self.keys.read().await;
        let fresh = guard
            .fetched_at
            .map(|t| t.elapsed() < JWKS_TTL)
            .unwrap_or(false);
        if !fresh {
            return None;
        }
        guard.keys.get(kid).cloned()
    }

    async fn refresh_jwks(&self) -> Result<()> {
        let resp = self
            .http
            .get(FIREBASE_JWKS_URL)
            .send()
            .await
            .context("fetch JWKS")?
            .error_for_status()
            .context("JWKS http status")?;
        let body: serde_json::Value = resp.json().await.context("parse JWKS")?;

        let mut new_keys: HashMap<String, Arc<DecodingKey>> = HashMap::new();
        let keys_arr = body
            .get("keys")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("JWKS response missing 'keys'"))?;
        for k in keys_arr {
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
            return Err(anyhow!("JWKS contained no usable RSA keys"));
        }

        let mut guard = self.keys.write().await;
        guard.keys = new_keys;
        guard.fetched_at = Some(Instant::now());
        Ok(())
    }
}
