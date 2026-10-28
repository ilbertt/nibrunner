use hyper::header::HeaderName;
use hyper::HeaderMap;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use protocol::{JwtAccessPolicy, JwtAlgorithm, JwtPublicKeys, JwtTokenFormat};

use super::jwks::JwksKeys;
use serde::Deserialize;

const MAX_PUBLIC_KEYS: usize = 8;
const MAX_TOKEN_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum PolicyError {
    #[error("the JWT header name is invalid")]
    Header,
    #[error("the JWT issuer must not be empty")]
    Issuer,
    #[error("the JWT policy must name at least one nonempty audience")]
    Audiences,
    #[error("the JWT policy must contain between one and eight public keys")]
    KeyCount,
    #[error("a JWT public key is invalid for its configured algorithm")]
    PublicKey,
    #[error("the JWKS URL must use HTTPS without credentials or a fragment")]
    JwksUrl,
}

pub(crate) struct JwtVerifier {
    header: HeaderName,
    format: JwtTokenFormat,
    keys: VerificationKeys,
    validation: Validation,
}

enum VerificationKeys {
    Pem(Vec<DecodingKey>),
    Jwks(String),
}

#[derive(Deserialize)]
struct Claims {
    exp: u64,
    iss: String,
}

impl JwtVerifier {
    pub(crate) fn compile(policy: &JwtAccessPolicy) -> Result<Self, PolicyError> {
        let header =
            HeaderName::from_bytes(policy.token.header.as_bytes()).map_err(|_| PolicyError::Header)?;
        if policy.issuer.trim().is_empty() {
            return Err(PolicyError::Issuer);
        }
        if policy.audiences.is_empty() || policy.audiences.iter().any(|audience| audience.trim().is_empty()) {
            return Err(PolicyError::Audiences);
        }
        let algorithm = match policy.algorithm {
            JwtAlgorithm::RS256 => Algorithm::RS256,
            JwtAlgorithm::ES256 => Algorithm::ES256,
            JwtAlgorithm::EdDSA => Algorithm::EdDSA,
        };
        let keys = match &policy.public_keys {
            JwtPublicKeys::Pem(pems) => {
                if pems.is_empty() || pems.len() > MAX_PUBLIC_KEYS {
                    return Err(PolicyError::KeyCount);
                }
                let keys = pems
                    .iter()
                    .map(|pem| {
                        let key = match policy.algorithm {
                            JwtAlgorithm::RS256 => DecodingKey::from_rsa_pem(pem.as_bytes()),
                            JwtAlgorithm::ES256 => DecodingKey::from_ec_pem(pem.as_bytes()),
                            JwtAlgorithm::EdDSA => DecodingKey::from_ed_pem(pem.as_bytes()),
                        }
                        .map_err(|_| PolicyError::PublicKey)?;
                        (jsonwebtoken::crypto::rust_crypto::DEFAULT_PROVIDER.verifier_factory)(
                            &algorithm, &key,
                        )
                        .map_err(|_| PolicyError::PublicKey)?;
                        Ok(key)
                    })
                    .collect::<Result<Vec<_>, PolicyError>>()?;
                VerificationKeys::Pem(keys)
            }
            JwtPublicKeys::Jwks { url } => {
                let parsed = reqwest::Url::parse(url).map_err(|_| PolicyError::JwksUrl)?;
                if parsed.scheme() != "https"
                    || parsed.host_str().is_none()
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || parsed.fragment().is_some()
                {
                    return Err(PolicyError::JwksUrl);
                }
                VerificationKeys::Jwks(parsed.to_string())
            }
        };
        let mut validation = Validation::new(algorithm);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.set_issuer(&[&policy.issuer]);
        validation.set_audience(&policy.audiences);
        validation.validate_nbf = true;
        validation.leeway = 0;
        Ok(Self {
            header,
            format: policy.token.format,
            keys,
            validation,
        })
    }

    pub(crate) fn jwks_url(&self) -> Option<&str> {
        match &self.keys {
            VerificationKeys::Pem(_) => None,
            VerificationKeys::Jwks(url) => Some(url),
        }
    }

