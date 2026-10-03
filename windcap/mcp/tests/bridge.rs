//! End-to-end checks against a real `windmcp` process and a real socket.
//!
//! The unit tests prove the handlers. These prove the *service*, which is where every one of the
//! security rules actually lives: that a request without a token is refused before it reaches a
//! session, that the token in a URL is not a substitute for the one in a header, that a bind nobody
//! authorised never opens, and that a JSON-RPC client written from the specification alone can
//! handshake, list the tools and read a real record.
//!
//! Nothing here has been verified against an official MCP client — see the report. The framing is
//! checked against the wire format the specification describes, by a client written from that
//! description, which is the closest this environment can get to the real thing.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wind_mcp::fixture;

const BIN: &str = env!("CARGO_BIN_EXE_windmcp");
const TOKEN: &str = "a-token-of-sufficient-length-indeed-for-the-guard";
const TIMEOUT: Duration = Duration::from_secs(25);

/// A month of history, and the install that points at it.
struct Harness {
    root: PathBuf,
    port: u16,
}

impl Harness {
    /// Seed one month and write a config that turns the service on, loopback, with a token.
    fn seeded(tag: &str, extra: Value) -> Harness {
        let port = free_port();
        let mut settings = json!({
            "enable_mcp_server": true,
            "mcp_server_host": "127.0.0.1",
            "mcp_server_port": port,
            "mcp_server_token": TOKEN,
            "mcp_server_auth_required": true,
        });
        if let Some(object) = extra.as_object() {
            for (key, value) in object {
                settings[key] = value.clone();
            }
        }
        let root = fixture::install(tag, &settings.to_string());
        fixture::month(&root, "default", 2026, 9, &fixture::busy_day());
        Harness { root, port }
    }

    fn config(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.user_config()).unwrap()).unwrap()
    }

    fn user_config(&self) -> PathBuf {
        self.root.join("userdata/config_user.json")
    }

    fn write_config(&self, values: Value) {
        // A different size as well as different contents, so `Runtime::refresh`'s stamp really moves.
        std::fs::write(self.user_config(), format!("{}\n", serde_json::to_string_pretty(&values).unwrap())).unwrap();
    }

}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

/// One line-delimited HTTP/1.0-style exchange: no keep-alive, no chunking, one response, close.
/// Exactly what `wind_mcp::http` emits, and enough to exercise a protocol from the outside.
#[derive(Debug)]
struct Exchanged {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Exchanged {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("body is not JSON ({e}): {}", self.body))
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(have, _)| have == name).map(|(_, value)| value.as_str())
    }
}

fn talk(host: &str, port: u16, request: &str) -> Exchanged {
    let mut stream = TcpStream::connect((host, port)).expect("connect");
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).replace("\r\n", "\n");
    let (head, body) = text.split_once("\n\n").unwrap_or((text.as_str(), ""));
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or_default();
    let status = status_line.split_whitespace().nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
    let headers = lines
        .filter_map(|line| line.split_once(':').map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string())))
        .collect();
    Exchanged { status, headers, body: body.to_string() }
}

fn post(port: u16, body: &str, token: Option<&str>, session: Option<&str>) -> Exchanged {
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(session) = session {
        request.push_str(&format!("Mcp-Session-Id: {session}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    talk("127.0.0.1", port, &request)
}

/// A live server, killed when the guard is dropped.
struct Server {
    child: Child,
    log: PathBuf,
}

impl Server {
    fn start(root: &Path) -> Server {
        Server::start_with(root, &[])
    }

    fn start_with(root: &Path, arguments: &[&str]) -> Server {
        let log = root.join("server.log");
        let file = std::fs::File::create(&log).unwrap();
        let err = std::fs::File::try_clone(&file).unwrap();
        let child = Command::new(BIN)
            .arg("serve")
            .arg("--root")
            .arg(root)
            .args(arguments)
            .stdout(std::process::Stdio::from(file))
            .stderr(std::process::Stdio::from(err))
            .spawn()
            .expect("spawn windmcp");
        Server { child, log }
    }

    /// Wait until the port answers *anything*, which is the only honest readiness signal: the guard
    /// runs before the bind, so a refusal means it never answers and the log says why.
    fn wait_until_listening(&mut self, port: u16) {
        let deadline = Instant::now() + TIMEOUT;
        while Instant::now() < deadline {
            if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
                drop(stream);
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!("windmcp exited with {status} before listening:\n{}", self.log_text());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("windmcp never answered on {port} within {TIMEOUT:?}:\n{}", self.log_text());
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A client written from the specification: JSON-RPC in a POST, the session id in a header.
struct Client {
    port: u16,
    token: String,
    session: Option<String>,
    id: i64,
}

impl Client {
    fn handshake(port: u16, token: &str) -> Client {
        let mut client = Client { port, token: token.to_string(), session: None, id: 0 };
        let response = client.request(
            "initialize",
            &json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "integration", "version": "0"}}),
        );
        assert_eq!(response.status, 200, "handshake: {}", response.body);
        client.session = response.header("mcp-session-id").map(str::to_string);
        assert!(client.session.is_some(), "no Mcp-Session-Id came back: {:?}", response.headers);
        let notice = client.request_raw("notifications/initialized", &json!({}), true);
        assert!(notice.status == 200 || notice.status == 202, "initialized notice: {}", notice.status);
        client
    }

    fn request(&mut self, method: &str, params: &Value) -> Exchanged {
        self.id += 1;
        let body = json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params}).to_string();
        post(self.port, &body, Some(&self.token), self.session.as_deref())
    }

    fn request_raw(&mut self, method: &str, params: &Value, as_notification: bool) -> Exchanged {
        let body = if as_notification {
            json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string()
        } else {
            self.id += 1;
            json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params}).to_string()
        };
        post(self.port, &body, Some(&self.token), self.session.as_deref())
    }

    fn call(&mut self, tool: &str, arguments: &Value) -> Value {
        let response = self.request("tools/call", &json!({"name": tool, "arguments": arguments}));
        assert_eq!(response.status, 200, "{tool} was refused at the transport: {}", response.body);
        let envelope = response.json();
        assert!(envelope.get("error").is_none(), "{tool}: {}", envelope);
        envelope["result"].clone()
    }

    fn structured(&mut self, tool: &str, arguments: &Value) -> Value {
        let result = self.call(tool, arguments);
        assert_eq!(result["isError"], json!(false), "{tool} failed: {}", result["content"]);
        result["structuredContent"].clone()
    }
}

// ---------------------------------------------------------------------------------------------
// the protocol conversation
// ---------------------------------------------------------------------------------------------

