//! Shared test-only HTTP mock server.
//!
//! Tests bind mock servers on `127.0.0.1:0`. On developer machines, local
//! tools (editors, port forwarders, hook daemons) probe newly opened
//! listening ports, and each probe used to consume one of a fixture's fixed
//! accept slots, so the real client failed with connection resets or wrong
//! scripted responses. Every mock here is immune to that traffic:
//!
//! - Each server owns a random path prefix (`/t-<uuid>`) and the base URL
//!   exposed to the code under test includes the prefix.
//! - The server keeps accepting connections until every scripted response
//!   has been served to a request whose target starts with the prefix. Any
//!   other request target receives a short 404 with `Connection: close` and
//!   is neither counted nor recorded.
//! - Recorded requests have the prefix stripped from the request line, so
//!   assertions such as `starts_with("GET /v1/models ")` keep working.
//! - Requests are read completely (headers plus Content-Length or chunked
//!   body) before the response is written, so closing the socket does not
//!   discard the response on Windows.
//!
//! [`serve`] covers the scripted shapes: status, content type, extra
//! headers, bodies, redirects, and chunked (one byte per chunk) streaming
//! bodies. [`serve_root`] serves at the origin root for code that replaces
//! any provided path (the Radius gateway rewrites its URL to
//! `/v1/config`), matching only requests whose target starts with a fixed
//! path. Servers that must stay bespoke (websocket or accept-and-hang
//! fixtures) can [`bind`] a prefixed listener and reuse
//! [`target_has_prefix`] for the same filtering.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A short refusal for connections whose request is not addressed to this
/// mock. Never counted or recorded.
const NOT_FOUND: &str = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// How a mock decides which requests it serves.
enum Matching {
    /// Serve only requests under this path prefix; strip it from records.
    StripPrefix(String),
    /// Serve only requests whose target starts with this absolute path.
    Path(String),
}

/// One scripted response, served to the next matching request.
pub(crate) struct MockResponse {
    status: u16,
    content_type: Option<String>,
    headers: Vec<(String, String)>,
    body: String,
    /// Write the body with chunked transfer encoding, one byte per chunk,
    /// to exercise streaming parsers.
    chunked: bool,
}

impl MockResponse {
    /// A JSON response (`Content-Type: application/json`). The body accepts
    /// anything that stringifies, so tests can pass `json!(…)` values
    /// directly.
    pub(crate) fn json(status: u16, body: impl ToString) -> Self {
        Self::text(status, "application/json", body.to_string())
    }

    /// A response with an explicit content type.
    pub(crate) fn text(status: u16, content_type: &str, body: impl ToString) -> Self {
        Self {
            status,
            content_type: Some(content_type.to_string()),
            headers: Vec::new(),
            body: body.to_string(),
            chunked: false,
        }
    }

    /// A bare status line with no content type and no body.
    pub(crate) fn status(status: u16) -> Self {
        Self {
            status,
            content_type: None,
            headers: Vec::new(),
            body: String::new(),
            chunked: false,
        }
    }

    /// A `302 Found` redirection without a body.
    pub(crate) fn redirect(location: impl ToString) -> Self {
        Self::status(302).with_header("Location", location)
    }

    /// Replace the body; useful with [`MockResponse::status`]. The literal
    /// `$BASE_URL` anywhere in a body is replaced with the serving mock's
    /// base URL, so fixture bodies can point back at the same server.
    pub(crate) fn with_body(mut self, body: impl ToString) -> Self {
        self.body = body.to_string();
        self
    }

    /// Add an extra response header.
    pub(crate) fn with_header(mut self, name: &str, value: impl ToString) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// Serve the body with chunked transfer encoding, one byte per chunk.
    pub(crate) fn chunked(mut self) -> Self {
        self.chunked = true;
        self
    }

    fn substitute_base(mut self, base: &str) -> Self {
        self.body = self.body.replace("$BASE_URL", base);
        self
    }

