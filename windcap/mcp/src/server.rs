//! The resident service: sessions, the bearer gate, and one JSON response per request.
//!
//! This is the layer that exists because the bridge stopped being stdio. A spawned process had a
//! peer by definition — whoever exec'd it — and a listening port has nobody but whoever can route a
//! packet to it. Everything in this file is about making those two sets not equal.
//!
//! Two properties worth stating before the code:
//!
//!   * **the token is re-read from disk on every request.** `userdata/config_user.json` is the only
//!     place it lives, so rotating it in the web UI takes effect on the next call rather than at the
//!     next start. That is also why a *failed* read must deny: the file is rewritten without
//!     atomicity, and a half-written config that parsed as "no token" would otherwise open the port.
//!   * **a response is one JSON document, never an open stream.** Every tool here answers in
//!     milliseconds, and an endpoint that may be reachable from the LAN should not require a proxy
//!     that survives chunked streaming in order to work.

use std::collections::BTreeSet;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::{thread, time};

use serde_json::{json, Value};

use crate::auth;
use crate::axis::Axis;
use crate::http::{self, Request, Response};
use crate::jsonrpc;
use crate::runtime::Runtime;

/// The one path this service answers on, and the one a client is told to configure.
pub const MCP_PATH: &str = "/mcp";
/// Every agent that points at this URL holds one session. The cap keeps a forgotten client or a port
/// scan from accumulating them without bound.
pub const MAX_SESSIONS: usize = 32;

#[derive(Debug)]
pub enum StartError {
    /// The bind was refused before a socket existed, with the reason already phrased for a user.
    Refused(String),
    /// The socket could not be opened.
    Io(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StartError::Refused(m) | StartError::Io(m) => f.write_str(m),
        }
    }
}

/// Everything a request needs in order to be answered, shared between connection threads.
struct Service {
    /// Guarded because the token and the privacy list can change under a running server.
    runtime: Mutex<Runtime>,
    axis: Axis,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    sessions: Mutex<BTreeSet<String>>,
}

/// Open the service and never come back: this blocks on the accept loop until the process is
/// killed. The banner is written to stderr by `serve` itself, before the loop opens, so a
/// supervisor or a terminal sees what was actually bound.
pub fn serve(runtime: Runtime, port_override: Option<i64>, host_override: Option<String>) -> Result<(), StartError> {
    // The one switch, checked before anything else and not overridable by a flag. `--host` and
    // `--port` change *where* a service that was turned on listens; neither is a way to turn it on,
    // because a stray process on a shared machine must not be able to open the port by naming it.
    if !runtime.enabled() {
        return Err(StartError::Refused(
            "enable_mcp_server is false, so this install has not opted in. Set it to true in \
             userdata/config_user.json (the web UI's Settings page writes the same key), then start \
             again. `windmcp doctor` reports what would be bound."
                .to_string(),
        ));
    }
    let host = host_override.unwrap_or_else(|| runtime.host());
    let port = port_override.unwrap_or_else(|| runtime.port());
    let banner = validate_bind(&host, port, &runtime.token(), runtime.auth_required()).map_err(StartError::Refused)?;

    let authority = crate::runtime::format_authority(&host, port);
    let address = authority
        .strip_suffix(&format!(":{port}"))
        .unwrap_or(host.as_str())
        .trim_matches(['[', ']']);
    let bound = address
        .parse::<std::net::IpAddr>()
        .map(|ip| std::net::SocketAddr::new(ip, port as u16))
        .unwrap_or_else(|_| std::net::SocketAddr::from(([127, 0, 0, 1], port as u16)));
    let listener = TcpListener::bind(bound).map_err(|e| {
        StartError::Io(match e.kind() {
            std::io::ErrorKind::AddrInUse => format!("nothing is listening on {authority} because it is already taken: {e}"),
            std::io::ErrorKind::PermissionDenied => format!("{authority} cannot be bound by this user: {e}"),
            _ => format!("cannot bind {authority}: {e}"),
        })
    })?;
    let bound = listener.local_addr().map_err(|e| StartError::Io(e.to_string()))?;
    // The name a client must present in its `Host` header is the one it actually typed, and the
    // allow-list is built from the *configured* host. Binding the wildcard therefore still allows
    // `127.0.0.1`, which is how the same install is reachable both ways.
    let (allowed_hosts, allowed_origins) = auth::allowed_hosts(if bound.ip().is_unspecified() { "0.0.0.0" } else { &host }, port);

    let service = Arc::new(Service {
        runtime: Mutex::new(runtime),
        axis: Axis::measure(),
        allowed_hosts,
        allowed_origins,
        sessions: Mutex::new(BTreeSet::new()),
    });
    eprintln!("windmcp: {banner} (bound {bound})");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let service = Arc::clone(&service);
                // One thread per in-flight request rather than an async runtime: the request count is
                // an AI assistant and its tools, and a thread costs nothing worth comparing to a
                // dependency on tokio in a workspace that does not have one.
                thread::spawn(move || {
                    if let Err(e) = serve_connection(stream, &service) {
                        eprintln!("windmcp: connection: {e}");
                    }
                });
            }
            // A client that vanished mid-accept, or EMFILE. Neither is fatal and neither should stop
            // a service the user turned on; a tight loop on a persistent error would be worse than a
            // short pause, so the two are distinguished.
            Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset) => {}
            Err(e) => {
                eprintln!("windmcp: accept: {e}");
                thread::sleep(time::Duration::from_millis(100));
            }
        }
    }
    Ok(())
}