#[test]
fn a_raw_json_rpc_client_can_read_the_whole_bridge() {
    let harness = Harness::seeded("protocol", json!(null));
    let mut server = Server::start(&harness.root);
    server.wait_until_listening(harness.port);
    let mut client = Client::handshake(harness.port, TOKEN);

    let initialized = client.request("initialize", &json!({"protocolVersion": "2025-06-18"}));
    let envelope = initialized.json();
    assert_eq!(envelope["result"]["serverInfo"]["name"], json!("windrecorder"));
    assert_eq!(envelope["result"]["protocolVersion"], json!("2025-06-18"));
    assert!(envelope["result"]["capabilities"]["tools"].is_object());
    assert!(envelope["result"]["capabilities"]["resources"].is_object());
    assert!(envelope["result"]["instructions"].as_str().unwrap().to_lowercase().contains("windrecorder"));

    let tools = client.request("tools/list", &json!({})).json();
    let listed = tools["result"]["tools"].as_array().cloned().unwrap_or_default();
    let names: Vec<&str> = listed.iter().filter_map(|tool| tool["name"].as_str()).collect();
    assert_eq!(
        names,
        vec![
            "windrecorder_status",
            "windrecorder_search",
            "windrecorder_around",
            "windrecorder_app_usage",
            "windrecorder_day_summary",
            "windrecorder_frame",
            // The summary surface, added by the AI summary feature: three more reads, and the first two
            // writes this bridge has ever published.
            "windrecorder_summaries_pending",
            "windrecorder_summaries_read",
            "windrecorder_prompts_read",
            "windrecorder_period_summary_write",
            "windrecorder_day_summary_write",
        ],
        "the advertised tool set moved"
    );
    // Nine of the eleven read only. The two writers must say so on the wire, because `readOnlyHint` is
    // the field a host uses to decide whether to ask a human first, and a writer that still advertised
    // itself as read-only would let an agent write without anyone noticing.
    for tool in &listed {
        let name = tool["name"].as_str().unwrap();
        let writes = name.ends_with("_write");
        assert_eq!(tool["annotations"]["readOnlyHint"], json!(!writes), "{name}");
        assert_eq!(tool["annotations"]["destructiveHint"], json!(false), "{name}");
    }
    let search = listed.iter().find(|tool| tool["name"] == "windrecorder_search").unwrap();
    assert_eq!(
        search["inputSchema"]["anyOf"],
        json!([{ "required": ["day"] }, { "required": ["start", "end"] }]),
        "an unbounded scan became possible"
    );
    assert_eq!(search["inputSchema"]["properties"]["limit"]["maximum"], json!(100));

    let resources = client.request("resources/templates/list", &json!({})).json();
    assert_eq!(resources["result"]["resourceTemplates"][0]["uriTemplate"], json!("windrecorder://thumbnail/{timestamp}"));

    // The summary queue, over the socket: the seeded day holds one stretch, nobody has written about
    // it, and the call carries its text — which is the promise the tool exists to keep.
    let queue = client.structured("windrecorder_summaries_pending", &json!({ "day": "2026-09-21" }));
    assert_eq!(queue["counted"]["segments_total"], json!(1), "{queue}");
    assert_eq!(queue["pending"].as_array().unwrap().len(), 1);
    assert_eq!(queue["pending"][0]["segment"], json!("2026-09-21_09-00-00"));
    assert!(queue["pending"][0]["frames_detail"].as_array().unwrap().iter().any(|frame| frame["text"]
        .as_str().unwrap().contains("quarterly forecast sheet")), "the frame text travels with the queue");

    // The daily gate, over the socket, in both directions.
    let refused = client.request("tools/call", &json!({
        "name": "windrecorder_day_summary_write",
        "arguments": { "date": "2026-09-21", "text": "a day, written too early" },
    }));
    assert_eq!(refused.json()["result"]["isError"], json!(true), "the gate must refuse over a socket too");
    assert!(refused.json()["result"]["content"][0]["text"].as_str().unwrap().contains("allow_partial"));
    let wrote = client.structured("windrecorder_period_summary_write", &json!({
        "segment": "2026-09-21_09-00-00.mp4", "text": "the Q3 review, then a chat", "written_by": "bridge-test",
    }));
    assert_eq!(wrote["day_coverage"]["complete"], json!(true), "{wrote}");
    let daily = client.structured("windrecorder_day_summary_write", &json!({ "date": "2026-09-21", "text": "a whole day" }));
    assert_eq!(daily["partial"], json!(false));
    let back = client.structured("windrecorder_summaries_read", &json!({ "day": "2026-09-21" }));
    assert_eq!(back["days"][0]["daily"]["state"], json!("answered"), "{back}");
    assert_eq!(back["days"][0]["daily"]["text"], json!("a whole day"));
    assert_eq!(back["days"][0]["period"]["entries"][0]["written_by"], json!("bridge-test"));

    // The prompts, and the fact that an override on disk changes what this reports.
    let prompts = client.structured("windrecorder_prompts_read", &json!({}));
    assert_eq!(prompts["prompts"].as_array().unwrap().len(), 7);
    std::fs::create_dir_all(harness.root.join("userdata/ai_prompts")).unwrap();
    std::fs::write(harness.root.join("userdata/ai_prompts/daily_summary_user.txt"), "{period_summaries}
").unwrap();
    let prompts = client.structured("windrecorder_prompts_read", &json!({}));
    let edited = prompts["prompts"].as_array().unwrap().iter().find(|entry| entry["name"] == json!("daily_summary_user")).unwrap();
    assert_eq!(edited["origin"], json!("user"), "the settings file and the wire agree");

    // status
    let status = client.structured("windrecorder_status", &json!({}));
    assert_eq!(status["has_data"], json!(true));
    assert!(status["databases"].is_array(), "`databases` is the settled key: {status}");
    assert!(status.get("months").is_none(), "a `months` key would be a regression: {status}");
    assert!(status.get("search_semantics").is_none(), "search_semantics was decided against: {status}");
    assert_eq!(status["databases"].as_array().unwrap().len(), 1);
    assert_eq!(status["total_rows"], json!(fixture::BUSY_DAY_ROWS));

    // search, and the CJK round trip
    let found = client.structured(
        "windrecorder_search",
        &json!({"keywords": "ffmpeg", "start": "2026-09-21 00:00:00", "end": "2026-09-21 23:59:59"}),
    );
    assert_eq!(found["total_matches"], json!(1));
    assert_eq!(found["results"][0]["window_title"], json!("Blender render.blend"), "the title must arrive normalised");
    let cjk = client.structured(
        "windrecorder_search",
        &json!({"keywords": "所有权", "start": "2026-09-21", "end": "2026-09-21 23:59:59"}),
    );
    assert_eq!(cjk["total_matches"], json!(2), "CJK text must survive the HTTP JSON round trip");
    assert!(cjk["results"][0]["text"].as_str().unwrap().contains("所有权"));
    assert_eq!(cjk["results"][0]["time"].as_str().unwrap().chars().last(), Some('0'), "every rendered time ends in an offset");
    assert!(cjk["results"][0]["time"].as_str().unwrap().contains('+'));

    // around, from the timestamp the search handed back
    let moment = found["results"][0]["timestamp"].clone();
    let around = client.structured("windrecorder_around", &json!({"timestamp": moment, "window_seconds": 120}));
    assert_eq!(around["center_timestamp"], moment, "a passed-back timestamp must select the same instant");
    assert!(!around["frames"].as_array().unwrap().is_empty());
    let times: Vec<&str> = around["frames"].as_array().unwrap().iter().map(|f| f["time"].as_str().unwrap()).collect();
    assert!(times.windows(2).all(|pair| pair[0] <= pair[1]), "around must be oldest first: {times:?}");

    // day summary: merged events, no screen text anywhere
    let day = client.structured("windrecorder_day_summary", &json!({"date": "2026-09-21"}));
    assert_eq!(day["date"], json!("2026-09-21"));
    let titles: Vec<&str> = day["events"].as_array().unwrap().iter().map(|e| e["window_title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"Blender render.blend"), "{day}");
    assert!(!titles.iter().any(|t| t.starts_with("(13)")), "an unnormalised title reached a day event: {titles:?}");
    assert!(!titles.iter().any(|t| t.contains("Personal")), "the Edge profile suffix survived: {titles:?}");
    assert_eq!(day["events"].as_str().map_or(false, |s| s.contains("forecast")), false);
    let serialised = day.to_string();
    assert!(!serialised.contains("forecast"), "a day summary must carry no screen text: {serialised}");

    // app usage
    let usage = client.structured("windrecorder_app_usage", &json!({"start": "2026-09-21 00:00:00", "end": "2026-09-21 23:59:59"}));
    assert!(usage["total_counted_seconds"].as_i64().unwrap() > 0);
    assert_eq!(usage["usage"].as_array().unwrap().iter().filter(|u| u["window_title"] == json!("1Password")).count(), 0, "an excluded title leaked");
    assert!(usage["withheld_excluded_titles"].as_i64().unwrap() >= 1);

    // frame, delivered as image content
    let frame_result = client.call("windrecorder_frame", &json!({"timestamp": moment}));
    let image = frame_result["content"].as_array().unwrap().iter().find(|block| block["type"] == json!("image")).cloned().unwrap_or(json!(null));
    assert_eq!(image["mimeType"], json!("image/jpeg"), "a recorded frame is labelled the format stored");
    assert!(image["data"].as_str().unwrap().starts_with("/9j/"), "the image block must carry JPEG bytes: {}", &image["data"].as_str().unwrap()[..16.min(image["data"].as_str().unwrap().len())]);
    assert!(frame_result["structuredContent"]["thumbnail"]["data"].is_null(), "the same bytes must not be paid for twice");
    let resource = frame_result["structuredContent"]["thumbnail"]["resource"].as_str().unwrap().to_string();

    // ...and the same bytes as a resource
    let read = client.request("resources/read", &json!({"uri": resource})).json();
    assert_eq!(read["result"]["contents"][0]["uri"], json!(resource));
    assert_eq!(read["result"]["contents"][0]["mimeType"], json!("image/jpeg"));
    assert_eq!(read["result"]["contents"][0]["blob"], image["data"], "the resource and the tool returned different bytes");

    // a typo is a tool error, not a transport failure
    let bad = client.call("windrecorder_search", &json!({"keywords": "x", "start": "yesterday", "end": "today"}));
    assert_eq!(bad["isError"], json!(true));
    assert!(bad["content"][0]["text"].as_str().unwrap().contains("2026-09-20"), "the error must teach the format");

    // ping, and a clean exit
    assert!(client.request("ping", &json!({})).json().get("result").is_some());
    let closed = post(harness.port, "", Some(TOKEN), client.session.as_deref());
    let _ = closed;
    let delete = format!("DELETE /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nMcp-Session-Id: {}\r\n\r\n", harness.port, client.session.unwrap());
    let gone = talk("127.0.0.1", harness.port, &delete);
    assert_eq!(gone.status, 204, "DELETE should retire a session");
}

// ---------------------------------------------------------------------------------------------
// auth and binding — behaviour, not comments
// ---------------------------------------------------------------------------------------------

#[test]
fn a_request_without_the_token_is_refused_before_it_reaches_a_session() {
    let harness = Harness::seeded("auth", json!(null));
    let mut server = Server::start(&harness.root);
    server.wait_until_listening(harness.port);
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();

    let bare = post(harness.port, &body, None, None);
    assert_eq!(bare.status, 401, "an unauthenticated tools/list got: {}", bare.body);
    assert!(bare.header("www-authenticate").unwrap_or_default().to_lowercase().contains("bearer"), "{:?}", bare.headers);
    assert!(!bare.body.contains("windrecorder_search"), "the refusal leaked the tool list: {}", bare.body);

    let wrong = post(harness.port, &body, Some("not-the-token-but-long-enough-to-be-plausible"), None);
    assert_eq!(wrong.status, 401);
    let near = post(harness.port, &body, Some(&TOKEN[..TOKEN.len() - 2]), None);
    assert_eq!(near.status, 401, "a near-miss token was served");
    let short = post(harness.port, &body, Some(&TOKEN[..8]), None);
    assert_eq!(short.status, 401);
    let schemeless = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: {TOKEN}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len(),
        port = harness.port
    );
    assert_eq!(talk("127.0.0.1", harness.port, &schemeless).status, 401, "a bare value without the scheme is not a credential");
    let basic = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Basic {TOKEN}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len(),
        port = harness.port
    );
    assert_eq!(talk("127.0.0.1", harness.port, &basic).status, 401);

    assert_eq!(post(harness.port, &body, Some(TOKEN), None).status, 200, "the right token was refused");
}

/// A token in a URL is a token in a proxy log, a browser history entry and a `netstat` neighbour.
/// It is refused outright, so "just put it in the query string" can never half-work.
#[test]
fn a_token_anywhere_but_the_header_is_not_a_credential() {
    let harness = Harness::seeded("querystring", json!(null));
    let mut server = Server::start(&harness.root);
    server.wait_until_listening(harness.port);
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();

    for spelling in [
        format!("POST /mcp?token={TOKEN} HTTP/1.1"),
        format!("POST /mcp?access_token={TOKEN}&x=1 HTTP/1.1"),
        format!("POST /mcp?bearer={TOKEN} HTTP/1.1"),
        format!("POST /mcp?key={TOKEN} HTTP/1.1"),
    ] {
        let request = format!(
            "{spelling}\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len(),
            port = harness.port
        );
        let response = talk("127.0.0.1", harness.port, &request);
        assert!(response.status == 400 || response.status == 401, "a query-string token got {}: {}", response.status, response.body);
        assert!(!response.body.contains(TOKEN), "the refusal echoed the secret back: {}", response.body);
        assert!(!response.body.contains("windrecorder_search"), "a query-string token was served: {}", response.body);
    }

    // The argv half: the flag does not exist, and the refusal says where the value does go.
    let output: Output = Command::new(BIN)
        .args(["serve", "--root"])
        .arg(&harness.root)
        .arg("--token")
        .arg("argv-value")
        .output()
        .expect("run windmcp");
    assert!(!output.status.success(), "windmcp accepted a token on the command line");
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(stderr.contains("config_user.json"), "{stderr}");
    assert!(!stderr.contains("argv-value"), "the refusal echoed the value");
    for flag in ["--bearer", "--no-auth", "--insecure"] {
        let refused: Output = Command::new(BIN).args(["serve", "--root"]).arg(&harness.root).arg(flag).arg("x").output().unwrap();
        assert!(!refused.status.success(), "{flag} was accepted");
        assert!(String::from_utf8_lossy(&refused.stderr).to_lowercase().contains("command line"), "{flag} got a generic error");
    }
}

#[test]
fn a_rotated_token_needs_no_restart_and_the_old_one_stops_working() {
    let harness = Harness::seeded("rotate", json!(null));
    let mut server = Server::start(&harness.root);
    server.wait_until_listening(harness.port);
    let mut client = Client::handshake(harness.port, TOKEN);
    assert_eq!(client.request("tools/list", &json!({})).status, 200);

    let rotated = format!("{TOKEN}-rotated-0000000");
    let mut settings = harness.config();
    settings["mcp_server_token"] = json!(rotated);
    harness.write_config(settings);
    // No restart, and no reload signal: the resident server re-reads the file it already read once.
    assert_eq!(post(harness.port, &json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}).to_string(), Some(&rotated), client.session.as_deref()).status, 200, "the rotated token was refused");
    assert_eq!(post(harness.port, &json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}).to_string(), Some(TOKEN), client.session.as_deref()).status, 401, "the superseded token still works");

    // And a config the app is mid-way through rewriting must deny, not open.
    std::fs::write(harness.user_config(), b"{\"mcp_server_token\": ").unwrap();
    assert_eq!(post(harness.port, &json!({"jsonrpc":"2.0","id":4,"method":"tools/list"}).to_string(), Some(&rotated), client.session.as_deref()).status, 401, "a torn config opened the service");
}