    async fn write_to(self, stream: &mut TcpStream) {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status));
        if let Some(content_type) = &self.content_type {
            head.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if self.chunked {
            head.push_str("Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
            stream.write_all(head.as_bytes()).await.unwrap();
            for byte in self.body.bytes() {
                stream
                    .write_all(format!("1\r\n{}\r\n", char::from(byte)).as_bytes())
                    .await
                    .unwrap();
            }
            stream.write_all(b"0\r\n\r\n").await.unwrap();
        } else {
            head.push_str(&format!(
                "Content-Length: {}\r\nConnection: close\r\n\r\n",
                self.body.len()
            ));
            stream.write_all(head.as_bytes()).await.unwrap();
            stream.write_all(self.body.as_bytes()).await.unwrap();
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        426 => "Upgrade Required",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Response",
    }
}

/// A running mock server that serves scripted responses in order.
pub(crate) struct MockServer {
    /// Base URL including the per-server prefix, for example
    /// `http://127.0.0.1:41234/t-9f2c1d…`. Hand this, or
    /// [`MockServer::url`], to the code under test exactly where the old
    /// `http://{address}` was used.
    pub(crate) base: String,
    requests: tokio::task::JoinHandle<Vec<String>>,
}

impl MockServer {
    /// The base URL joined with `path`, for example `url("/v1/models")`.
    pub(crate) fn url(&self, path: &str) -> String {
        if path.starts_with('/') {
            format!("{}{path}", self.base)
        } else {
            format!("{}/{path}", self.base)
        }
    }

    /// Wait until every scripted response has been served and return the
    /// recorded requests, with the prefix stripped from each request line.
    pub(crate) async fn requests(self) -> Vec<String> {
        self.requests.await.unwrap()
    }
}

/// Serve `responses` in order under a fresh random path prefix.
pub(crate) async fn serve(responses: Vec<MockResponse>) -> MockServer {
    let prefix = format!("/t-{}", uuid::Uuid::now_v7().simple());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}{prefix}");
    spawn_server(listener, base, Matching::StripPrefix(prefix), responses)
}

/// Serve `responses` in order at the origin root, counting only requests
/// whose target starts with `path`. For code that builds its own absolute
/// path from a URL origin instead of appending to the URL it is given.
pub(crate) async fn serve_root(responses: Vec<MockResponse>, path: &str) -> MockServer {
    assert!(
        path.starts_with('/'),
        "the matched target must be an absolute path"
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    spawn_server(listener, base, Matching::Path(path.to_string()), responses)
}

/// Bind a listener and return it with a base URL under a fresh random
/// prefix, for bespoke fixtures (websocket servers and the like) that apply
/// [`target_has_prefix`] themselves.
pub(crate) async fn bind() -> (TcpListener, String, String) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let prefix = format!("/t-{}", uuid::Uuid::now_v7().simple());
    let base = format!("http://{address}{prefix}");
    (listener, base, prefix)
}

fn spawn_server(
    listener: TcpListener,
    base: String,
    matching: Matching,
    responses: Vec<MockResponse>,
) -> MockServer {
    let responses = responses
        .into_iter()
        .map(|response| response.substitute_base(&base))
        .collect::<Vec<_>>();
    let task = tokio::spawn(async move {
        let mut recorded = Vec::new();
        for response in responses {
            serve_response(&listener, &matching, response, &mut recorded).await;
        }
        recorded
    });
    MockServer {
        base,
        requests: task,
    }
}

async fn serve_response(
    listener: &TcpListener,
    matching: &Matching,
    response: MockResponse,
    recorded: &mut Vec<String>,
) {
    loop {
        let (mut stream, _) = listener.accept().await.unwrap();
        let Some(request) = read_request(&mut stream).await else {
            // The peer closed before sending a complete request.
            continue;
        };
        let Some(record) = recorded_request(&request, matching) else {
            // Not addressed to this mock: refuse it and keep the slot.
            let _ = stream.write_all(NOT_FOUND.as_bytes()).await;
            continue;
        };
        recorded.push(record);
        response.write_to(&mut stream).await;
        return;
    }
}

