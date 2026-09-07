//! Handing one request onwards and its answer back.

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;

pub type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// Headers describing one hop of a connection rather than the message travelling on it. Copying
/// them onto the next hop is how a proxy tells a client about a connection it does not have.
const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn strip_hop_by_hop(headers: &mut hyper::HeaderMap) {
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
}

pub fn upstream_client() -> Client<HttpConnector, Incoming> {
    let mut connector = HttpConnector::new();
    // A guest that has just been restored accepts long before it answers; the connect itself is
    // the fast part, and a request that is already waiting on a wake must not be given up on here.
    connector.set_nodelay(true);
    Client::builder(TokioExecutor::new()).build(connector)
}

fn rewritten(uri: &Uri, host: &str, port: u16) -> Uri {
    let path = uri.path_and_query().map_or("/", |path| path.as_str());
    Uri::builder()
        .scheme("http")
        .authority(format!("{host}:{port}"))
        .path_and_query(path)
        .build()
        .unwrap_or_else(|_| Uri::from_static("http://127.0.0.1/"))
}

/// One request to an address, and its answer back. `keep_alive` is the difference between the
/// edge, which pools upstream connections and should, and the activator, which must not: a
/// connection opened to the activator while the microVM was down goes on being answered by it
/// long after the rule that should have taken it over is in the kernel.
pub async fn forward(
    client: &Client<HttpConnector, Incoming>,
    request: Request<Incoming>,
    host: &str,
    port: u16,
    keep_alive: bool,
) -> Response<ProxyBody> {
    let (mut parts, body) = request.into_parts();
    parts.uri = rewritten(&parts.uri, host, port);
    strip_hop_by_hop(&mut parts.headers);
    let forwarded = Request::from_parts(parts, body);

    match client.request(forwarded).await {
        Ok(upstream) => {
            let (mut parts, body) = upstream.into_parts();
            strip_hop_by_hop(&mut parts.headers);
            if !keep_alive {
                parts.headers.insert(
                    HeaderName::from_static("connection"),
                    HeaderValue::from_static("close"),
                );
            }
            Response::from_parts(parts, body.boxed())
        }
        Err(error) => {
            tracing::warn!(%error, host, port, "an upstream would not answer");
            say(StatusCode::BAD_GATEWAY, "This app could not be reached.\n")
        }
    }
}

/// Plain and short, because a person reads it in a browser with no styling around it.
pub fn say(status: StatusCode, message: &str) -> Response<ProxyBody> {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .header("cache-control", "no-store")
        .header("connection", "close")
        .body(
            Full::new(Bytes::from(message.to_string()))
                .map_err(|never| match never {})
                .boxed(),
        )
        .expect("a constant response is always buildable")
}

/// The hostname a request names, without the port an edge may have carried with it.
pub fn hostname_of(request: &Request<Incoming>) -> Option<String> {
    request
        .headers()
        .get(hyper::header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| request.uri().host())
        .map(|host| host.split(':').next().unwrap_or(host).to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[test]
    fn a_uri_is_rewritten_onto_the_upstream_keeping_its_path_and_query() {
        let uri: Uri = "https://app.example.com/a/b?c=1".parse().unwrap();
        assert_eq!(
            rewritten(&uri, "127.0.0.1", 21000).to_string(),
            "http://127.0.0.1:21000/a/b?c=1"
        );
        let bare: Uri = "/".parse().unwrap();
        assert_eq!(
            rewritten(&bare, "10.201.0.2", 3000).to_string(),
            "http://10.201.0.2:3000/"
        );
    }

    #[test]
    fn hop_by_hop_headers_do_not_travel() {
        let mut headers = hyper::HeaderMap::new();
        headers.insert("connection", HeaderValue::from_static("keep-alive"));
        headers.insert("upgrade", HeaderValue::from_static("websocket"));
        headers.insert("x-real", HeaderValue::from_static("kept"));
        strip_hop_by_hop(&mut headers);
        assert!(headers.get("connection").is_none());
        assert!(headers.get("upgrade").is_none());
        assert_eq!(headers.get("x-real").unwrap(), "kept");
    }

    #[tokio::test]
    async fn a_refusal_reads_as_a_sentence_and_is_not_reusable() {
        let response = say(StatusCode::SERVICE_UNAVAILABLE, "This app is not running.\n");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get("connection").unwrap(), "close");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "This app is not running.\n");
    }
}
