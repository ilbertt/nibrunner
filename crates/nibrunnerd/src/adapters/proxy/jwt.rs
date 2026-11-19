use std::collections::HashMap;

use const_oid::db::{
    rfc5912::{ID_EC_PUBLIC_KEY, RSA_ENCRYPTION, SECP_256_R_1},
    rfc8410::ID_ED_25519,
};
use hyper::header::HeaderName;
use hyper::HeaderMap;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use protocol::{JwtAccessPolicy, JwtPublicKeys, JwtTokenFormat};
use spki::{
    der::{Decode, Document},
    SubjectPublicKeyInfoRef,
};

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
    #[error("a JWT public key is invalid or uses an unsupported signing algorithm")]
    PublicKey,
    #[error("the JWKS URL must use public HTTPS without credentials or a fragment")]
    JwksUrl,
}

pub(crate) struct JwtVerifier {
    header: HeaderName,
    format: JwtTokenFormat,
    keys: VerificationKeys,
    validations: HashMap<Algorithm, Validation>,
}

enum VerificationKeys {
    Pem(Vec<(Algorithm, DecodingKey)>),
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
        let keys = match &policy.public_keys {
            JwtPublicKeys::Pem(pems) => {
                if pems.is_empty() || pems.len() > MAX_PUBLIC_KEYS {
                    return Err(PolicyError::KeyCount);
                }
                let keys = pems
                    .iter()
                    .map(|pem| pem_signing_key(pem))
                    .collect::<Result<Vec<_>, PolicyError>>()?;
                VerificationKeys::Pem(keys)
            }
            JwtPublicKeys::Jwks { url } => {
                let parsed = reqwest::Url::parse(url).map_err(|_| PolicyError::JwksUrl)?;
                if parsed.scheme() != "https"
                    || !super::public_dns::allows_literal(&parsed)
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || parsed.fragment().is_some()
                {
                    return Err(PolicyError::JwksUrl);
                }
                VerificationKeys::Jwks(parsed.to_string())
            }
        };
        let validations = [Algorithm::RS256, Algorithm::ES256, Algorithm::EdDSA]
            .into_iter()
            .map(|algorithm| {
                let mut validation = Validation::new(algorithm);
                validation.set_required_spec_claims(&["exp", "iss", "aud"]);
                validation.set_issuer(&[&policy.issuer]);
                validation.set_audience(&policy.audiences);
                validation.validate_nbf = true;
                validation.leeway = 0;
                (algorithm, validation)
            })
            .collect();
        Ok(Self {
            header,
            format: policy.token.format,
            keys,
            validations,
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
        let Ok(header) = decode_header(token) else {
            return false;
        };
        let Some(validation) = self.validations.get(&header.alg) else {
            return false;
        };
        match &self.keys {
            VerificationKeys::Pem(keys) => keys
                .iter()
                .any(|(algorithm, key)| *algorithm == header.alg && valid_token(token, key, validation)),
            VerificationKeys::Jwks(_) => {
                let Some(kid) = header.kid else {
                    return false;
                };
                let Some(key) = jwks.and_then(|keys| keys.get(&kid, header.alg)) else {
                    return false;
                };
                valid_token(token, key, validation)
            }
        }
    }
}

fn valid_token(token: &str, key: &DecodingKey, validation: &Validation) -> bool {
    decode::<Claims>(token, key, validation).is_ok_and(|token| {
        token.header.crit.is_none()
            && token.claims.exp > jsonwebtoken::get_current_timestamp()
            && validation
                .iss
                .as_ref()
                .is_some_and(|issuers| issuers.contains(&token.claims.iss))
    })
}

fn pem_signing_key(pem: &str) -> Result<(Algorithm, DecodingKey), PolicyError> {
    let (label, document) = Document::from_pem(pem).map_err(|_| PolicyError::PublicKey)?;
    let (algorithm, key) = match label {
        "RSA PUBLIC KEY" => (Algorithm::RS256, DecodingKey::from_rsa_der(document.as_bytes())),
        "PUBLIC KEY" => {
            let info =
                SubjectPublicKeyInfoRef::from_der(document.as_bytes()).map_err(|_| PolicyError::PublicKey)?;
            let bytes = info.subject_public_key.as_bytes().ok_or(PolicyError::PublicKey)?;
            match info.algorithm.oid {
                RSA_ENCRYPTION => (Algorithm::RS256, DecodingKey::from_rsa_der(bytes)),
                ID_EC_PUBLIC_KEY if info.algorithm.parameters_oid().ok() == Some(SECP_256_R_1) => {
                    (Algorithm::ES256, DecodingKey::from_ec_der(bytes))
                }
                ID_ED_25519 if info.algorithm.parameters.is_none() => {
                    (Algorithm::EdDSA, DecodingKey::from_ed_der(bytes))
                }
                _ => return Err(PolicyError::PublicKey),
            }
        }
        _ => return Err(PolicyError::PublicKey),
    };
    (jsonwebtoken::crypto::rust_crypto::DEFAULT_PROVIDER.verifier_factory)(&algorithm, &key)
        .map_err(|_| PolicyError::PublicKey)?;
    Ok((algorithm, key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::jwt::{
        claims, policy, token, token_signed_with_another_key, JwtAlgorithm, OTHER_PUBLIC_KEY,
    };
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
    fn rsa_keys_in_pkcs1_format_verify_rs256() {
        let (_, document) =
            Document::from_pem(include_str!("../../test_support/jwt/rsa-public.pem")).unwrap();
        let info = SubjectPublicKeyInfoRef::from_der(document.as_bytes()).unwrap();
        let pkcs1 = Document::from_der(info.subject_public_key.as_bytes().unwrap())
            .unwrap()
            .to_pem("RSA PUBLIC KEY", spki::der::pem::LineEnding::LF)
            .unwrap();
        let mut policy = policy(JwtAlgorithm::RS256);
        policy.public_keys = JwtPublicKeys::Pem(vec![pkcs1]);
        assert!(JwtVerifier::compile(&policy)
            .unwrap()
            .permits_pem(&headers(&token(JwtAlgorithm::RS256, &claims()))));
    }

    #[test]
    fn an_unsupported_spki_algorithm_is_refused_even_for_an_ed25519_sized_key() {
        use spki::der::{pem::LineEnding, EncodePem};
        let (_, document) = Document::from_pem(include_str!("../../test_support/jwt/ed-public.pem")).unwrap();
        let mut info = SubjectPublicKeyInfoRef::from_der(document.as_bytes()).unwrap();
        info.algorithm.oid = const_oid::db::rfc8410::ID_X_25519;
        assert!(matches!(
            pem_signing_key(&info.to_pem(LineEnding::LF).unwrap()),
            Err(PolicyError::PublicKey)
        ));
    }

    #[test]
    fn a_pem_policy_can_rotate_between_different_key_types() {
        let mut policy = policy(JwtAlgorithm::RS256);
        let JwtPublicKeys::Pem(keys) = &mut policy.public_keys else {
            unreachable!()
        };
        keys.push(include_str!("../../test_support/jwt/ec-public.pem").into());
        let verifier = JwtVerifier::compile(&policy).unwrap();
        for algorithm in [JwtAlgorithm::RS256, JwtAlgorithm::ES256] {
            assert!(verifier.permits_pem(&headers(&token(algorithm, &claims()))));
        }
        assert!(!verifier.permits_pem(&headers(&token(JwtAlgorithm::EdDSA, &claims()))));
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