#[test]
fn an_invented_session_id_is_not_served() {
    let harness = Harness::seeded("session", json!(null));
    let mut server = Server::start(&harness.root);
    server.wait_until_listening(harness.port);
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();
    let forged = post(harness.port, &body, Some(TOKEN), Some("made-up-session-id"));
    assert!(forged.status >= 400, "an invented session id was served: {}", forged.status);
    assert!(!forged.body.contains("windrecorder_search"));
    // A request with no session id at all is answered statelessly: the bearer token is the
    // credential, and one-request-one-answer needs no session to be correct.
    let none = post(harness.port, &body, Some(TOKEN), None);
    assert_eq!(none.status, 200, "a sessionless request was refused: {}", none.body);
    assert!(none.body.contains("windrecorder_search"));
}

/// The bind rules, each proven by a process that exits rather than by a comment saying it would.
#[test]
fn an_unauthorised_bind_refuses_to_start_before_anything_listens() {
    const USABLE: &str = "long-enough-token-value-here-ok";
    let cases: &[(&str, &str, &str, bool, &str)] = &[
        ("loopback with no token", "127.0.0.1", "", true, "mcp_server_token"),
        ("loopback with a short token", "127.0.0.1", "abc", true, "mcp_server_token"),
        ("a LAN address with no token", "192.168.1.10", "", true, "mcp_server_token"),
        ("every interface with a short token", "0.0.0.0", "abc", true, "mcp_server_token"),
        ("auth off on a LAN address", "192.168.1.10", USABLE, false, "authentication is off"),
        ("auth off on every interface", "0.0.0.0", USABLE, false, "authentication is off"),
    ];
    for (name, host, token, auth_required, expected) in cases {
        let port = free_port();
        let root = fixture::install("bind", &json!({
            "enable_mcp_server": true, "mcp_server_host": host, "mcp_server_port": port,
            "mcp_server_token": token, "mcp_server_auth_required": auth_required,
        }).to_string());
        let output = Command::new(BIN).args(["serve", "--root"]).arg(&root).output().expect("run windmcp");
        assert!(!output.status.success(), "{name}: windmcp started anyway");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("refused to start"), "{name}: {stderr}");
        assert!(stderr.contains(expected), "{name} blamed the wrong setting: {stderr}");
        assert!(!stderr.contains(USABLE) || token.is_empty(), "{name} printed the secret: {stderr}");
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err(), "{name} bound a socket before refusing");
        let _ = std::fs::remove_dir_all(root);
    }
}

