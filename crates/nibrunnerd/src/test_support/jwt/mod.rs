use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use protocol::{JwtAccessPolicy, JwtAlgorithm, JwtTokenFormat, JwtTokenSource};
use serde_json::{json, Value};

pub const OTHER_PUBLIC_KEY: &str = include_str!("other-ed-public.pem");

pub fn policy(algorithm: JwtAlgorithm) -> JwtAccessPolicy {
    let public_key = match algorithm {
        JwtAlgorithm::RS256 => include_str!("rsa-public.pem"),
        JwtAlgorithm::ES256 => include_str!("ec-public.pem"),
        JwtAlgorithm::EdDSA => include_str!("ed-public.pem"),
    };
    JwtAccessPolicy {
        token: JwtTokenSource {
            header: "Cf-Access-Jwt-Assertion".to_string(),
            format: JwtTokenFormat::Raw,
        },
        issuer: "https://issuer.example.com".to_string(),
        audiences: vec!["app-one".to_string()],
        algorithm,
        public_keys: vec![public_key.to_string()],
    }
}

pub fn claims() -> Value {
    json!({
        "iss": "https://issuer.example.com",
        "aud": "app-one",
        "exp": jsonwebtoken::get_current_timestamp() + 3600,
    })
}

pub fn token(algorithm: JwtAlgorithm, claims: &Value) -> String {
    let (algorithm, key) = match algorithm {
        JwtAlgorithm::RS256 => (
            Algorithm::RS256,
            EncodingKey::from_rsa_pem(include_bytes!("rsa-private.pem")),
        ),
        JwtAlgorithm::ES256 => (
            Algorithm::ES256,
            EncodingKey::from_ec_pem(include_bytes!("ec-private.pem")),
        ),
        JwtAlgorithm::EdDSA => (
            Algorithm::EdDSA,
            EncodingKey::from_ed_pem(include_bytes!("ed-private.pem")),
        ),
    };
    encode(&Header::new(algorithm), claims, &key.expect("a test signing key")).expect("a signed test token")
}

pub fn token_signed_with_another_key() -> String {
    let key = EncodingKey::from_ed_pem(include_bytes!("other-ed-private.pem")).expect("another signing key");
    encode(&Header::new(Algorithm::EdDSA), &claims(), &key).expect("a token signed with another key")
}
