//! A small HTTP/1.1 reader and writer.
//!
//! Not a web server: one request per connection, one response, no keep-alive, no chunked transfer
//! encoding, no TLS. That is the whole shape an MCP client sees from this service, and every one of
//! those omissions is a decision rather than an accident — see `server` for why the response body is
//! always a single JSON document.
//!
//! What is *not* omitted is the bounding. A socket that will accept a body of any size is a socket
//! that will accept a denial of service, and the process sitting on the other end of it asked to read
//! someone's screen history. Both the header block and the body have a hard ceiling, and a request
//! that exceeds either is closed rather than buffered.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// The header block, in bytes. A URL longer than this is not a request anyone meant to send.
pub const MAX_HEADER_BYTES: usize = 16 * 1024;
/// A JSON-RPC call to a read-only bridge is a few hundred bytes; a megabyte is generous and finite.
pub const MAX_BODY_BYTES: u64 = 1024 * 1024;
/// A client that connects and says nothing must not occupy a thread forever either.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct Request {
    pub method: String,
    /// The path without its query string.
    pub path: String,
    /// The query string, kept only so it can be *rejected* as a place to put a credential.
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(have, _)| have == name).map(|(_, value)| value.as_str())
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, body: &str) -> Response {
        Response {
            status,
            reason: reason(status),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                // The bridge hands screen history to a browser-capable client on a localhost port.
                // Nothing here is meant to be cached by anybody but the caller.
                ("cache-control".to_string(), "no-store".to_string()),
            ],
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn error(status: u16, message: &str) -> Response {
        Response::json(status, &serde_json::json!({ "error": message }).to_string())
    }

    /// An unauthenticated refusal that names the scheme, so a client library can pick the right
    /// credential flow instead of guessing from a bare 401.
    pub fn unauthorized() -> Response {
        let mut response = Response::error(401, "unauthorized");
        response.headers.push(("www-authenticate".to_string(), "Bearer realm=\"windrecorder\"".to_string()));
        response
    }

    pub fn with(mut self, name: &str, value: &str) -> Response {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    pub fn write_to(&self, stream: &mut TcpStream) -> std::io::Result<()> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, self.reason);
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!("content-length: {}\r\nconnection: close\r\n\r\n", self.body.len()));
        stream.write_all(head.as_bytes())?;
        stream.write_all(&self.body)?;
        stream.flush()
    }
}

/// Read one request. `Ok(None)` is a clean close with nothing said, which is a port probe and not an
/// error worth logging.
pub fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut buffered = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = find_subarray(&buffered, b"\r\n\r\n") {
            break at + 4;
        }
        if buffered.len() >= MAX_HEADER_BYTES {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "header block too large"));
        }
        match stream.read(&mut chunk)? {
            0 => return Ok(None),
            read => buffered.extend_from_slice(&chunk[..read]),
        }
    };

    let head = std::str::from_utf8(&buffered[..head_end])
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "headers are not UTF-8"))?
        .to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut pieces = request_line.split_whitespace();
    let method = pieces.next().unwrap_or_default().to_ascii_uppercase();
    let target = pieces.next().unwrap_or("");
    let version = pieces.next().unwrap_or("");
    // Exactly three tokens, and the third one is a version. A fourth is a smuggled second request,
    // and a missing version is not something this server will guess at.
    if target.is_empty() || method.is_empty() || !version.starts_with("HTTP/") || pieces.next().is_some() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "malformed request line"));
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.to_string(), String::new()),
    };
    // An absolute-form target (`http://host/mcp`) is legal in a request line and is what a proxy
    // sends; reduce it to its path so the route check below has one thing to compare.
    let path = match path.strip_prefix("http://") {
        // Absolute form: `http://authority/path`, and the authority is everything up to the first
        // slash. Dropping the whole prefix rather than only the scheme is what stops a proxy's
        // request line from routing to a path that begins with somebody's hostname.
        Some(rest) => match rest.find('/') {
            Some(slash) => rest[slash..].to_string(),
            None => "/".to_string(),
        },
        None => path.clone(),
    };

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "malformed header line"));
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }

    let declared = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .unwrap_or(0);
    if declared > MAX_BODY_BYTES {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "request body too large"));
    }
    // A request that claims to carry a body but names no length is a framing attack: there is no
    // other way to know where it ends, and reading until the timeout is how one slow client ties up
    // every thread this process has.
    if declared == 0 && !matches!(method.as_str(), "GET" | "DELETE" | "HEAD") && head.contains("transfer-encoding") {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "chunked request bodies are not accepted"));
    }

    let mut body = buffered[head_end..].to_vec();
    while (body.len() as u64) < declared {
        match stream.read(&mut chunk)? {
            0 => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "body shorter than content-length")),
            read => body.extend_from_slice(&chunk[..read]),
        }
        if body.len() as u64 > declared {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "body longer than content-length"));
        }
    }
    body.truncate(declared as usize);

    Ok(Some(Request { method, path, query, headers, body }))
}