#[test]
fn a_port_of_zero_refuses_to_start_rather_than_moving_the_endpoint() {
    let root = fixture::install("port", &json!({
        "enable_mcp_server": true, "mcp_server_host": "127.0.0.1", "mcp_server_port": "not-a-port",
        "mcp_server_token": TOKEN, "mcp_server_auth_required": true,
    }).to_string());
    let output = Command::new(BIN).args(["serve", "--root"]).arg(&root).output().unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("mcp_server_port"), "{stderr}");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_service_stays_off_until_the_one_switch_is_flipped() {
    // Everything else configured, and `enable_mcp_server` simply absent — which is what a config
    // written before this feature existed looks like.
    let port = free_port();
    let root = fixture::install("off", &json!({
        "mcp_server_host": "127.0.0.1", "mcp_server_port": port, "mcp_server_token": TOKEN,
    }).to_string());
    let output = Command::new(BIN).args(["serve", "--root"]).arg(&root).output().unwrap();
    assert!(!output.status.success(), "the bridge listened without being turned on");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("enable_mcp_server"), "{stderr}");
    assert!(TcpStream::connect(("127.0.0.1", port)).is_err(), "a socket opened anyway");

    // A flag is not a way to opt in.
    let bypass = Command::new(BIN).args(["serve", "--root"]).arg(&root).args(["--host", "127.0.0.1"]).arg("--port").arg(&port.to_string()).output().unwrap();
    assert!(!bypass.status.success(), "--host/--port switched the service on");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn authentication_off_still_serves_on_loopback_and_only_there() {
    let port = free_port();
    let root = fixture::install("noauth", &json!({
        "enable_mcp_server": true, "mcp_server_host": "127.0.0.1", "mcp_server_port": port,
        "mcp_server_token": "", "mcp_server_auth_required": false,
    }).to_string());
    fixture::month(&root, "default", 2026, 9, &fixture::busy_day());
    let mut server = Server::start(&root);
    server.wait_until_listening(port);
    let mut client = Client::handshake(port, "");
    client.token = String::new();
    let status = client.structured("windrecorder_status", &json!({}));
    assert_eq!(status["has_data"], json!(true), "an anonymous loopback client was not served");
    assert_eq!(client.session.clone().map(|s| s.len()).unwrap_or(0) > 0, true);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_forged_host_header_is_refused_on_a_loopback_bind() {
    let harness = Harness::seeded("rebind", json!(null));
    let mut server = Server::start(&harness.root);
    server.wait_until_listening(harness.port);
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}).to_string();
    for (host, origin) in [
        ("evil.example", "http://evil.example"),
        ("127.0.0.1:21120", "http://evil.example"),
        ("localhost:9999", "http://evil.example:9999"),
    ] {
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let response = talk("127.0.0.1", harness.port, &request);
        assert_eq!(response.status, 403, "{host} / {origin} was served: {}", response.body);
        assert!(!response.body.contains("windrecorder_search"));
    }
    let honest = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nOrigin: http://localhost:3000\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        harness.port,
        body.len()
    );
    assert_eq!(talk("127.0.0.1", harness.port, &honest).status, 200, "a legitimate loopback origin was blocked");
}

// ---------------------------------------------------------------------------------------------
// the tools, on the fixture data, as plain function calls
// ---------------------------------------------------------------------------------------------

#[test]
fn status_reports_databases_and_no_search_semantics() {
    let harness = Harness::seeded("status-shape", json!(null));
    let (runtime, axis) = runtime(&harness);
    let status = wind_mcp::tools::status(&runtime, &axis);
    assert!(status["databases"].is_array());
    assert!(status.get("search_semantics").is_none());
    assert!(status.get("months").is_none());
    assert_eq!(status["total_rows"], json!(fixture::BUSY_DAY_ROWS));
    assert_eq!(status["databases"][0]["database"], json!("default_2026-09_wind.db"));
    assert_eq!(status["databases"][0]["missing_columns"].as_array().unwrap().len(), 0);
    assert_eq!(status["clock"]["utc_offset_seconds"], json!(axis.utc_offset_seconds));
}

/// The contract's sharpest edge: the two tools that measure time must agree, because they take it
/// from one helper. Reaching `*_in` keeps the comparison on identical bounds.
#[test]
fn app_usage_and_day_summary_credit_the_same_seconds() {
    let harness = Harness::seeded("gap-credit", json!(null));
    let (runtime, axis) = runtime(&harness);
    let (from, to) = (fixture::at("2026-09-21_00-00-00"), fixture::at("2026-09-21_23-59-59"));
    let bounds = wind_mcp::tools::Window { from, to, rule: wind_mcp::tools::Rule::Explicit, day_begin_minutes: runtime.day_begin_minutes() };
    let usage = wind_mcp::tools::app_usage_in(&runtime, &axis, &bounds, 100).unwrap();
    let day = wind_mcp::tools::day_summary(&runtime, &axis, &json!({"date": "2026-09-21"})).unwrap();

    assert_eq!(
        usage["total_counted_seconds"], day["total_counted_seconds"],
        "the two tools disagreed about how many seconds the same frames are worth"
    );
    let listed: i64 = usage["usage"].as_array().unwrap().iter().map(|u| u["seconds"].as_i64().unwrap()).sum();
    let events: i64 = day["events"].as_array().unwrap().iter().map(|e| e["seconds"].as_i64().unwrap()).sum();
    assert_eq!(listed, events, "per-title seconds and per-event seconds diverged");

    // The 100-second clip, on data built to cross it: the 09:01:30 -> 09:08:30 silence is 420 s of
    // clock and must be worth exactly the cap.
    let edge = usage["usage"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["window_title"] == json!("ChatGPT - Microsoft Edge"))
        .expect("the Edge title must survive normalisation into one bucket");
    assert!(edge["seconds"].as_i64().unwrap() <= 2 * 100 + 30, "the gap cap was not applied: {edge}");
    assert!(usage["total_counted_seconds"].as_i64().unwrap() < (to - from), "an entire day cannot be counted as work");

    // Shares are of counted time, not of the clock range.
    let sum: f64 = usage["usage"].as_array().unwrap().iter().map(|u| u["share_of_counted_time"].as_f64().unwrap()).sum();
    assert!((sum - 1.0).abs() < 0.02, "shares should add up over the titles shown, got {sum}");
}

