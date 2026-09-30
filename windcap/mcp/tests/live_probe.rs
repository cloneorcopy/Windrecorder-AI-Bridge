//! TEMPORARY probe: drives a real `windmcp serve` over raw HTTP and prints the actual responses.
//! Run with: cargo test -p wind-mcp --offline --test live_probe -- --nocapture

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use wind_mcp::fixture;

const BIN: &str = env!("CARGO_BIN_EXE_windmcp");
const TOKEN: &str = "probe-token-long-enough-to-satisfy-the-guard";

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

fn talk(port: u16, request: &str) -> (u16, Vec<(String, String)>, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.shutdown(std::net::Shutdown::Write).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).replace("\r\n", "\n");
    let (head, body) = text.split_once("\n\n").unwrap_or((text.as_str(), ""));
    let mut lines = head.lines();
    let status: u16 = lines.next().unwrap_or_default().split_whitespace().nth(1).unwrap_or("0").parse().unwrap_or(0);
    let headers = lines
        .filter_map(|l| l.split_once(':').map(|(n, v)| (n.trim().to_lowercase(), v.trim().to_string())))
        .collect();
    (status, headers, body.to_string())
}

/// Walk the parsed response and shorten only the base64 payloads, so the transcript is valid JSON
/// rather than a hand-edited one. Everything an agent would read is printed in full.
fn elide(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (_, entry) in map.iter_mut() {
                elide(entry);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(elide),
        Value::String(text) => {
            let looks_base64 = text.len() > 60
                && text.bytes().all(|b| b.is_ascii_alphanumeric() || b"=/+".contains(&b));
            if looks_base64 {
                let head: String = text.chars().take(20).collect();
                *text = format!("{head}...[{} bytes of base64 elided]", text.len());
            }
        }
        _ => {}
    }
}

fn render(body: &str) -> String {
    let mut parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    elide(&mut parsed);
    parsed.to_string()
}

struct Rpc {
    port: u16,
    id: i64,
    session: Option<String>,
}

impl Rpc {
    fn send(&mut self, method: &str, params: Value, token: Option<&str>) -> (u16, Vec<(String, String)>, String) {
        self.id += 1;
        let body = json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params}).to_string();
        self.send_raw(&body, token)
    }

    fn send_raw(&self, body: &str, token: Option<&str>) -> (u16, Vec<(String, String)>, String) {
        let mut request = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n",
            self.port,
            body.len()
        );
        if let Some(token) = token {
            request.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        if let Some(session) = &self.session {
            request.push_str(&format!("Mcp-Session-Id: {session}\r\n"));
        }
        request.push_str("\r\n");
        request.push_str(body);
        talk(self.port, &request)
    }

    fn show(&mut self, label: &str, method: &str, params: Value) -> Value {
        let (status, headers, body) = self.send(method, params, Some(TOKEN));
        println!("\n--- {label}  ({method}) ---");
        println!("HTTP {status}   session: {}", headers.iter().find(|(n, _)| n == "mcp-session-id").map(|(_, v)| v.as_str()).unwrap_or("-"));
        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        println!("{}", render(&parsed.to_string()));
        parsed
    }
}