fn find_subarray(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|window| window == needle)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Content Too Large",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Bad Request",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    /// Drive `read_request` through a real loopback socket: the framing rules are about what arrives
    /// in pieces and what never arrives at all, and a `Cursor` can produce neither.
    fn exchange(raw: &[u8]) -> Option<Request> {
        let raw = raw.to_vec();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let writer = std::thread::spawn(move || {
            let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let _ = client.write_all(&raw);
            let _ = client.shutdown(std::net::Shutdown::Write);
        });
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_request(&mut stream).ok().flatten();
        writer.join().ok();
        request
    }

    /// The end-of-headers marker, spelled once so every fixture below reads like a capture.
    const CRLF: &str = "\r\n";

    fn lines(parts: &[&str]) -> String {
        parts.join(CRLF) + CRLF + CRLF
    }

    #[test]
    fn a_plain_post_is_parsed_into_its_parts() {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let mut raw = lines(&[
            "POST /mcp HTTP/1.1",
            "Host: 127.0.0.1:21120",
            "Authorization: Bearer tok",
            &format!("content-length: {}", body.len()),
        ]);
        raw.push_str(body);
        let request = exchange(raw.as_bytes()).expect("parsed");
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/mcp");
        assert_eq!(request.header("authorization"), Some("Bearer tok"));
        assert_eq!(std::str::from_utf8(&request.body).unwrap(), body);
        // Header names are case-folded once, at the boundary, so nothing downstream has to guess.
        assert!(request.headers.iter().all(|(name, _)| name == &name.to_ascii_lowercase()));
    }

    #[test]
    fn a_query_string_is_kept_separate_from_the_path() {
        let request = exchange(lines(&["GET /mcp?token=abc HTTP/1.1", "Host: x"]).as_bytes()).unwrap();
        assert_eq!(request.path, "/mcp");
        assert_eq!(request.query, "token=abc");
    }

    #[test]
    fn an_absolute_form_target_reduces_to_its_path() {
        let request = exchange(lines(&["POST http://127.0.0.1:21120/mcp HTTP/1.1", "Host: x"]).as_bytes()).unwrap();
        assert_eq!(request.path, "/mcp");
    }

    #[test]
    fn a_connection_that_says_nothing_is_a_close_not_an_error() {
        assert!(exchange(b"").is_none(), "a port probe is not a fault worth a log line");
    }

    #[test]
    fn a_header_block_past_the_ceiling_is_refused_rather_than_buffered() {
        let mut raw = format!("GET /mcp HTTP/1.1{}X-Pad: ", CRLF).into_bytes();
        raw.extend(std::iter::repeat(b'a').take(MAX_HEADER_BYTES + 10));
        assert!(exchange(&raw).is_none(), "an oversized header block must not be returned as a request");
    }

    #[test]
    fn an_oversized_declared_body_is_refused() {
        let raw = lines(&["POST /mcp HTTP/1.1", &format!("content-length: {}", MAX_BODY_BYTES + 1)]);
        assert!(exchange(raw.as_bytes()).is_none());
    }

    #[test]
    fn a_body_shorter_than_its_content_length_is_an_error() {
        let raw = lines(&["POST /mcp HTTP/1.1", "content-length: 50"]) + "short";
        assert!(exchange(raw.as_bytes()).is_none(), "a truncated body is not a smaller request");
    }

    #[test]
    fn a_chunked_body_is_not_accepted() {
        let raw = lines(&["POST /mcp HTTP/1.1", "transfer-encoding: chunked"]) + "3\r\n{}\r\n";
        assert!(exchange(raw.as_bytes()).is_none(), "no declared length means no way to know where it ends");
    }

    #[test]
    fn a_response_names_the_scheme_it_refused_and_declares_its_length() {
        let response = Response::error(401, "unauthorized").with("www-authenticate", "Bearer realm=\"windrecorder\"");
        assert_eq!(response.status, 401);
        assert_eq!(response.reason, "Unauthorized");
        assert_eq!(String::from_utf8_lossy(&response.body), r#"{"error":"unauthorized"}"#);
        assert!(response.headers.iter().any(|(n, v)| n == "www-authenticate" && v.contains("Bearer")));
        assert!(response.headers.iter().any(|(n, v)| n == "cache-control" && v == "no-store"));

        // The framing `write_to` produces is the part a client actually parses, so send it for real.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            response.write_to(&mut stream).unwrap();
        });
        let (mut stream, _) = listener.accept().unwrap();
        let mut echoed = Vec::new();
        stream.read_to_end(&mut echoed).unwrap();
        let text = String::from_utf8_lossy(&echoed).replace("\r\n", "\n");
        assert!(text.starts_with("HTTP/1.1 401 Unauthorized\n"), "{text}");
        assert!(text.contains(&format!("content-length: {}\n", r#"{"error":"unauthorized"}"#.len())), "{text}");
        assert!(text.contains("connection: close\n"), "{text}");
        assert!(text.ends_with(r#"{"error":"unauthorized"}"#), "the body must be the last thing on the wire");
    }
}