#[test]
fn every_title_that_comes_back_has_been_normalised_once() {
    let harness = Harness::seeded("titles", json!(null));
    let (runtime, axis) = runtime(&harness);
    let day = wind_mcp::tools::day_summary(&runtime, &axis, &json!({"date": "2026-09-21"})).unwrap();
    let titles: Vec<&str> = day["events"].as_array().unwrap().iter().map(|e| e["window_title"].as_str().unwrap()).collect();
    assert!(titles.contains(&"大懒趴俱乐部"), "telegram unread counts survived: {titles:?}");
    assert!(titles.contains(&"Blender render.blend"), "the unsaved-document asterisk survived: {titles:?}");
    assert!(titles.contains(&"Q3  review - Excel"), "the badge survived: {titles:?}");
    assert!(titles.iter().all(|t| !t.contains('(') || t.contains("(2026)")), "{titles:?}");
    // A title that is nothing but a badge is no title, and must not appear as one.
    assert!(!titles.contains(&"(42)"), "an empty-after-cleaning title was bucketed anyway: {titles:?}");
    // And it is the *same* string in the usage table, which is the whole point of one shared rule.
    let usage = wind_mcp::tools::app_usage(&runtime, &axis, &json!({"start": "2026-09-21", "end": "2026-09-21 23:59:59", "limit": 100})).unwrap();
    let usage_titles: Vec<&str> = usage["usage"].as_array().unwrap().iter().map(|u| u["window_title"].as_str().unwrap()).collect();
    for title in &titles {
        assert!(usage_titles.contains(title), "{title} is in the day but not the usage table: {usage_titles:?}");
    }
    let search = wind_mcp::tools::search(&runtime, &axis, &json!({"keywords": "Blender", "start": "2026-09-21", "end": "2026-09-21 23:59:59"})).unwrap();
    assert_eq!(search["results"][0]["window_title"], json!("Blender render.blend"));
}

#[test]
fn an_out_of_range_or_empty_query_is_an_empty_answer_not_an_error() {
    let harness = Harness::seeded("empty", json!(null));
    let (runtime, axis) = runtime(&harness);

    // A month with no file at all: `months_covering` selects nothing, so the shape must still be right.
    let far = wind_mcp::tools::search(&runtime, &axis, &json!({"keywords": "anything", "start": "2019-01-01", "end": "2019-01-02"})).unwrap();
    assert_eq!(far["total_matches"], json!(0));
    assert_eq!(far["results"].as_array().unwrap().len(), 0);
    assert_eq!(far["has_more"], json!(false));
    assert!(far["range"]["start"].is_string());

    let no_words = wind_mcp::tools::search(&runtime, &axis, &json!({"start": "2026-09-21", "end": "2026-09-21 23:59:59"})).unwrap();
    assert_eq!(no_words["total_matches"], json!(fixture::BUSY_DAY_ROWS), "an empty keyword list should list the range");

    let nothing = wind_mcp::tools::around(&runtime, &axis, &json!({"timestamp": fixture::at("2026-09-21_00-00-00"), "window_seconds": 5})).unwrap();
    assert_eq!(nothing["frames"].as_array().unwrap().len(), 0);

    let empty_day = wind_mcp::tools::day_summary(&runtime, &axis, &json!({"date": "2020-02-02"})).unwrap();
    assert_eq!(empty_day["total_events"], json!(0));
    assert_eq!(empty_day["events"].as_array().unwrap().len(), 0);
    assert_eq!(empty_day["total_counted_seconds"], json!(0));
    assert_eq!(empty_day["date"], json!("2020-02-02"));

    let empty_usage = wind_mcp::tools::app_usage(&runtime, &axis, &json!({"start": "2020-02-02", "end": "2020-02-03"})).unwrap();
    assert_eq!(empty_usage["usage"].as_array().unwrap().len(), 0);
    assert_eq!(empty_usage["total_counted_seconds"], json!(0));
    assert_eq!(empty_usage["distinct_titles"], json!(0));
    // A page past the end is empty, not an error, and not a 500.
    let deep = wind_mcp::tools::search(&runtime, &axis, &json!({"start": "2026-09-21", "end": "2026-09-21 23:59:59", "offset": 500})).unwrap();
    assert_eq!(deep["results"].as_array().unwrap().len(), 0);
    assert_eq!(deep["has_more"], json!(false));
}

#[test]
fn a_bad_argument_is_a_rejection_that_names_the_shape_it_wanted() {
    let harness = Harness::seeded("reject", json!(null));
    let (runtime, axis) = runtime(&harness);
    let cases: Vec<(&str, Value)> = vec![
        ("windrecorder_search", json!({"start": "yesterday", "end": "today"})),
        ("windrecorder_search", json!({"keywords": "x"})),
        ("windrecorder_search", json!({"start": "2026-09-21", "end": "2026-09-22", "limit": 0})),
        ("windrecorder_search", json!({"start": "2026-09-21", "end": "2026-09-22", "limit": 500})),
        ("windrecorder_search", json!({"start": "2026-09-21", "end": "2026-09-22", "limit": "many"})),
        ("windrecorder_around", json!({"window_seconds": 10})),
        ("windrecorder_day_summary", json!({})),
        ("windrecorder_frame", json!({"timestamp": "not a time"})),
        // A bare integer is a *stored* timestamp, so 1 and 2 are legal bounds and the rejection has
        // to come from the field that is not a number at all.
        ("windrecorder_search", json!({"start": "2026-09-21", "end": "2026-09-22", "offset": -1})),
        ("windrecorder_around", json!({"timestamp": true})),
        ("windrecorder_day_summary", json!({"date": ["2026-09-21"]})),
    ];
    for (name, arguments) in cases {
        let error = wind_mcp::tools::call(&runtime, &axis, name, &arguments).unwrap_err().to_string();
        assert!(!error.is_empty(), "{name} accepted {arguments}");
        if name == "windrecorder_frame" || arguments.get("timestamp").is_some() {
            assert!(error.contains("2026-09-20"), "{name} with {arguments} rejected as: {error}");
        }
    }
    // An unknown tool is refused by the dispatcher, and told what exists.
    let unknown = wind_mcp::tools::call(&runtime, &axis, "windrecorder_delete_everything", &json!({})).unwrap_err().to_string();
    assert!(unknown.contains("unknown tool") && unknown.contains("windrecorder_status"), "{unknown}");
}

/// An argument the tool never advertised must not buy a silent default.
///
/// `window_second` — one letter short of `window_seconds` — used to answer with the 120-second
/// default window, `isError: false`, and a `range` that only a reader who looked would notice was
/// not the one asked for. The agent that typed it believes it got a ten-minute look. Every schema in
/// `jsonrpc::tool_list` already declared `"additionalProperties": false`; this test is what makes
/// that sentence true, and it is checked against those same published schemas rather than a second
/// list that could drift from them.
#[test]
fn an_argument_no_tool_ever_published_is_refused_and_the_nearest_name_is_offered() {
    let harness = Harness::seeded("strictargs", json!(null));
    let (runtime, axis) = runtime(&harness);
    let moment = fixture::at("2026-09-21_09-01-00");

    let typo = wind_mcp::tools::call(&runtime, &axis, "windrecorder_around", &json!({"timestamp": moment, "window_second": 600}))
        .unwrap_err()
        .to_string();
    assert!(typo.contains("window_second"), "{typo}");
    assert!(typo.contains("window_seconds"), "{typo} names the typo but not the spelling it meant");
    // The honest spelling does answer a different window, which is the whole cost of the miss.
    let asked = wind_mcp::tools::call(&runtime, &axis, "windrecorder_around", &json!({"timestamp": moment, "window_seconds": 600})).unwrap();
    assert_eq!(asked["range"]["seconds"], json!(1201), "{asked}");
    let defaulted = wind_mcp::tools::call(&runtime, &axis, "windrecorder_around", &json!({"timestamp": moment})).unwrap();
    assert_eq!(defaulted["range"]["seconds"], json!(241), "{defaulted}");

    // `status` publishes no arguments at all, so anything on it is a mistake about the tool.
    let idle = wind_mcp::tools::call(&runtime, &axis, "windrecorder_status", &json!({"refresh": true})).unwrap_err().to_string();
    assert!(idle.contains("refresh") && idle.contains("takes no arguments"), "{idle}");

    // A name from a sibling tool is refused here too, and says which tools do take it.
    let crossed = wind_mcp::tools::call(&runtime, &axis, "windrecorder_frame", &json!({"timestamp": moment, "date": "2026-09-21"}))
        .unwrap_err()
        .to_string();
    assert!(crossed.contains("date"), "{crossed}");

    // Every key every schema publishes must still pass, or this check would be a refusal machine.
    for tool in wind_mcp::jsonrpc::tool_list(&axis)["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        let keys: Vec<String> = tool["inputSchema"]["properties"].as_object().unwrap().keys().cloned().collect();
        let mut given = json!({});
        for key in &keys {
            given[key.as_str()] = match key.as_str() {
                "timestamp" | "window_seconds" | "limit" | "offset" | "max_text_chars" => json!(1),
                "day" | "date" => json!("2026-09-21"),
                "start" => json!("2026-09-21"),
                "end" => json!("2026-09-22"),
                _ => json!("x"),
            };
        }
        let outcome = wind_mcp::tools::call(&runtime, &axis, name, &given);
        if let Err(rejected) = outcome {
            let message = rejected.to_string();
            assert!(!message.contains("not an argument"), "{name} refused its own published key {keys:?}: {message}");
            assert!(!message.contains("unknown tool"), "{name} is a tool: {message}");
        }
    }
}