/// The bind rules, factored out of `serve` so they are testable without opening a socket.
pub fn validate_bind(host: &str, port: i64, token: &str, auth_required: bool) -> Result<String, String> {
    auth::startup_guard(host, port, token, auth_required).map_err(|refused| refused.to_string())
}

fn serve_connection(mut stream: TcpStream, service: &Service) -> std::io::Result<()> {
    let Some(request) = http::read_request(&mut stream)? else { return Ok(()) };
    let response = route(&request, service);
    response.write_to(&mut stream)?;
    stream.flush()
}

/// One request in, one response out. Every rejection happens before a session is looked at.
fn route(request: &Request, service: &Service) -> Response {
    // DNS rebinding guard: a page on an attacker's domain can be resolved to this host, and the
    // browser will send the request with the credentials it believes are same-origin. `Host` is the
    // header that betrays it.
    if let Some(host) = request.header("host") {
        if !auth::host_is_allowed(&service.allowed_hosts, host) {
            return Response::error(403, "host not allowed for this bind");
        }
    }
    if !auth::origin_is_allowed(&service.allowed_origins, request.header("origin")) {
        return Response::error(403, "origin not allowed for this bind");
    }

    if request.method == "OPTIONS" {
        return Response::error(405, "this service speaks POST only");
    }
    if !matches!(request.path.as_str(), MCP_PATH) {
        return Response::error(404, "not found; the MCP endpoint is /mcp");
    }
    // A credential in a query string is a credential in somebody's access log, browser history and
    // `keyserver` output. It is refused outright rather than ignored, so nobody can be told "just put
    // it in the URL" and have it half-work.
    if !request.query.is_empty() {
        let names: Vec<&str> = request
            .query
            .split('&')
            .filter_map(|pair| pair.split_once('=').map(|(name, _)| name))
            .filter(|name| matches!(name.to_ascii_lowercase().as_str(), "token" | "access_token" | "bearer" | "api_key" | "key"))
            .collect();
        if !names.is_empty() {
            return Response::error(400, "a token in the query string is not accepted; send Authorization: Bearer <token>");
        }
    }

    let auth_required = {
        let mut runtime = service.runtime.lock().expect("runtime lock");
        runtime.refresh();
        runtime.auth_required()
    };
    if auth_required {
        let token = {
            let mut runtime = service.runtime.lock().expect("runtime lock");
            runtime.refresh();
            runtime.token()
        };
        // `auth::authenticates` denies on an empty expectation for exactly the reason above.
        if !auth::authenticates(auth::supplied_token(&request.headers), &token) {
            return Response::unauthorized();
        }
    }

    match request.method.as_str() {
        "POST" => post(request, service),
        // The specification's SSE stream. Deliberately not offered: see the module comment.
        "GET" => Response::error(405, "server-initiated streams are not offered; POST one JSON-RPC message per request")
            .with("allow", "POST, DELETE"),
        "DELETE" => terminate(request, service),
        other => Response::error(405, &format!("{other} is not supported")).with("allow", "POST, DELETE"),
    }
}

