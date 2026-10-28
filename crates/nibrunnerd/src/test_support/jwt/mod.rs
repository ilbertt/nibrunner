use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use protocol::{JwtAccessPolicy, JwtAlgorithm, JwtTokenFormat, JwtTokenSource};
use serde_json::{json, Value};

pub struct JwksEndpoint {
    pub url: String,
    pub client: reqwest::Client,
    pub requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    response: std::sync::Arc<std::sync::RwLock<(hyper::StatusCode, String)>>,
    task: tokio::task::JoinHandle<()>,
}

impl JwksEndpoint {
    pub async fn new(document: Value) -> Self {
        use hyper::server::conn::http1;
        use hyper::service::service_fn;
        use hyper_util::rt::TokioIo;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, RwLock};

        let certificates = rustls_pemfile::certs(&mut include_bytes!("jwks-server-cert.pem").as_slice())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let key = rustls_pemfile::private_key(&mut include_bytes!("jwks-server-key.pem").as_slice())
            .unwrap()
            .unwrap();
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certificates, key)
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let url = format!("https://127.0.0.1:{}/keys", listener.local_addr().unwrap().port());
        let response = Arc::new(RwLock::new((hyper::StatusCode::OK, document.to_string())));
        let requests = Arc::new(AtomicUsize::new(0));
        let served = response.clone();
        let count = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                let served = served.clone();
                let count = count.clone();
                tokio::spawn(async move {
                    let Ok(stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let service = service_fn(move |_: hyper::Request<hyper::body::Incoming>| {
                        let (status, body) = served.read().unwrap().clone();
                        count.fetch_add(1, Ordering::SeqCst);
                        async move {
                            let mut response = hyper::Response::builder().status(status);
                            if status.is_redirection() {
                                response = response.header(hyper::header::LOCATION, "/redirect");
                            }
                            let chunks = futures::stream::iter([Ok::<_, std::convert::Infallible>(
                                hyper::body::Frame::data(bytes::Bytes::from(body)),
                            )]);
                            Ok::<_, std::convert::Infallible>(
                                response.body(http_body_util::StreamBody::new(chunks)).unwrap(),
                            )
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        let client = crate::adapters::proxy::jwks::client_builder()
            .no_proxy()
            .tls_certs_only([reqwest::Certificate::from_pem(include_bytes!("jwks-server-cert.pem")).unwrap()])
            .build()
            .unwrap();
        Self {
            url,
            client,
            requests,
            response,
            task,
        }
    }

    pub fn respond(&self, status: hyper::StatusCode, body: String) {
        *self.response.write().unwrap() = (status, body);
    }
}

impl Drop for JwksEndpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

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
        public_keys: protocol::JwtPublicKeys::Pem(vec![public_key.to_string()]),
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
    sign(algorithm, claims, None)
}

pub fn token_with_kid(algorithm: JwtAlgorithm, claims: &Value, kid: &str) -> String {
    sign(algorithm, claims, Some(kid))
}

fn sign(algorithm: JwtAlgorithm, claims: &Value, kid: Option<&str>) -> String {
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
    let mut header = Header::new(algorithm);
    header.kid = kid.map(str::to_owned);
    encode(&header, claims, &key.expect("a test signing key")).expect("a signed test token")
}

pub fn jwk(algorithm: JwtAlgorithm, kid: &str) -> Value {
    let (algorithm, key) = match algorithm {
        JwtAlgorithm::RS256 => (
            Algorithm::RS256,
            jsonwebtoken::DecodingKey::from_rsa_pem(include_bytes!("rsa-public.pem")),
        ),
        JwtAlgorithm::ES256 => (
            Algorithm::ES256,
            jsonwebtoken::DecodingKey::from_ec_pem(include_bytes!("ec-public.pem")),
        ),
        JwtAlgorithm::EdDSA => (
            Algorithm::EdDSA,
            jsonwebtoken::DecodingKey::from_ed_pem(include_bytes!("ed-public.pem")),
        ),
    };
    let mut jwk = jsonwebtoken::jwk::Jwk::from_decoding_key(&key.unwrap(), Some(algorithm)).unwrap();
    jwk.common.key_id = Some(kid.to_string());
    serde_json::to_value(jwk).unwrap()
}

pub fn token_signed_with_another_key() -> String {
    let key = EncodingKey::from_ed_pem(include_bytes!("other-ed-private.pem")).expect("another signing key");
    encode(&Header::new(Algorithm::EdDSA), &claims(), &key).expect("a token signed with another key")
}