/// A day given as a number is a caller that meant a calendar date, not 1970.
///
/// `start`, `end` and `timestamp` take a bare integer because that is how a stored timestamp comes
/// back and goes again — see the note in the rejection table above. `day` and `date` are a different
/// promise: one whole product day named by its calendar date. Coercing `20260921` through the
/// timestamp path used to answer with `1970-08-22`, printed in a `range` that looked official. This
/// is [`a_malformed_day_is_refused_rather_than_widened_to_a_guess`] for the one shape that test could
/// not reach from a shell flag.
#[test]
fn a_day_given_as_a_number_is_refused_rather_than_read_as_a_timestamp() {
    let harness = Harness::seeded("numericday", json!(null));
    let (runtime, axis) = runtime(&harness);
    for (tool, arguments) in [
        ("windrecorder_search", json!({"day": 20_260_921})),
        ("windrecorder_app_usage", json!({"day": 20_260_921_i64})),
        ("windrecorder_day_summary", json!({"date": 20_260_921_i64})),
    ] {
        let error = wind_mcp::tools::call(&runtime, &axis, tool, &arguments).unwrap_err().to_string();
        assert!(error.contains("2026-09-21"), "{tool} took {arguments} and refused without the shape: {error}");
        assert!(!error.contains("1970"), "{tool} answered in 1970: {error}");
    }
    // The same day spelled as a date still resolves through the day rule, not a literal bound.
    let ok = wind_mcp::tools::call(&runtime, &axis, "windrecorder_search", &json!({"day": "2026-09-21"})).unwrap();
    assert_eq!(ok["range"]["rule"], json!("day"), "{ok}");
    // And an integer is still legal where it is a real timestamp.
    let moment = fixture::at("2026-09-21_09-01-00");
    assert!(wind_mcp::tools::call(&runtime, &axis, "windrecorder_around", &json!({"timestamp": moment})).is_ok(), "a stored timestamp must stay passable");
}

#[test]
fn the_thumbnail_resource_and_the_frame_tool_return_the_same_bytes() {
    let harness = Harness::seeded("thumbnail", json!(null));
    let (runtime, axis) = runtime(&harness);
    let moment = fixture::at("2026-09-21_09-01-00");
    let value = wind_mcp::tools::frame_at(&runtime, &axis, moment, 1).expect("a frame at that instant");
    assert_eq!(value["thumbnail"]["mime_type"], json!("image/jpeg"));
    assert!(value["thumbnail"]["bytes"].as_i64().unwrap() > 100);
    assert_eq!(value["thumbnail"]["resource"], json!(wind_mcp::tools::thumbnail_uri(moment)));

    let (bytes, mime, _) = wind_mcp::tools::read_thumbnail(&runtime, &axis, &wind_mcp::tools::thumbnail_uri(moment)).unwrap();
    assert_eq!(mime, "image/jpeg");
    assert_eq!(&bytes[..3], &[0xff, 0xd8, 0xff], "the JPEG magic is what the label claims");

    // Out of range, and a frame stored without a preview, are both a refusal with a message.
    assert!(wind_mcp::tools::read_thumbnail(&runtime, &axis, "windrecorder://thumbnail/1").is_err());
    assert!(wind_mcp::tools::read_thumbnail(&runtime, &axis, "windrecorder://not-a-number").is_err());
    assert!(wind_mcp::tools::read_thumbnail(&runtime, &axis, "http://example.com/x").is_err());

    let blank_root = fixture::install("blankthumb", "{}");
    fixture::month(&blank_root, "default", 2026, 9, &fixture::busy_day_without_thumbnails());
    let blank = wind_mcp::Runtime::open(&blank_root).unwrap();
    let err = wind_mcp::tools::read_thumbnail(&blank, &axis, &wind_mcp::tools::thumbnail_uri(moment)).unwrap_err().to_string();
    assert!(err.contains("no stored thumbnail"), "{err}");
    let _ = std::fs::remove_dir_all(blank_root);
}

#[test]
fn a_frame_knows_where_its_real_image_and_its_video_are() {
    let harness = Harness::seeded("paths", json!(null));
    let picture = format!("{}.jpg", fixture::SEGMENT);
    let on_disk = fixture::slice(&harness.root, fixture::SEGMENT, &picture);
    let video = fixture::segment(&harness.root, 2026, 9, fixture::SEGMENT);
    let (runtime, axis) = runtime(&harness);
    let value = wind_mcp::tools::frame_at(&runtime, &axis, fixture::at(fixture::SEGMENT), 1).expect("the seeded frame");
    assert_eq!(value["offset_in_segment"], json!(0), "a frame at its segment's own start is zero seconds in");
    assert_eq!(std::path::Path::new(value["frame_path"].as_str().unwrap()), on_disk.as_path(), "the frame resolved somewhere else");
    assert_eq!(std::path::Path::new(value["video_path"].as_str().unwrap()), video.as_path(), "the segment resolved by name, not by prefix");

    // A frame 90 seconds into that same recording carries the number a player has to seek to,
    // which is the row's timestamp minus its *segment's* stamp, not minus its own.
    let later = wind_mcp::tools::frame_at(&runtime, &axis, fixture::at("2026-09-21_09-01-30"), 1).unwrap();
    assert_eq!(later["offset_in_segment"], json!(90), "{later}");
    assert_eq!(later["video_file"], json!(format!("{}.mp4", fixture::SEGMENT)));
    assert_eq!(std::path::Path::new(later["video_path"].as_str().unwrap()), video.as_path());
    assert_eq!(later["url"], json!("https://chat.example/thread"));
    assert_eq!(later["window_title"], json!("ChatGPT - Microsoft Edge"));
}

#[test]
fn the_cli_and_the_tools_agree_byte_for_byte() {
    // `windmcp status --json` is the same function the MCP tool calls, and the test that says so has
    // to compare the two rather than assert it in a comment.
    let harness = Harness::seeded("cli-parity", json!(null));
    let (runtime, axis) = runtime(&harness);
    let direct = wind_mcp::tools::status(&runtime, &axis);
    let output = Command::new(BIN).args(["status", "--root"]).arg(&harness.root).arg("--json").output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let from_cli: Value = serde_json::from_slice(&output.stdout).expect("windmcp status --json should be JSON");
    for key in ["databases", "total_rows", "user_name", "has_data", "clock"] {
        assert_eq!(from_cli[key], direct[key], "{key} differed between the CLI and the tool");
    }
    assert!(from_cli.get("search_semantics").is_none());
    assert!(from_cli.get("months").is_none());

    let searched = Command::new(BIN)
        .args(["search", "ffmpeg", "--day", "2026-09-21", "--json"])
        .arg("--root")
        .arg(&harness.root)
        .output()
        .unwrap();
    assert!(searched.status.success(), "{}", String::from_utf8_lossy(&searched.stderr));
    let value: Value = serde_json::from_slice(&searched.stdout).unwrap();
    assert_eq!(value["results"][0]["window_title"], json!("Blender render.blend"));
    assert_eq!(value["total_matches"], json!(1));

    let text = Command::new(BIN).args(["day-summary", "2026-09-21", "--root"]).arg(&harness.root).output().unwrap();
    let rendered = String::from_utf8_lossy(&text.stdout).to_string();
    assert!(rendered.contains("2026-09-21"), "{rendered}");
    assert!(rendered.contains("day-summary:"), "no measured time in the report: {rendered}");
}

