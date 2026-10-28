use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use futures::{stream, StreamExt};
use jsonwebtoken::jwk::{
    AlgorithmParameters, EllipticCurve, JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse,
};
use jsonwebtoken::{Algorithm, DecodingKey};
use tokio::sync::RwLock;
use tokio::time::Instant;

const MAX_KEYS: usize = 32;
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_CONCURRENT_FETCHES: usize = 4;
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const REFRESH_INTERVAL: Duration = Duration::from_secs(300);
pub(super) const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const MAX_KEY_AGE: Duration = Duration::from_secs(900);

#[derive(Debug, thiserror::Error)]
pub(super) enum FetchError {
    #[error("the JWKS request failed: {0}")]
    Request(reqwest::Error),
    #[error("the JWKS endpoint returned HTTP {0}")]
    Status(reqwest::StatusCode),
    #[error("the JWKS response exceeds 256 KiB")]
    BodySize,
    #[error("the JWKS document is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the JWKS document contains more than 32 keys")]
    KeyCount,
    #[error("a JWKS signing key is invalid or has a missing, empty or duplicate kid")]
    Key,
}

pub(super) struct JwksKeys {
    keys: BTreeMap<String, (Algorithm, DecodingKey)>,
}

impl JwksKeys {
    fn parse(bytes: &[u8]) -> Result<Self, FetchError> {
        let set: JwkSet = serde_json::from_slice(bytes)?;
        if set.keys.len() > MAX_KEYS {
            return Err(FetchError::KeyCount);
        }
        let mut keys = BTreeMap::new();
        for jwk in set.keys {
            let (algorithm, key_algorithm) = match &jwk.algorithm {
                AlgorithmParameters::RSA(_) => (Algorithm::RS256, KeyAlgorithm::RS256),
                AlgorithmParameters::EllipticCurve(key) if key.curve == EllipticCurve::P256 => {
                    (Algorithm::ES256, KeyAlgorithm::ES256)
                }
                AlgorithmParameters::OctetKeyPair(key) if key.curve == EllipticCurve::Ed25519 => {
                    (Algorithm::EdDSA, KeyAlgorithm::EdDSA)
                }
                _ => continue,
            };
            if jwk
                .common
                .public_key_use
                .as_ref()
                .is_some_and(|usage| *usage != PublicKeyUse::Signature)
                || jwk
                    .common
                    .key_operations
                    .as_ref()
                    .is_some_and(|operations| !operations.contains(&KeyOperations::Verify))
                || jwk.common.key_algorithm.is_some_and(|alg| alg != key_algorithm)
            {
                continue;
            }
            let kid = jwk
                .common
                .key_id
                .as_ref()
                .filter(|kid| !kid.is_empty())
                .ok_or(FetchError::Key)?;
            let key = DecodingKey::from_jwk(&jwk).map_err(|_| FetchError::Key)?;
            (jsonwebtoken::crypto::rust_crypto::DEFAULT_PROVIDER.verifier_factory)(&algorithm, &key)
                .map_err(|_| FetchError::Key)?;
            if keys.insert(kid.clone(), (algorithm, key)).is_some() {
                return Err(FetchError::Key);
            }
        }
        Ok(Self { keys })
    }

    pub(super) fn get(&self, kid: &str, algorithm: Algorithm) -> Option<&DecodingKey> {
        self.keys
            .get(kid)
            .filter(|(expected, _)| *expected == algorithm)
            .map(|(_, key)| key)
    }
}

pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    crate::install_crypto_provider();
    reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(FETCH_TIMEOUT)
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<JwksKeys, FetchError> {
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|error| FetchError::Request(error.without_url()))?;
    if !response.status().is_success() {
        return Err(FetchError::Status(response.status()));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return Err(FetchError::BodySize);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| FetchError::Request(error.without_url()))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_BODY_BYTES {
            return Err(FetchError::BodySize);
        }
        body.extend_from_slice(&chunk);
    }
    JwksKeys::parse(&body)
}

struct CachedKeys {
    keys: Option<Arc<JwksKeys>>,
    expires_at: Instant,
    next_refresh: Instant,
}

