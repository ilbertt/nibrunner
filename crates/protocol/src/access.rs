use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AccessPolicy {
    Public,
    Jwt(JwtAccessPolicy),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JwtAccessPolicy {
    pub token: JwtTokenSource,
    pub issuer: String,
    pub audiences: Vec<String>,
    pub algorithm: JwtAlgorithm,
    /// Supplied PEM public keys or an HTTPS JWKS endpoint.
    pub public_keys: JwtPublicKeys,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged, deny_unknown_fields)]
pub enum JwtPublicKeys {
    Pem(Vec<String>),
    Jwks { url: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct JwtTokenSource {
    pub header: String,
    pub format: JwtTokenFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "lowercase")]
pub enum JwtTokenFormat {
    Raw,
    Bearer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum JwtAlgorithm {
    RS256,
    ES256,
    EdDSA,
}