#[test]
fn live_probe() {
    let port = free_port();
    let root = fixture::install("live-probe", &json!({
        "enable_mcp_server": true, "mcp_server_host": "127.0.0.1", "mcp_server_port": port,
        "mcp_server_token": TOKEN, "mcp_server_auth_required": true,
    }).to_string());
    fixture::month(&root, "default", 2026, 9, &fixture::busy_day());
    fixture::slice(&root, fixture::SEGMENT, &format!("{}.jpg", fixture::SEGMENT));
    fixture::segment(&root, 2026, 9, fixture::SEGMENT);

    // The shape every real install is in: `windai tags --month` has written `2026-09`, and no day key
    // exists anywhere. Twenty-two month tags is what a full month actually produces, so the response
    // cap and its `omitted_tags` counter are both in the transcript rather than only in a unit test.
    let tags_dir = root.join("userdata/result_ai_extract_tag");
    std::fs::create_dir_all(&tags_dir).unwrap();
    let month: Vec<String> = (1..=22).map(|n| format!("tag{n}")).collect();
    std::fs::write(
        tags_dir.join("2026.json"),
        json!({"2026-08": ["teaching", "grading"], "2026-09": month}).to_string(),
    )
    .unwrap();

    let log = root.join("serve.log");
    let out = std::fs::File::create(&log).unwrap();
    let err = std::fs::File::try_clone(&out).unwrap();
    let mut child = std::process::Command::new(BIN)
        .args(["serve", "--root"])
        .arg(&root)
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err))
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        if child.try_wait().unwrap().is_some() {
            panic!("exited:\n{}", std::fs::read_to_string(&log).unwrap());
        }
        assert!(Instant::now() < deadline, "never came up");
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("\n=== windmcp serve, stderr as written by the process ===");
    print!("{}", std::fs::read_to_string(&log).unwrap());

    let mut rpc = Rpc { port, id: 0, session: None };

    // 1. the handshake, verbatim
    let (status, headers, body) = rpc.send("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "live-probe", "version": "0"}}), Some(TOKEN));
    println!("\n--- initialize ---\nHTTP {status}");
    println!("mcp-session-id: {}", headers.iter().find(|(n, _)| n == "mcp-session-id").map(|(_, v)| v.as_str()).unwrap_or("<none>"));
    let parsed: Value = serde_json::from_str(&body).unwrap();
    println!("{}", serde_json::to_string_pretty(&parsed["result"]["serverInfo"]).unwrap());
    println!("protocolVersion = {}", parsed["result"]["protocolVersion"]);
    println!("capabilities    = {}", parsed["result"]["capabilities"]);
    println!("instructions    = {} bytes of text", parsed["result"]["instructions"].as_str().unwrap().len());
    rpc.session = headers.iter().find(|(n, _)| n == "mcp-session-id").map(|(_, v)| v.clone());

    let (n, _, _) = rpc.send_raw(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}).to_string(), Some(TOKEN));
    println!("notifications/initialized -> HTTP {n} (no body, as a notification must be)");

    // 2. the tool list, names and bounds only
    let listed = rpc.show("tools/list", "tools/list", json!({}));
    println!("\ntool                required            read-only");
    for tool in listed["result"]["tools"].as_array().unwrap() {
        println!(
            "{:<20}{:<20}{}",
            tool["name"].as_str().unwrap(),
            tool["inputSchema"]["required"].to_string(),
            tool["annotations"]["readOnlyHint"]
        );
    }
    rpc.show("resources/templates/list", "resources/templates/list", json!({}));

    // 3. every tool, with the real answers
    rpc.show("windrecorder_status", "tools/call", json!({"name": "windrecorder_status", "arguments": {}}));
    let search = rpc.show(
        "windrecorder_search",
        "tools/call",
        json!({"name": "windrecorder_search", "arguments": {"keywords": "所有权", "start": "2026-09-21", "end": "2026-09-21 23:59:59"}}),
    );
    let moment = search["result"]["structuredContent"]["results"][0]["timestamp"].clone();
    rpc.show("windrecorder_search (out of range)", "tools/call", json!({"name": "windrecorder_search", "arguments": {"keywords": "anything", "start": "2019-01-01", "end": "2019-01-02"}}));
    rpc.show("windrecorder_around (handed back the timestamp)", "tools/call", json!({"name": "windrecorder_around", "arguments": {"timestamp": moment, "window_seconds": 240, "limit": 3}}));
    rpc.show("windrecorder_app_usage", "tools/call", json!({"name": "windrecorder_app_usage", "arguments": {"start": "2026-09-21", "end": "2026-09-21 23:59:59"}}));
    rpc.show("windrecorder_day_summary", "tools/call", json!({"name": "windrecorder_day_summary", "arguments": {"date": "2026-09-21"}}));
    rpc.show("windrecorder_frame", "tools/call", json!({"name": "windrecorder_frame", "arguments": {"timestamp": moment}}));
    rpc.show("a bad argument", "tools/call", json!({"name": "windrecorder_search", "arguments": {"keywords": "x", "start": "yesterday", "end": "today"}}));

    // 3b. the AI answers, from the same socket, on a root whose only tags are month-shaped. Before
    // this existed all three of these calls returned no `ai_tags` field at all, which a client could
    // read in exactly one way: "this user has no tags", about a month that had them.
    let inside = rpc.show("day_summary for 2026-09-21, inside a tagged month", "tools/call", json!({"name": "windrecorder_day_summary", "arguments": {"date": "2026-09-21"}}));
    let outside = rpc.show("day_summary for 2026-11-05, a month nobody tagged", "tools/call", json!({"name": "windrecorder_day_summary", "arguments": {"date": "2026-11-05"}}));
    std::fs::remove_dir_all(&tags_dir).unwrap();
    let never = rpc.show("day_summary for 2026-09-21 with no tags cache at all", "tools/call", json!({"name": "windrecorder_day_summary", "arguments": {"date": "2026-09-21"}}));
    let tags_of = |answer: &Value| answer["result"]["structuredContent"]["ai_tags"].clone();
    let (in_month, off_month, no_cache) = (tags_of(&inside), tags_of(&outside), tags_of(&never));
    assert_eq!(in_month["granularity"], json!("month"), "a month answer must label itself: {in_month}");
    assert_eq!(in_month["tags"].as_array().map(Vec::len), Some(20), "{in_month}");
    assert_eq!(in_month["omitted_tags"], json!(2), "the two the cap dropped must be counted, not lost");
    assert_eq!(off_month["state"], json!("not_generated"), "{off_month}");
    assert!(off_month["note"].as_str().unwrap().contains("2 month"), "and must say what the file does hold: {}", off_month["note"]);
    assert_eq!(no_cache["state"], json!("not_generated"), "{no_cache}");
    assert_eq!(off_month["cache_file_present"], json!(true), "{off_month}");
    assert_eq!(no_cache["cache_file_present"], json!(false), "{no_cache}");
    assert_ne!(off_month["note"], no_cache["note"], "an untagged month and an absent cache are different facts and must not read alike");
    println!("\n--- the three no-answers, side by side ---");
    println!("tagged month     granularity={} state={} cache_file_present={} tags={}", in_month["granularity"], in_month["state"], in_month["cache_file_present"], in_month["tags"].as_array().map_or(0, Vec::len));
    println!("untagged month   granularity={} state={} cache_file_present={} note={}", off_month["granularity"], off_month["state"], off_month["cache_file_present"], off_month["note"]);
    println!("no cache at all  granularity={} state={} cache_file_present={} note={}", no_cache["granularity"], no_cache["state"], no_cache["cache_file_present"], no_cache["note"]);
    std::fs::create_dir_all(&tags_dir).unwrap();
    std::fs::write(tags_dir.join("2026.json"), json!({"2026-08": ["teaching", "grading"], "2026-09": month}).to_string()).unwrap();

    // 4. the resource
    let uri = format!("windrecorder://thumbnail/{moment}");
    rpc.show(&format!("resources/read {uri}"), "resources/read", json!({"uri": uri}));

    // 5. the gate, proven over the same socket
    println!("\n=== the access rules, as observed on the wire ===");
    let body = json!({"jsonrpc": "2.0", "id": 900, "method": "tools/list"}).to_string();
    let bare = format!("POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    let (status, headers, text) = talk(port, &bare);
    println!("no Authorization header        -> HTTP {status}, www-authenticate: {:?}, body: {text}", headers.iter().find(|(n, _)| n == "www-authenticate").map(|(_, v)| v.as_str()));
    let wrong = rpc.send_raw(&body, Some("not-the-token-but-long-enough-to-be-plausible"));
    println!("wrong token                    -> HTTP {}", wrong.0);
    let near = rpc.send_raw(&body, Some(&TOKEN[..TOKEN.len() - 2]));
    println!("near-miss token                -> HTTP {}", near.0);
    let query = format!("POST /mcp?token={TOKEN} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
    let (status, _, text) = talk(port, &query);
    println!("token in the query string      -> HTTP {status}, body: {}", text.chars().take(120).collect::<String>());
    let with_bogus_session = {
        let forged = Rpc { port, id: 900, session: Some("invented-session-id".to_string()) };
        forged.send_raw(&body, Some(TOKEN))
    };
    println!("invented Mcp-Session-Id        -> HTTP {}", with_bogus_session.0);
    let rebinding = format!(
        "POST /mcp HTTP/1.1\r\nHost: evil.example\r\nOrigin: http://evil.example\r\nAuthorization: Bearer {TOKEN}\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let (status, _, _) = talk(port, &rebinding);
    println!("Host/Origin of another site    -> HTTP {status} (DNS-rebinding guard)");
    println!("right token                    -> HTTP {}", rpc.send_raw(&body, Some(TOKEN)).0);

    // 6. rotation, with no restart
    let rotated = format!("{TOKEN}-rotated");
    std::fs::write(root.join("userdata/config_user.json"), json!({
        "enable_mcp_server": true, "mcp_server_host": "127.0.0.1", "mcp_server_port": port,
        "mcp_server_token": rotated, "mcp_server_auth_required": true,
    }).to_string()).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    println!("token rotated in the config    -> old: HTTP {}, new: HTTP {}", rpc.send_raw(&body, Some(TOKEN)).0, rpc.send_raw(&body, Some(&rotated)).0);

    let _ = child.kill();
    let _ = child.wait();

    // The same install, seen from a terminal rather than a socket: `doctor` and the tool commands are
    // the other half of the transcript, and they must not contradict it.
    let run = |words: &[&str]| -> String {
        let output = std::process::Command::new(BIN).args(words).arg("--root").arg(&root).output().unwrap();
        assert!(output.status.success(), "{} failed:\n{}", words.join(" "), String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).to_string()
    };
    println!("\n=== windmcp doctor ===");
    print!("{}", run(&["doctor"]));
    println!("\n=== windmcp search 所有权 --day 2026-09-21 ===");
    print!("{}", run(&["search", "所有权", "--day", "2026-09-21"]));
    println!("\n=== windmcp app-usage --day 2026-09-21 ===");
    print!("{}", run(&["app-usage", "--day", "2026-09-21"]));
    println!("\n=== windmcp day-summary 2026-09-21 ===");
    print!("{}", run(&["day-summary", "2026-09-21"]));
    println!("\n=== windmcp around 1789981710 --window 240 --limit 2 ===");
    print!("{}", run(&["around", "1789981710", "--window", "240", "--limit", "2"]));

    // What the transcript above claims, asserted rather than printed: the two tools that measure time
    // agree, the settled key names hold, and nothing that left this service is an unnormalised title.
    let usage: Value = serde_json::from_str(&run(&["app-usage", "--day", "2026-09-21", "--json"])).unwrap();
    let summary: Value = serde_json::from_str(&run(&["day-summary", "2026-09-21", "--json"])).unwrap();
    let status: Value = serde_json::from_str(&run(&["status", "--json"])).unwrap();
    assert_eq!(usage["total_counted_seconds"], summary["total_counted_seconds"], "app-usage and day-summary disagreed");
    assert_eq!(usage["total_counted_seconds"], json!(370));
    assert!(status["databases"].is_array() && status.get("months").is_none() && status.get("search_semantics").is_none());
    assert!(!summary.to_string().contains("forecast"), "a day summary leaked screen text");
    assert!(!usage.to_string().contains("1Password"), "an excluded title leaked");
    assert!(usage.to_string().contains("+08:00"), "a rendered time carried no offset");
    let _ = std::fs::remove_dir_all(root);
}