/// The request recorded for a matching connection, or `None` when the
/// request is not addressed to this mock. In [`Matching::StripPrefix`]
/// mode the prefix is removed from the request line (a target that becomes
/// empty is rewritten to `/`), leaving every other byte unchanged.
fn recorded_request(request: &str, matching: &Matching) -> Option<String> {
    let line = request.lines().next()?;
    let (method, rest) = line.split_once(' ')?;
    let (target, version) = rest.split_once(' ')?;
    match matching {
        Matching::StripPrefix(prefix) => {
            let stripped = target.strip_prefix(prefix.as_str())?;
            if !stripped.is_empty() && !stripped.starts_with('/') && !stripped.starts_with('?') {
                return None;
            }
            let target = if stripped.is_empty() || stripped.starts_with('?') {
                format!("/{stripped}")
            } else {
                stripped.to_string()
            };
            let rewritten = format!("{method} {target} {version}");
            Some(request.replacen(line, &rewritten, 1))
        }
        Matching::Path(path) => target
            .starts_with(path.as_str())
            .then(|| request.to_string()),
    }
}

/// Whether the request target starts with `prefix` and ends at a segment
/// boundary. Bespoke fixtures use this for the same filtering the scripted
/// servers apply automatically.
pub(crate) fn target_has_prefix(request: &str, prefix: &str) -> bool {
    let Some(line) = request.lines().next() else {
        return false;
    };
    let Some((_, rest)) = line.split_once(' ') else {
        return false;
    };
    let Some((target, _)) = rest.split_once(' ') else {
        return false;
    };
    target.strip_prefix(prefix).is_some_and(|remaining| {
        remaining.is_empty() || remaining.starts_with('/') || remaining.starts_with('?')
    })
}

/// Read one complete HTTP request (request line, headers, and Content-Length
/// or chunked body) from a mock server socket. Returns `None` when the peer
/// closes before sending a complete request. Reading fully before
/// responding keeps Windows from resetting the connection.
pub(crate) async fn read_request(stream: &mut TcpStream) -> Option<String> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..header_end]).into_owned();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while bytes.len() < header_end + content_length {
        let count = stream.read(&mut chunk).await.ok()?;
        if count == 0 {
            return None;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    let chunked = head.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value.trim().eq_ignore_ascii_case("chunked")
        })
    });
    let body = if chunked {
        loop {
            if let Some(decoded) = decode_chunked_body(&bytes[header_end..]) {
                break decoded;
            }
            let count = stream.read(&mut chunk).await.ok()?;
            if count == 0 {
                return None;
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
    } else {
        bytes[header_end..].to_vec()
    };
    let mut request = bytes[..header_end].to_vec();
    request.extend_from_slice(&body);
    Some(String::from_utf8_lossy(&request).into_owned())
}

/// Decode a complete chunked body (ending in a zero-size chunk) or return
/// `None` while it is still incomplete or malformed.
fn decode_chunked_body(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = Vec::new();
    let mut offset = 0;
    loop {
        let line_end = bytes[offset..]
            .windows(2)
            .position(|part| part == b"\r\n")?
            + offset;
        let size = std::str::from_utf8(&bytes[offset..line_end])
            .ok()?
            .split(';')
            .next()
            .and_then(|size| usize::from_str_radix(size.trim(), 16).ok())?;
        offset = line_end + 2;
        if size == 0 {
            return (bytes.get(offset..offset + 2) == Some(b"\r\n")).then_some(decoded);
        }
        let data_end = offset.checked_add(size)?;
        if bytes.get(data_end..data_end + 2) != Some(b"\r\n") {
            return None;
        }
        decoded.extend_from_slice(bytes.get(offset..data_end)?);
        offset = data_end + 2;
    }
}