#[test]
fn doctor_reports_the_bind_and_never_the_secret() {
    let harness = Harness::seeded("doctor", json!(null));
    let output = Command::new(BIN).args(["doctor", "--root"]).arg(&harness.root).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
    assert!(text.contains(&format!("127.0.0.1:{}", harness.port)), "the bind address is missing:\n{text}");
    assert!(text.contains("yes (enable_mcp_server)"), "{text}");
    let token_line = text.lines().find(|line| line.starts_with("token:")).unwrap_or_default();
    assert!(token_line.contains("configured") && !token_line.contains("not configured"), "{text}");
    assert!(text.contains(&format!("{} row", fixture::BUSY_DAY_ROWS)), "the row count is missing:\n{text}");
    assert!(text.contains("http://127.0.0.1:") && text.contains("/mcp"), "no client URL:\n{text}");
    assert!(text.contains("naive-local"), "the axis is not stated:\n{text}");
    assert!(!text.contains(&harness.config()["mcp_server_token"].as_str().unwrap().to_lowercase()), "doctor printed the token");
}

// ---------------------------------------------------------------------------------------------
// one `--day`, one window, everywhere
// ---------------------------------------------------------------------------------------------
//
// The defect these four tests exist for was not arithmetic — `clock::day_bounds` had always been
// right — it was a command doing its own day sum on the way to that arithmetic. So none of them
// calls a day helper. Every one drives the built binary from argv and reads back what it printed,
// which is the only shape of test that fails when a command quietly stops asking the shared helper.

/// Four frames, one in each hour that matters: before the boundary, inside the band a calendar day
/// would steal, on the last second of the product day, and well inside the next one. With the
/// shipped 03:00 start the first three are the 21st's and only the last is the 22nd's; under a
/// calendar day that split is exactly reversed, which is what makes these counts hard to get right
/// by accident.
fn boundary_rows() -> Vec<fixture::Row> {
    vec![
        fixture::Row::new("2026-09-21_22-00-00", "monday evening sheet", Some("Excel - quarterly forecast")).segment("2026-09-21_09-00-00"),
        fixture::Row::new("2026-09-22_00-30-00", "the small hours", Some("VLC media player")).segment("2026-09-21_09-00-00"),
        fixture::Row::new("2026-09-22_02-59-59", "still the small hours", Some("VLC media player")).segment("2026-09-21_09-00-00"),
        fixture::Row::new("2026-09-22_09-00-00", "tuesday morning", Some("Blender render.blend")).segment("2026-09-21_09-00-00"),
    ]
}

/// An install holding exactly those frames, with the day start set to whatever the test is probing.
///
/// The service is left off: these tests drive the terminal front door, and a fixture that cannot
/// bind a port is a fixture that cannot collide with another test's.
fn day_install(tag: &str, day_begin_minutes: i64) -> PathBuf {
    let root = fixture::install(tag, &json!({ "day_begin_minutes": day_begin_minutes }).to_string());
    fixture::month(&root, "default", 2026, 9, &boundary_rows());
    root
}