#[derive(Default)]
pub(super) struct JwksCache {
    entries: RwLock<BTreeMap<String, CachedKeys>>,
}

impl JwksCache {
    pub(super) async fn get(&self, url: &str) -> Option<Arc<JwksKeys>> {
        let entries = self.entries.read().await;
        let entry = entries.get(url)?;
        (Instant::now() < entry.expires_at)
            .then(|| entry.keys.clone())
            .flatten()
    }

    pub(super) async fn refresh(&self, client: &reqwest::Client, urls: BTreeSet<String>) {
        let due = {
            let mut entries = self.entries.write().await;
            entries.retain(|url, _| urls.contains(url));
            let now = Instant::now();
            urls.into_iter()
                .filter(|url| {
                    let entry = entries.entry(url.clone()).or_insert(CachedKeys {
                        keys: None,
                        expires_at: now,
                        next_refresh: now,
                    });
                    if now < entry.next_refresh {
                        return false;
                    }
                    entry.next_refresh = now + RETRY_INTERVAL;
                    true
                })
                .collect::<Vec<_>>()
        };
        let mut fetched = stream::iter(due.into_iter().map(|url| async move {
            let result = fetch(client, &url).await;
            (url, result)
        }))
        .buffer_unordered(MAX_CONCURRENT_FETCHES);
        while let Some((url, result)) = fetched.next().await {
            let mut entries = self.entries.write().await;
            let Some(entry) = entries.get_mut(&url) else {
                continue;
            };
            match result {
                Ok(keys) => {
                    entry.keys = Some(Arc::new(keys));
                    entry.expires_at = Instant::now() + MAX_KEY_AGE;
                    entry.next_refresh = Instant::now() + REFRESH_INTERVAL;
                }
                Err(error) => tracing::warn!(%error, "JWT signing keys could not be refreshed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use hyper::{HeaderMap, StatusCode};
    use protocol::{JwtAlgorithm, JwtPublicKeys};
    use serde_json::json;

    use super::*;
    use crate::adapters::proxy::jwt::JwtVerifier;
    use crate::test_support::jwt::{claims, jwk, policy, token, token_with_kid, JwksEndpoint};

    fn remote_verifier(algorithm: JwtAlgorithm) -> JwtVerifier {
        let mut policy = policy(algorithm);
        policy.public_keys = JwtPublicKeys::Jwks {
            url: "https://issuer.example.com/keys".into(),
        };
        JwtVerifier::compile(&policy).unwrap()
    }

    fn headers(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("cf-access-jwt-assertion", token.parse().unwrap());
        headers
    }

    async fn elapse(duration: Duration) {
        tokio::time::pause();
        tokio::time::advance(duration).await;
        tokio::time::resume();
    }

    #[test]
    fn jwks_keys_verify_each_supported_algorithm_and_require_a_matching_kid() {
        for algorithm in [JwtAlgorithm::RS256, JwtAlgorithm::ES256, JwtAlgorithm::EdDSA] {
            let keys =
                JwksKeys::parse(&serde_json::to_vec(&json!({"keys": [jwk(algorithm, "current")]})).unwrap())
                    .unwrap();
            let verifier = remote_verifier(algorithm);
            assert!(verifier.permits(
                &headers(&token_with_kid(algorithm, &claims(), "current")),
                Some(&keys)
            ));
            assert!(!verifier.permits(
                &headers(&token_with_kid(algorithm, &claims(), "unknown")),
                Some(&keys)
            ));
            assert!(!verifier.permits(&headers(&token(algorithm, &claims())), Some(&keys)));
            assert!(!verifier.permits(&headers(&token_with_kid(algorithm, &claims(), "current")), None));
            for (claim, value) in [
                ("iss", json!("other")),
                ("aud", json!("other")),
                ("exp", json!(0)),
                ("nbf", json!(jsonwebtoken::get_current_timestamp() + 3600)),
            ] {
                let mut invalid = claims();
                invalid[claim] = value;
                assert!(!verifier.permits(
                    &headers(&token_with_kid(algorithm, &invalid, "current")),
                    Some(&keys)
                ));
            }
        }
        let keys = JwksKeys::parse(
            &serde_json::to_vec(&json!({"keys": [jwk(JwtAlgorithm::ES256, "current")]})).unwrap(),
        )
        .unwrap();
        assert!(!remote_verifier(JwtAlgorithm::RS256).permits(
            &headers(&token_with_kid(JwtAlgorithm::ES256, &claims(), "current")),
            Some(&keys)
        ));
    }

    #[test]
    fn encryption_symmetric_and_mismatched_algorithm_keys_cannot_verify_tokens() {
        let original = jwk(JwtAlgorithm::EdDSA, "current");
        for (field, value) in [
            ("use", json!("enc")),
            ("key_ops", json!(["sign"])),
            ("alg", json!("RS256")),
        ] {
            let mut key = original.clone();
            key[field] = value;
            let keys = JwksKeys::parse(&serde_json::to_vec(&json!({"keys": [key]})).unwrap()).unwrap();
            assert!(keys.get("current", Algorithm::EdDSA).is_none());
        }
        let keys =
            JwksKeys::parse(br#"{"keys":[{"kty":"oct","kid":"current","alg":"HS256","k":"c2VjcmV0"}]}"#)
                .unwrap();
        assert!(keys.keys.is_empty());
        let mut key = original;
        key.as_object_mut().unwrap().remove("alg");
        let keys = JwksKeys::parse(&serde_json::to_vec(&json!({"keys": [key]})).unwrap()).unwrap();
        assert!(keys.get("current", Algorithm::EdDSA).is_some());
    }

    #[test]
    fn malformed_keys_and_ambiguous_key_ids_cannot_replace_the_cache() {
        let original = jwk(JwtAlgorithm::EdDSA, "current");
        let mut no_kid = original.clone();
        no_kid.as_object_mut().unwrap().remove("kid");
        let mut invalid = original.clone();
        invalid["x"] = json!("not valid base64!");
        for keys in [
            json!([original.clone(), original.clone()]),
            json!([no_kid]),
            json!([invalid]),
            json!(vec![original; MAX_KEYS + 1]),
        ] {
            assert!(JwksKeys::parse(&serde_json::to_vec(&json!({"keys": keys})).unwrap()).is_err());
        }
        assert!(JwksKeys::parse(b"{}").is_err());
        assert!(JwksKeys::parse(b"not JSON").is_err());
        assert!(JwksKeys::parse(br#"{"keys":[]}"#).unwrap().keys.is_empty());
    }

    #[tokio::test]
    async fn cached_keys_refresh_without_requests_and_removed_keys_stop_working() {
        let endpoint = JwksEndpoint::new(json!({"keys": [jwk(JwtAlgorithm::EdDSA, "old")]})).await;
        let cache = JwksCache::default();
        let urls = BTreeSet::from([endpoint.url.clone()]);
        cache.refresh(&endpoint.client, urls.clone()).await;
        let first = cache.get(&endpoint.url).await.unwrap();
        assert!(first.get("old", Algorithm::EdDSA).is_some());
        cache.refresh(&endpoint.client, urls.clone()).await;
        assert_eq!(endpoint.requests.load(Ordering::SeqCst), 1);

        let key = DecodingKey::from_ed_pem(crate::test_support::jwt::OTHER_PUBLIC_KEY.as_bytes()).unwrap();
        let mut rotated = jsonwebtoken::jwk::Jwk::from_decoding_key(&key, Some(Algorithm::EdDSA)).unwrap();
        rotated.common.key_id = Some("new".into());
        endpoint.respond(StatusCode::OK, json!({"keys": [rotated]}).to_string());
        elapse(REFRESH_INTERVAL).await;
        cache.refresh(&endpoint.client, urls.clone()).await;
        let keys = cache.get(&endpoint.url).await.unwrap();
        assert!(keys.get("old", Algorithm::EdDSA).is_none());
        let mut header = jsonwebtoken::Header::new(Algorithm::EdDSA);
        header.kid = Some("new".into());
        let signing = jsonwebtoken::EncodingKey::from_ed_pem(include_bytes!(
            "../../test_support/jwt/other-ed-private.pem"
        ))
        .unwrap();
        let signed = jsonwebtoken::encode(&header, &claims(), &signing).unwrap();
        assert!(remote_verifier(JwtAlgorithm::EdDSA).permits(&headers(&signed), Some(&keys)));

        endpoint.respond(StatusCode::OK, json!({"keys": []}).to_string());
        elapse(REFRESH_INTERVAL).await;
        cache.refresh(&endpoint.client, urls).await;
        assert!(cache.get(&endpoint.url).await.unwrap().keys.is_empty());
        cache.refresh(&endpoint.client, BTreeSet::new()).await;
        assert!(cache.get(&endpoint.url).await.is_none());
    }

    #[tokio::test]
    async fn refresh_failures_keep_keys_only_until_their_original_expiry_and_then_recover() {
        let document = json!({"keys": [jwk(JwtAlgorithm::EdDSA, "current")]});
        let endpoint = JwksEndpoint::new(document.clone()).await;
        let cache = JwksCache::default();
        let urls = BTreeSet::from([endpoint.url.clone()]);
        endpoint.respond(StatusCode::SERVICE_UNAVAILABLE, String::new());
        cache.refresh(&endpoint.client, urls.clone()).await;
        assert!(cache.get(&endpoint.url).await.is_none());
        endpoint.respond(StatusCode::OK, document.to_string());
        elapse(RETRY_INTERVAL).await;
        cache.refresh(&endpoint.client, urls.clone()).await;
        assert!(cache.get(&endpoint.url).await.is_some());
        endpoint.respond(StatusCode::OK, "not JSON".into());
        elapse(REFRESH_INTERVAL).await;
        cache.refresh(&endpoint.client, urls.clone()).await;
        assert!(cache.get(&endpoint.url).await.is_some());
        let attempts = endpoint.requests.load(Ordering::SeqCst);
        cache.refresh(&endpoint.client, urls.clone()).await;
        assert_eq!(endpoint.requests.load(Ordering::SeqCst), attempts);
        elapse(MAX_KEY_AGE).await;
        cache.refresh(&endpoint.client, urls.clone()).await;
        assert!(cache.get(&endpoint.url).await.is_none());
        endpoint.respond(StatusCode::OK, document.to_string());
        elapse(RETRY_INTERVAL).await;
        cache.refresh(&endpoint.client, urls).await;
        assert!(cache.get(&endpoint.url).await.is_some());
    }

    #[tokio::test]
    async fn untrusted_tls_redirects_http_errors_and_oversized_responses_are_refused() {
        let endpoint = JwksEndpoint::new(json!({"keys": []})).await;
        assert!(fetch(&client_builder().build().unwrap(), &endpoint.url)
            .await
            .is_err());
        for status in [
            StatusCode::FOUND,
            StatusCode::NOT_FOUND,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            endpoint.respond(status, "not keys".into());
            assert!(
                matches!(fetch(&endpoint.client, &endpoint.url).await, Err(FetchError::Status(received)) if received == status)
            );
        }
        endpoint.respond(StatusCode::OK, " ".repeat(MAX_BODY_BYTES + 1));
        assert!(matches!(
            fetch(&endpoint.client, &endpoint.url).await,
            Err(FetchError::BodySize)
        ));
        assert!(fetch(&client_builder().build().unwrap(), "http://127.0.0.1/keys")
            .await
            .is_err());
    }

    #[test]
    fn the_jwks_client_can_start_before_any_tls_listener() {
        assert!(client_builder().build().is_ok());
    }

    #[test]
    fn jwks_configuration_requires_an_explicit_https_endpoint() {
        for url in [
            "http://issuer.example.com/keys",
            "file:///keys",
            "not a URL",
            "https://user:pass@issuer.example.com/keys",
            "https://issuer.example.com/keys#fragment",
        ] {
            let mut policy = policy(JwtAlgorithm::EdDSA);
            policy.public_keys = JwtPublicKeys::Jwks { url: url.into() };
            assert!(JwtVerifier::compile(&policy).is_err());
        }
    }
}