fn post(request: &Request, service: &Service) -> Response {
    if !request
        .header("accept")
        .map(|accept| accept.contains("application/json") || accept.contains('*'))
        .unwrap_or(true)
    {
        return Response::error(406, "this service answers application/json");
    }
    let parsed: Value = match serde_json::from_slice(&request.body) {
        Ok(value) => value,
        Err(e) => {
            return json_body(jsonrpc::failure_notification(&json!(null), jsonrpc::PARSE_ERROR, &format!("body is not JSON: {e}")))
        }
    };
    // Batched requests were removed from the 2025-06-18 revision. Answering one with a single result
    // would be a lie, so say what happened instead.
    if parsed.is_array() {
        return json_body(jsonrpc::failure_notification(&json!(null), jsonrpc::INVALID_REQUEST, "batched requests are not supported"));
    }

    let is_initialize = parsed.get("method").and_then(Value::as_str) == Some("initialize");
    // A session id that is *presented* has to be one this process issued: an invented one is the
    // signature of a client trying to ride along with somebody else's stream. A request that carries
    // none is answered statelessly, because every one of them is separately bearer-authenticated and
    // the transport this server offers is one-request-one-answer rather than a held session.
    if !is_initialize && request.header("mcp-session-id").is_some_and(|id| !service.session_holds(id)) {
        return Response::error(400, "no such session; send the Mcp-Session-Id from the initialize response, or none at all");
    }

    let response = {
        let mut runtime = service.runtime.lock().expect("runtime lock");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| jsonrpc::handle(&mut runtime, &service.axis, &parsed)));
        drop(runtime);
        outcome
    };
    let answer = match response {
        Ok(Some(value)) => value,
        Ok(None) => return Response { status: 202, reason: "Accepted", headers: Vec::new(), body: Vec::new() },
        Err(_) => jsonrpc::failure_notification(&parsed.get("id").cloned().unwrap_or(Value::Null), jsonrpc::INTERNAL_ERROR, "the handler panicked; the service is still up"),
    };

    if is_initialize {
        match issue_session(service) {
            Some(id) => return json_body(answer).with("mcp-session-id", &id),
            None => return Response::error(429, "too many sessions; this service holds at most {MAX_SESSIONS}"),
        }
    }
    let mut response = json_body(answer);
    if let Some(id) = request.header("mcp-session-id") {
        response = response.with("mcp-session-id", id);
    }
    response
}

/// `initialize` has already been answered, so the session id is this service's own bookkeeping.
fn issue_session(service: &Service) -> Option<String> {
    let id = new_session_id();
    let mut sessions = service.sessions.lock().expect("session lock");
    if sessions.len() >= MAX_SESSIONS {
        // The one that asked is the one we just served; keeping it alive is better than evicting an
        // agent mid-task, so a full table refuses newcomers until an old session expires or leaves.
        return None;
    }
    sessions.insert(id.clone());
    Some(id)
}

fn terminate(request: &Request, service: &Service) -> Response {
    match request.header("mcp-session-id") {
        Some(id) => {
            let existed = service.sessions.lock().expect("session lock").remove(id);
            if existed {
                Response { status: 204, reason: "No Content", headers: Vec::new(), body: Vec::new() }
            } else {
                Response::error(404, "no such session")
            }
        }
        None => Response::error(400, "DELETE needs an Mcp-Session-Id header"),
    }
}

impl Service {
    fn session_holds(&self, presented: &str) -> bool {
        self.sessions.lock().expect("session lock").contains(presented)
    }
}

/// A session id that need not be unguessable — it authorises nothing on its own, the bearer token
/// does — but must not be collidable by a client that is trying to hijack somebody else's stream.
fn new_session_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let clock = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let salt = std::collections::hash_map::RandomState::new().build_hasher().finish();
    format!("{:016x}{:016x}{:x}", salt, clock as u64, std::process::id())
}

fn json_body(value: Value) -> Response {
    Response::json(200, &value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_distinct_and_long() {
        let ids: Vec<String> = (0..64).map(|_| new_session_id()).collect();
        let unique: BTreeSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "session ids collided");
        assert!(ids.iter().all(|id| id.len() >= 32), "a 16-char id is guessable");
    }

    #[test]
    fn the_bind_guard_runs_before_any_socket_exists() {
        assert!(validate_bind("192.168.1.10", 21120, "", false).is_err());
        assert!(validate_bind("127.0.0.1", 0, "a-token-of-sufficient-length-indeed", true).is_err());
        assert!(validate_bind("127.0.0.1", 21120, "a-token-of-sufficient-length-indeed", true).is_ok());
    }

    #[test]
    fn a_refused_bind_names_the_setting_that_is_wrong_and_never_the_secret() {
        let secret = "a-token-of-sufficient-length-indeed";
        let message = validate_bind("127.0.0.1", 21120, "abc", true).unwrap_err();
        assert!(message.contains("mcp_server_token"), "{message}");
        assert!(!message.contains(secret), "the refusal must not echo a token");
    }
}
