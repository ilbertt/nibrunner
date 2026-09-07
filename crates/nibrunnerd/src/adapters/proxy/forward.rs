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
        assert_eq!(
            response.headers().get("cache-control").unwrap(),
            "no-store",
            "a refusal must never be cached against the app"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(std::str::from_utf8(&body).unwrap(), "This app is not running.\n");
    }

    #[test]
    fn a_uri_that_cannot_be_rebuilt_falls_back_to_somewhere_that_answers_nothing() {
        let uri: Uri = "/a".parse().unwrap();
        assert_eq!(
            rewritten(&uri, "not a host", 3000).to_string(),
            "http://127.0.0.1/"
        );
    }

    async fn reading_hostnames() -> u16 {
        let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(|request: Request<Incoming>| async move {
                        let read = hostname_of(&request).unwrap_or_else(|| "(none)".to_string());
                        Ok::<_, std::convert::Infallible>(say(StatusCode::OK, &read))
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        port
    }

    async fn asked(port: u16, request: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut answer = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            stream.read_to_string(&mut answer),
        )
        .await
        .expect("the proxy closes the connection it answered on")
        .unwrap();
        answer.rsplit("\r\n\r\n").next().unwrap_or_default().to_string()
    }

    #[tokio::test]
    async fn the_hostname_a_request_names_is_read_from_the_header_and_stripped_of_its_port() {
        let port = reading_hostnames().await;
        assert_eq!(
            asked(port, "GET / HTTP/1.0\r\nHost: app-1.apps.example.com\r\n\r\n").await,
            "app-1.apps.example.com"
        );
        assert_eq!(
            asked(
                port,
                "GET / HTTP/1.0\r\nHost: APP-1.Apps.Example.Com:8443\r\n\r\n"
            )
            .await,
            "app-1.apps.example.com"
        );
    }

    #[tokio::test]
    async fn a_request_that_names_no_host_at_all_names_no_app() {
        let port = reading_hostnames().await;
        assert_eq!(asked(port, "GET /a/b HTTP/1.0\r\n\r\n").await, "(none)");
    }

    #[tokio::test]
    async fn a_request_that_names_its_host_only_in_the_line_itself_is_still_routed() {
        let port = reading_hostnames().await;
        assert_eq!(
            asked(port, "GET http://App-1.Example.Com:80/a HTTP/1.0\r\n\r\n").await,
            "app-1.example.com"
        );
        assert_eq!(
            asked(
                port,
                "GET http://line.example.com/ HTTP/1.0\r\nHost: header.example.com\r\n\r\n"
            )
            .await,
            "header.example.com",
            "the header is what the client asked for"
        );
    }

    async fn proxying(host: &'static str, port: u16, keep_alive: bool) -> u16 {
        let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let listening_on = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let client = std::sync::Arc::new(upstream_client());
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let client = client.clone();
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                        let client = client.clone();
                        async move {
                            Ok::<_, std::convert::Infallible>(
                                forward(&client, request, host, port, keep_alive).await,
                            )
                        }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        listening_on
    }

    #[tokio::test]
    async fn an_upstream_that_is_not_listening_is_a_bad_gateway_rather_than_a_hang() {
        let listener = tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let nobody = listener.local_addr().unwrap().port();
        drop(listener);
        let proxy = proxying("127.0.0.1", nobody, true).await;
        let answered = asked(proxy, "GET / HTTP/1.0\r\nHost: app-1.example.com\r\n\r\n").await;
        assert_eq!(answered, "This app could not be reached.\n");
    }

    #[tokio::test]
    async fn a_request_reaches_the_upstream_with_its_path_and_without_the_headers_that_do_not_travel() {
        let upstream = reading_hostnames().await;
        let proxy = proxying("127.0.0.1", upstream, true).await;
        let answered = asked(
            proxy,
            "GET /a?b=1 HTTP/1.0\r\nHost: app-1.example.com\r\nupgrade: websocket\r\n\r\n",
        )
        .await;
        assert_eq!(
            answered, "app-1.example.com",
            "the host the client named travels, the hop-by-hop header does not"
        );
    }
}
