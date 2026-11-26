//! `HttpTransport` against a one-shot loopback HTTP server (no GitHub traffic).

use contrib_scout_core::transport::{ApiRequest, HttpTransport, Transport};
use contrib_scout_core::ScoutError;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};

/// What the server received.
struct Received {
    request_line: String,
    /// Lowercased names.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Serves exactly one request with `response` (raw HTTP/1.1 bytes).
fn serve_once(response: &'static str) -> (String, JoinHandle<Received>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        reader.read_line(&mut request_line).unwrap();
        let mut headers = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let (name, value) = line.split_once(':').unwrap();
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
        let length = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .map_or(0, |(_, value)| value.parse().unwrap());
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let mut stream = stream;
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
        Received {
            request_line: request_line.trim_end().to_owned(),
            headers,
            body,
        }
    });
    (base, handle)
}

fn transport(base: &str) -> HttpTransport {
    HttpTransport::new(Some("test-token".into())).with_api_base(base)
}

#[test]
fn conditional_get_returns_304_as_response_and_sends_github_headers() {
    let (base, server) = serve_once(
        "HTTP/1.1 304 Not Modified\r\nETag: W/\"abc\"\r\nX-RateLimit-Remaining: 4999\r\nContent-Length: 0\r\n\r\n",
    );
    let response = transport(&base)
        .get(&ApiRequest::get("/repos/o/r/issues?state=open").etag(Some("W/\"abc\"".into())))
        .unwrap();
    assert!(response.is_not_modified());
    assert_eq!(response.etag(), Some("W/\"abc\""));
    assert_eq!(response.header_value("x-ratelimit-remaining"), Some("4999"));

    let received = server.join().unwrap();
    assert_eq!(
        received.request_line,
        "GET /repos/o/r/issues?state=open HTTP/1.1"
    );
    assert_eq!(received.header("if-none-match"), Some("W/\"abc\""));
    assert_eq!(
        received.header("accept"),
        Some("application/vnd.github+json")
    );
    assert_eq!(received.header("x-github-api-version"), Some("2022-11-28"));
    assert_eq!(received.header("authorization"), Some("Bearer test-token"));
    assert_eq!(
        received.header("user-agent"),
        Some(concat!("contrib-scout/", env!("CARGO_PKG_VERSION")))
    );
}

#[test]
fn ok_response_keeps_duplicate_headers_link_and_body() {
    let (base, server) = serve_once(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nVary: Accept\r\nVary: Accept-Encoding\r\nLink: <https://api.github.com/x?page=2>; rel=\"next\"\r\nContent-Length: 2\r\n\r\n[]",
    );
    let response = transport(&base)
        .get(&ApiRequest::get("/x").accept("application/vnd.github.raw+json"))
        .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"[]");
    let vary: Vec<_> = response
        .headers
        .iter()
        .filter(|(name, _)| name == "vary")
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(vary, ["Accept", "Accept-Encoding"]);
    assert!(response
        .header_value("Link")
        .unwrap()
        .contains("rel=\"next\""));
    assert_eq!(
        server.join().unwrap().header("accept"),
        Some("application/vnd.github.raw+json")
    );
}

#[test]
fn error_status_and_redirect_are_responses_not_followed() {
    let (base, server) = serve_once(
        "HTTP/1.1 503 Service Unavailable\r\nRetry-After: 7\r\nContent-Length: 4\r\n\r\nbusy",
    );
    let response = transport(&base).get(&ApiRequest::get("/x")).unwrap();
    assert_eq!(
        (response.status, response.body.as_slice()),
        (503, &b"busy"[..])
    );
    assert_eq!(response.header_value("retry-after"), Some("7"));
    server.join().unwrap();

    let (base, server) = serve_once(
        "HTTP/1.1 302 Found\r\nLocation: https://evil.example/\r\nContent-Length: 0\r\n\r\n",
    );
    let response = transport(&base).get(&ApiRequest::get("/x")).unwrap();
    assert_eq!(response.status, 302);
    server.join().unwrap();
}

#[test]
fn graphql_posts_query_and_variables() {
    let (base, server) = serve_once(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"data\":{}}",
    );
    let variables = json!({ "n": 2 });
    let response = transport(&base)
        .graphql("query($n:Int!){ viewer { login } }", &variables)
        .unwrap();
    assert_eq!(response.status, 200);

    let received = server.join().unwrap();
    assert_eq!(received.request_line, "POST /graphql HTTP/1.1");
    assert_eq!(received.header("content-type"), Some("application/json"));
    let body: Value = serde_json::from_slice(&received.body).unwrap();
    assert_eq!(body["query"], "query($n:Int!){ viewer { login } }");
    assert_eq!(body["variables"], variables);
}

#[test]
fn connection_refused_is_network_unavailable() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let error = transport(&base).get(&ApiRequest::get("/x")).unwrap_err();
    assert_eq!(
        ScoutError::find(&error).map(ScoutError::code),
        Some("network_unavailable"),
        "{error:#}"
    );
}

#[test]
fn request_url_only_allows_the_api_origin() {
    let http = HttpTransport::new(None);
    assert_eq!(
        http.request_url("/repos/o/r?page=2").unwrap().as_str(),
        "https://api.github.com/repos/o/r?page=2"
    );
    assert_eq!(
        http.request_url("https://api.github.com/repositories/1/issues?page=3")
            .unwrap()
            .as_str(),
        "https://api.github.com/repositories/1/issues?page=3"
    );
    for bad in [
        "http://api.github.com/x",
        "https://evil.example/x",
        "https://api.github.com.evil.example/x",
        "https://user:pw@api.github.com/x",
        "https://api.github.com:8443/x",
        "https://api.github.com/x#frag",
    ] {
        assert!(http.request_url(bad).is_err(), "{bad}");
    }

    // Plain http is refused for non-loopback bases.
    let insecure = HttpTransport::new(None).with_api_base("http://api.github.com");
    assert!(insecure.request_url("/x").is_err());

    // GHES: absolute URLs must stay under the /api/v3 prefix.
    let ghes = HttpTransport::new(None).with_api_base("https://ghe.example.com/api/v3");
    assert_eq!(
        ghes.request_url("/repos/o/r").unwrap().as_str(),
        "https://ghe.example.com/api/v3/repos/o/r"
    );
    assert!(ghes.request_url("https://ghe.example.com/login").is_err());
}