/// One `windmcp` invocation, its stdout, and a failure message that carries argv plus stderr.
fn stdout_of(words: &[&str], root: &Path) -> String {
    let output = Command::new(BIN).args(words).args(["--root"]).arg(root).output().unwrap();
    assert!(output.status.success(), "`windmcp {}` failed:\n{}", words.join(" "), String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn payload_of(words: &[&str], root: &Path) -> Value {
    let mut words = words.to_vec();
    words.push("--json");
    serde_json::from_str(&stdout_of(&words, root)).unwrap_or_else(|e| panic!("`windmcp {}` printed no JSON: {e}", words.join(" ")))
}

/// The regression itself: a command that stops calling `clock::day_bounds` changes these numbers.
#[test]
fn a_day_is_the_product_day_for_every_command_that_takes_one() {
    let root = day_install("day-window", 180);
    let (from, to) = wind_base::clock::day_bounds(2026, 9, 22, 180);
    for (label, words) in [
        ("search", vec!["search", "--day", "2026-09-22"]),
        ("app-usage", vec!["app-usage", "--day", "2026-09-22"]),
        ("day-summary", vec!["day-summary", "2026-09-22"]),
    ] {
        let value = payload_of(&words, &root);
        assert_eq!(value["range"]["from"].as_i64(), Some(from), "{label} did not start the day where the app does");
        assert_eq!(value["range"]["to"].as_i64(), Some(to), "{label} did not end the day where the app does");
        assert_eq!(value["range"]["rule"], json!("day"), "{label} did not say which rule made its window");
        assert_eq!(value["range"]["day_begin"], json!("03:00"), "{label} did not name the day start it used");
    }
    // The frames themselves, split the way the web UI splits them and not the way a calendar does.
    assert_eq!(payload_of(&["search", "--day", "2026-09-21"], &root)["total_matches"], json!(3), "the 21st owns the small hours");
    assert_eq!(payload_of(&["search", "--day", "2026-09-22"], &root)["total_matches"], json!(1), "the 22nd must not inherit them");
    let usage = payload_of(&["app-usage", "--day", "2026-09-21"], &root);
    assert_eq!(usage["titled_frames"], json!(3), "app-usage counted a different frames than search did");
    assert_eq!(payload_of(&["app-usage", "--day", "2026-09-22"], &root)["titled_frames"], json!(1), "app-usage's 22nd is not search's 22nd");
    let summary = payload_of(&["day-summary", "2026-09-21"], &root);
    assert_eq!(summary["frames"], json!(3), "day-summary counted a different frames than search did");
    assert_eq!(summary["date"], json!("2026-09-21"), "a summary labelled a day its window is not");
    assert_eq!(payload_of(&["day-summary", "2026-09-22"], &root)["date"], json!("2026-09-22"), "the label rolled off the named day");
    let _ = std::fs::remove_dir_all(root);
}

/// The sentence this whole change is for, measured on the built binaries rather than in a function.
#[test]
fn two_commands_given_the_same_day_resolve_to_identical_bounds() {
    let root = day_install("day-parity", 180);
    let search = payload_of(&["search", "--day", "2026-09-22"], &root);
    let usage = payload_of(&["app-usage", "--day", "2026-09-22"], &root);
    let summary = payload_of(&["day-summary", "2026-09-22"], &root);
    assert_eq!(search["range"], usage["range"], "`--day` meant different windows to search and to app-usage");
    assert_eq!(usage["range"], summary["range"], "`--day` meant different windows to app-usage and to day-summary");
    assert_eq!(search["range"]["start"], json!("2026-09-22T03:00:00+08:00"), "the shared window is not the product day: {}", search["range"]);
    assert_eq!(search["range"]["end"], json!("2026-09-23T02:59:59+08:00"), "{}", search["range"]);
    let _ = std::fs::remove_dir_all(root);
}

/// The compatibility case for every existing install that never touched the setting.
#[test]
fn a_day_start_of_midnight_leaves_the_window_exactly_where_the_calendar_day_had_it() {
    let root = day_install("day-midnight", 0);
    let day = payload_of(&["search", "--day", "2026-09-22"], &root);
    // Byte-for-byte the window the old `--day` built by string formatting.
    assert_eq!(day["range"]["start"], json!("2026-09-22T00:00:00+08:00"), "{}", day["range"]);
    assert_eq!(day["range"]["end"], json!("2026-09-22T23:59:59+08:00"), "{}", day["range"]);
    assert_eq!(day["range"]["seconds"], json!(86_400), "a day is still exactly a day");
    assert_eq!(day["range"]["day_begin"], json!("00:00"), "{}", day["range"]);
    // ...and identical to what the same install answers for the explicit bounds a user would have
    // typed before, which is the whole claim: nothing moved for anyone whose day starts at zero.
    let explicit = payload_of(&["search", "--from", "2026-09-22 00:00:00", "--to", "2026-09-22 23:59:59"], &root);
    assert_eq!(day["range"]["from"], explicit["range"]["from"], "a zero-shifted --day is not the calendar window");
    assert_eq!(day["range"]["to"], explicit["range"]["to"], "a zero-shifted --day is not the calendar window");
    assert_eq!(day["total_matches"], json!(3), "at midnight the 00:30, 02:59:59 and 09:00 frames are all the 22nd's: {}", day["results"]);
    assert_eq!(payload_of(&["search", "--day", "2026-09-21"], &root)["total_matches"], json!(1), "and the 21st keeps only its own evening");
    let usage = payload_of(&["app-usage", "--day", "2026-09-22"], &root);
    assert_eq!(usage["range"], day["range"], "app-usage and search disagreed on a midnight day");
    let _ = std::fs::remove_dir_all(root);
}

/// A window a reader cannot see is a window nobody can trust after the fact.
#[test]
fn every_report_that_took_a_window_prints_the_one_it_used() {
    let root = day_install("day-printed", 180);
    let moment = fixture::at("2026-09-22_09-00-00").to_string();
    // The value of a report's `window:` line, with the label's column padding taken off.
    let window = |words: &[&str]| -> String {
        let text = stdout_of(words, &root);
        let line = text.lines().find(|line| line.starts_with("window:")).unwrap_or_else(|| panic!("`windmcp {}` printed no window line:\n{text}", words.join(" ")));
        line["window:".len()..].trim().to_string()
    };
    // The three that take a day print the *same sentence*, not merely the same interval.
    let expected = "2026-09-22T03:00:00+08:00 → 2026-09-23T02:59:59+08:00 (day, day_begin 03:00)";
    assert_eq!(window(&["search", "--day", "2026-09-22"]), expected);
    assert_eq!(window(&["app-usage", "--day", "2026-09-22"]), expected);
    assert_eq!(window(&["day-summary", "2026-09-22"]), expected);
    // The two that take a centre and a `--window` name the interval they read, and the day start
    // they did *not* apply, so a reader can tell the two kinds of window apart at a glance.
    for words in [vec!["around", "1790046000", "--window", "60"], vec!["frame", moment.as_str(), "--window", "60"]] {
        assert!(window(&words).contains("(centered, day_begin 03:00)"), "`windmcp {}` hid its rule: {}", words.join(" "), window(&words));
    }
    // `status` and `doctor` carry no window, but both must state the convention that decides one.
    for words in [vec!["status"], vec!["doctor"]] {
        let text = stdout_of(&words, &root);
        assert!(text.contains("product day") && text.contains("begins at 03:00") && text.contains("day_begin_minutes 180"), "`windmcp {}` never named the day start:\n{text}", words.join(" "));
    }
    let _ = std::fs::remove_dir_all(root);
}

/// A typo in a `--day` must be refused the same way by both binaries, or the two disagree about
/// what a mistake means as well as about what a correct answer means.
#[test]
fn a_malformed_day_is_refused_rather_than_widened_to_a_guess() {
    let root = day_install("day-typo", 180);
    for bad in ["2026-9-22", "2026-13-01", "yesterday", "2026-09-22_09-00-00"] {
        let output = Command::new(BIN).args(["app-usage", "--day", bad, "--root"]).arg(&root).output().unwrap();
        assert!(!output.status.success(), "`--day {bad}` was accepted");
        let message = String::from_utf8_lossy(&output.stderr).to_lowercase();
        assert!(message.contains("invalid --day") || message.contains("day"), "`--day {bad}` refused without saying why: {message}");
    }
    let _ = std::fs::remove_dir_all(root);
}

/// The two executables, one word. `windcapctl query --day` and `windmcp app-usage --day` are the
/// pair a user cross-checks — "what was on my screen" against "what did I use" — so the interval
/// behind that word has to be one interval. This is the only test in the workspace that runs both
/// binaries and compares what they printed.
#[test]
fn windcapctl_and_windmcp_agree_on_what_one_day_is() {
    let root = day_install("cross-binary", 180);
    let (from, to) = wind_base::clock::day_bounds(2026, 9, 22, 180);
    let control = Command::new(sibling_bin("windcapctl"))
        .args(["query", "--day", "2026-09-22", "--root"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(control.status.success(), "windcapctl failed: {}", String::from_utf8_lossy(&control.stderr));
    let printed = String::from_utf8_lossy(&control.stdout).to_string();
    let line = printed.lines().find(|line| line.contains("window ")).unwrap_or_else(|| panic!("no window line from windcapctl:\n{printed}"));
    // The two report the same instants in their own house styles: naive-local `HH:MM:SS` there,
    // ISO-8601 with the measured offset here, so the comparison is on the numbers, not the spelling.
    assert!(
        line.contains(&format!(
            "{} .. {}",
            wind_base::clock::LocalParts::from_naive_epoch(from).display(),
            wind_base::clock::LocalParts::from_naive_epoch(to).display()
        )),
        "windcapctl's window is not the product day: {line}"
    );
    assert!(line.contains("(day, day_begin 03:00)"), "windcapctl hid its rule: {line}");
    let bridge = payload_of(&["app-usage", "--day", "2026-09-22"], &root);
    assert_eq!(bridge["range"]["from"].as_i64(), Some(from), "windmcp opened {bridge} where {line} opened");
    assert_eq!(bridge["range"]["to"].as_i64(), Some(to), "windmcp closed {bridge} where {line} closed");
    // Same window, so the same frames — the 21st's three small-hours frames are not in it either way.
    assert_eq!(bridge["titled_frames"], json!(1), "{bridge} vs {line}");
    let _ = std::fs::remove_dir_all(root);
}

/// The sibling executable, which `cargo test --workspace` — the gate this repository is checked by —
/// always builds alongside the test binary running this.
fn sibling_bin(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("a test runs from a file");
    let debug = exe.parent().and_then(Path::parent).expect("target/<profile>/deps");
    let bin = debug.join(if cfg!(windows) { format!("{name}.exe") } else { name.to_string() });
    assert!(bin.exists(), "{} is not built; this test needs `cargo test --offline --workspace --no-fail-fast`", bin.display());
    bin
}

fn runtime(harness: &Harness) -> (wind_mcp::Runtime, wind_mcp::Axis) {
    (wind_mcp::Runtime::open(&harness.root).unwrap(), wind_mcp::Axis::measure())
}

/// The answers in `tests/ai_cache.rs` are proven as plain function calls there. This proves them as
/// what an AI client actually receives from the running service, because the defect was never in the
/// function in isolation — it was in a day query coming back empty over a wire that reported no error.
///
/// Getting there changes nothing about the gate: same loopback bind, same bearer token, same token
/// that lives only in the config file. A wider answer set is not a wider audience.
#[test]
fn the_running_service_answers_a_day_from_the_month_cache_and_labels_it() {
    let harness = Harness::seeded("ai-over-the-wire", json!(null));
    let dir = harness.root.join("userdata/result_ai_extract_tag");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("2026.json"), json!({"2026-09": ["rust", "mcp", "refactoring"]}).to_string()).unwrap();

    let mut server = Server::start(&harness.root);
    server.wait_until_listening(harness.port);

    // No token, still refused, and refused before the new answer is reachable any more than the old
    // one was.
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {
        "name": "windrecorder_day_summary", "arguments": {"date": "2026-09-21"}}})
    .to_string();
    assert_eq!(post(harness.port, &body, None, None).status, 401, "an unauthenticated day query was served");

    let mut client = Client::handshake(harness.port, TOKEN);
    let day = client.structured("windrecorder_day_summary", &json!({"date": "2026-09-21"}));
    assert_eq!(day["date"], json!("2026-09-21"));
    assert_eq!(day["ai_tags"]["state"], json!("answered"), "{day}");
    assert_eq!(day["ai_tags"]["granularity"], json!("month"), "the wire answer must state its own width");
    assert_eq!(day["ai_tags"]["cache_key"], json!("2026-09"));
    assert_eq!(day["ai_tags"]["tags"].as_array().map(Vec::len), Some(3), "{day}");
    assert_eq!(day["ai_summary"]["state"], json!("not_generated"), "and must not invent a summary: {day}");

    let status = client.structured("windrecorder_status", &json!({}));
    assert_eq!(status["ai_caches"]["tags"]["years"][0]["month_keys"], json!(1));
    assert_eq!(status["ai_caches"]["tags"]["years"][0]["day_keys"], json!(0));
}