    pub(super) fn permits(&self, headers: &HeaderMap, jwks: Option<&JwksKeys>) -> bool {
        let mut values = headers.get_all(&self.header).iter();
        let Some(value) = values.next().and_then(|value| value.to_str().ok()) else {
            return false;
        };
        if values.next().is_some() || value.len() > MAX_TOKEN_BYTES {
            return false;
        }
        let token = match self.format {
            JwtTokenFormat::Raw => value,
            JwtTokenFormat::Bearer => {
                let Some((scheme, token)) = value.split_once(' ') else {
                    return false;
                };
                if !scheme.eq_ignore_ascii_case("bearer") {
                    return false;
                }
                token
            }
        };
        match &self.keys {
            VerificationKeys::Pem(keys) => keys.iter().any(|key| self.valid_token(token, key)),
            VerificationKeys::Jwks(_) => {
                let Ok(header) = decode_header(token) else {
                    return false;
                };
                let Some(kid) = header.kid else {
                    return false;
                };
                let Some(key) = jwks.and_then(|keys| keys.get(&kid, header.alg)) else {
                    return false;
                };
                self.valid_token(token, key)
            }
        }
    }

    fn valid_token(&self, token: &str, key: &DecodingKey) -> bool {
        decode::<Claims>(token, key, &self.validation).is_ok_and(|token| {
            token.header.crit.is_none()
                && token.claims.exp > jsonwebtoken::get_current_timestamp()
                && self
                    .validation
                    .iss
                    .as_ref()
                    .is_some_and(|issuers| issuers.contains(&token.claims.iss))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::jwt::{claims, policy, token, token_signed_with_another_key, OTHER_PUBLIC_KEY};
    use hyper::header::HeaderValue;
    use serde_json::json;

    impl JwtVerifier {
        fn permits_pem(&self, headers: &HeaderMap) -> bool {
            self.permits(headers, None)
        }
    }

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("cf-access-jwt-assertion", HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn every_supported_algorithm_verifies_with_its_matching_public_key() {
        for algorithm in [JwtAlgorithm::RS256, JwtAlgorithm::ES256, JwtAlgorithm::EdDSA] {
            let verifier = JwtVerifier::compile(&policy(algorithm)).unwrap();
            assert!(verifier.permits_pem(&headers(&token(algorithm, &claims()))));
        }
    }

    #[test]
    fn a_token_cannot_choose_another_algorithm_or_signing_key() {
        let verifier = JwtVerifier::compile(&policy(JwtAlgorithm::EdDSA)).unwrap();
        assert!(!verifier.permits_pem(&headers(&token(JwtAlgorithm::ES256, &claims()))));
        assert!(!verifier.permits_pem(&headers(&token_signed_with_another_key())));
        let signed = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::HS256),
            &claims(),
            &jsonwebtoken::EncodingKey::from_secret(include_bytes!("../../test_support/jwt/ed-public.pem")),
        )
        .unwrap();
        assert!(!verifier.permits_pem(&headers(&signed)));
    }

    #[test]
    fn a_valid_signature_does_not_allow_unsupported_critical_extensions() {
        let verifier = JwtVerifier::compile(&policy(JwtAlgorithm::EdDSA)).unwrap();
        let key =
            jsonwebtoken::EncodingKey::from_ed_pem(include_bytes!("../../test_support/jwt/ed-private.pem"))
                .unwrap();
        let mut header = jsonwebtoken::Header::new(Algorithm::EdDSA);
        header.crit = Some(vec!["unknown-extension".to_string()]);
        let signed = jsonwebtoken::encode(&header, &claims(), &key).unwrap();
        assert!(!verifier.permits_pem(&headers(&signed)));
    }

    #[test]
    fn required_claims_and_validity_times_are_enforced() {
        let verifier = JwtVerifier::compile(&policy(JwtAlgorithm::EdDSA)).unwrap();
        for required in ["iss", "aud", "exp"] {
            let mut claims = claims();
            claims.as_object_mut().unwrap().remove(required);
            assert!(
                !verifier.permits_pem(&headers(&token(JwtAlgorithm::EdDSA, &claims))),
                "{required}"
            );
        }
        for (claim, value) in [
            ("iss", json!("https://other.example.com")),
            ("iss", json!(["https://issuer.example.com"])),
            ("aud", json!("another-app")),
            ("aud", json!([])),
            ("exp", json!(0)),
            ("exp", json!(jsonwebtoken::get_current_timestamp())),
            ("exp", json!("tomorrow")),
            ("nbf", json!(jsonwebtoken::get_current_timestamp() + 3600)),
            ("nbf", json!("yesterday")),
        ] {
            let mut claims = claims();
            claims[claim] = value;
            assert!(
                !verifier.permits_pem(&headers(&token(JwtAlgorithm::EdDSA, &claims))),
                "{claims}"
            );
        }
    }

    #[test]
    fn one_matching_audience_in_a_list_is_enough() {
        let verifier = JwtVerifier::compile(&policy(JwtAlgorithm::EdDSA)).unwrap();
        let mut claims = claims();
        claims["aud"] = json!(["another-app", "app-one"]);
        assert!(verifier.permits_pem(&headers(&token(JwtAlgorithm::EdDSA, &claims))));
    }

    #[test]
    fn missing_duplicate_malformed_and_oversized_headers_are_refused() {
        let verifier = JwtVerifier::compile(&policy(JwtAlgorithm::EdDSA)).unwrap();
        assert!(!verifier.permits_pem(&HeaderMap::new()));
        for value in ["", "not.a.token", &"a".repeat(MAX_TOKEN_BYTES + 1)] {
            assert!(!verifier.permits_pem(&headers(value)));
        }
        let signed = token(JwtAlgorithm::EdDSA, &claims());
        let mut duplicate = headers(&signed);
        duplicate.append("cf-access-jwt-assertion", HeaderValue::from_str(&signed).unwrap());
        assert!(!verifier.permits_pem(&duplicate));
        duplicate.clear();
        duplicate.insert(
            "cf-access-jwt-assertion",
            HeaderValue::from_bytes(&[0xff]).unwrap(),
        );
        assert!(!verifier.permits_pem(&duplicate));
    }

    #[test]
    fn bearer_extraction_uses_only_the_configured_header() {
        let mut policy = policy(JwtAlgorithm::EdDSA);
        policy.token.header = "Authorization".to_string();
        policy.token.format = JwtTokenFormat::Bearer;
        let verifier = JwtVerifier::compile(&policy).unwrap();
        let signed = token(JwtAlgorithm::EdDSA, &claims());
        assert!(!verifier.permits_pem(&headers(&signed)));
        for scheme in ["Bearer", "bearer", "BEARER"] {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", format!("{scheme} {signed}").parse().unwrap());
            assert!(verifier.permits_pem(&headers));
        }
        for value in [
            signed.clone(),
            format!("Basic {signed}"),
            format!("Bearer  {signed}"),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("authorization", value.parse().unwrap());
            assert!(!verifier.permits_pem(&headers));
        }
    }

    #[test]
    fn a_supplied_key_can_rotate_without_accepting_a_key_that_was_removed() {
        let mut policy = policy(JwtAlgorithm::EdDSA);
        policy.public_keys = JwtPublicKeys::Pem(vec![
            include_str!("../../test_support/jwt/ed-public.pem").to_string(),
            OTHER_PUBLIC_KEY.to_string(),
        ]);
        let verifier = JwtVerifier::compile(&policy).unwrap();
        assert!(verifier.permits_pem(&headers(&token(JwtAlgorithm::EdDSA, &claims()))));
        assert!(verifier.permits_pem(&headers(&token_signed_with_another_key())));
        policy.public_keys = JwtPublicKeys::Pem(vec![OTHER_PUBLIC_KEY.to_string()]);
        let verifier = JwtVerifier::compile(&policy).unwrap();
        assert!(!verifier.permits_pem(&headers(&token(JwtAlgorithm::EdDSA, &claims()))));
        assert!(verifier.permits_pem(&headers(&token_signed_with_another_key())));
    }

    #[test]
    fn incomplete_or_invalid_policies_are_refused_before_any_request() {
        for (edit, expected) in [
            (0, PolicyError::Header),
            (1, PolicyError::Issuer),
            (2, PolicyError::Audiences),
            (3, PolicyError::KeyCount),
            (4, PolicyError::PublicKey),
            (5, PolicyError::KeyCount),
        ] {
            let mut policy = policy(JwtAlgorithm::EdDSA);
            match edit {
                0 => policy.token.header = "invalid header".to_string(),
                1 => policy.issuer.clear(),
                2 => policy.audiences.clear(),
                3 => policy.public_keys = JwtPublicKeys::Pem(vec![]),
                4 => policy.public_keys = JwtPublicKeys::Pem(vec!["not a key".to_string()]),
                5 => {
                    policy.public_keys =
                        JwtPublicKeys::Pem(vec![OTHER_PUBLIC_KEY.to_string(); MAX_PUBLIC_KEYS + 1])
                }
                _ => unreachable!(),
            }
            assert!(matches!(JwtVerifier::compile(&policy), Err(error) if error == expected));
        }
    }
}
