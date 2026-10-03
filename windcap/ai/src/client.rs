//! The OpenAI-compatible chat client: request shape, URL assembly, response parsing.
//!
//! One endpoint, one verb, one request shape. `POST {open_ai_base_url}/chat/completions` with
//! `{"model", "messages": [{"role":"system"},{"role":"user"}], "temperature"}` and, when the caller
//! needs machine-readable output, `"response_format": {"type": "json_object"}`. That is the whole
//! compatible surface `windrecorder/llm.py` used, and it is what every gateway that calls itself
//! "OpenAI compatible" implements.
//!
//! # What this module is careful about
//!
//! * **The key.** It is read from configuration (`crate::settings`), formatted into exactly one
//!   header, and never into anything else. Every error leaving here is built through `Faults`, which
//!   strips it. See `crate::error` for why redaction happens at construction rather than at print.
//! * **Plain HTTP.** Sent, when that is what the address says. There is deliberately no scheme gate
//!   here: the address is the user's, and a rule that refused `http://` beyond loopback made every
//!   self-hosted gateway on the same network unusable while reporting itself as a configuration fault
//!   — a home-lab endpoint at `http://192.168.x.x:3000/v1` is the ordinary shape of running your own
//!   model server, and nothing in this product gets to call that a typo. What a cleartext request
//!   costs is the user's judgement, made once when the address was typed; the diagnostics still name
//!   the scheme they are about to use, so a mistake is visible rather than silent.
//! * **Request bodies.** The prompt for the tag feature contains the user's window titles; the prompt
//!   for search contains the phrase they typed. Neither is ever logged, and the byte count is the
//!   only statistic this crate writes anywhere about a body.
//! * **The response.** `choices[0].message.content` is not trusted to be a string, or to be present,
//!   or to be JSON, or to be *in range* — see `crate::plan`, which is where a parsed model answer is
//!   validated before it is allowed anywhere near the index.

use serde_json::{json, Value};

use crate::error::{AiError, Faults};
use crate::http;
use crate::settings::Settings;

/// Seam for the socket layer.
///
/// Production uses [`WinHttp`]. Tests use it too, pointed at a loopback listener, so the request
/// bytes asserted on are the bytes WinHTTP would really put on the wire — and a [`Scripted`] double
/// for the cases no socket can produce deterministically (a truncated body, a header set inspected
/// without a proxy in the middle).
///
/// `post` borrows `&self` and answers, and that is the whole contract. The one caller that works on
/// several things at a time — `summarize`, which keeps four requests in flight — gives each of its lanes
/// its own client instead of sharing one across the threads; see [`Client::transport`] for why.
pub trait Transport {
    fn post(&self, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<http::Response, http::TransportError>;
}

/// The real transport: WinHTTP, TLS trust delegated to the system certificate store.
#[derive(Debug, Clone, Copy)]
pub struct WinHttp;

impl Transport for WinHttp {
    fn post(&self, url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<http::Response, http::TransportError> {
        http::post(url, headers, body)
    }
}

/// One turn of conversation to send: a system prompt and a single user message, which is the shape
/// both upstream call sites use.
#[derive(Debug, Clone, Copy)]
pub struct ChatRequest<'a> {
    pub system: &'a str,
    pub user: &'a str,
    /// Upstream's per-feature constants: 0.7 default, 0.3 for tags, 0.2 for query parsing, 0.1 for
    /// dates. Low temperatures are for answers that must be parseable, not creative.
    pub temperature: f64,
    /// Ask for `response_format: json_object`. Supported by the OpenAI-compatible family broadly;
    /// a gateway that ignores it still has to return text the parser can work with.
    pub json_mode: bool,
}

impl ChatRequest<'_> {
    /// The request document. Split out so the shape is assertable without a socket.
    pub fn to_json(&self, model: &str) -> Value {
        let mut body = json!({
            "model": model,
            "messages": [
                {"role": "system", "content": self.system},
                {"role": "user", "content": self.user},
            ],
            // Non-streaming on purpose: a streamed completion has to be reassembled before it can be
            // parsed, and the two callers here both want one complete answer.
            "stream": false,
        });
        // `temperature` is omitted rather than zeroed for gateways that reject it outright (several
        // reasoning models do), and clamped because the JSON schema allows values that would be a
        // server-side 400.
        if (0.0..=2.0).contains(&self.temperature) {
            body["temperature"] = json!(self.temperature);
        }
        if self.json_mode {
            body["response_format"] = json!({"type": "json_object"});
        }
        body
    }
}

/// Token counts, when the gateway reports them. Surfaced by `windai search --explain` so a user can
/// see what a phrase cost before deciding whether to do it again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

/// A parsed completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    pub usage: Option<Usage>,
}

pub struct Client<T: Transport = WinHttp> {
    pub settings: Settings,
    faults: Faults,
    transport: T,
}

impl Client<WinHttp> {
    /// A client for the configuration the user actually has.
    pub fn new(settings: Settings) -> Client<WinHttp> {
        let faults = Faults::new(&settings.api_key);
        Client { settings, faults, transport: WinHttp }
    }
}

impl<T: Transport> Client<T> {
    pub fn with_transport(settings: Settings, transport: T) -> Client<T> {
        let faults = Faults::new(&settings.api_key);
        Client { settings, faults, transport }
    }

    pub fn faults(&self) -> &Faults {
        &self.faults
    }

    pub fn error(&self, build: impl FnOnce(&Faults) -> AiError) -> AiError {
        build(&self.faults)
    }

    /// The transport value this client posts through, for a caller that has to hand one to a thread.
    ///
    /// This exists for `summarize`'s waves. A lane cannot borrow `&Client<T>`: `Client` is generic over
    /// `T: Transport` and that bound promises nothing about sharing, so putting one client behind two
    /// threads would need either a `Client<T>: Sync` bound pushed onto every transport — including the
    /// scripted doubles in tests — or an `unsafe impl` this crate is not going to write. What a lane
    /// needs instead is the pair `Client` is built from: the settings (which are `Clone`) and this. Each
    /// lane then calls [`Client::with_transport`] for the request it is making and owns the result.
    /// Nothing is lost by that, because `Client` keeps no state between calls — `http::post` opens its
    /// own WinHTTP session per request and closes it — so two lanes sharing one client would have been
    /// sharing nothing but a `Settings` copy anyway.
    pub fn transport(&self) -> T
    where
        T: Clone,
    {
        self.transport.clone()
    }

    /// Send one chat request and return the assistant's text.
    pub fn ask(&self, request: &ChatRequest<'_>) -> Result<Completion, AiError> {
        self.settings.require_usable(&self.faults)?;
        let url = self.settings.chat_completions_url();
        let body = serde_json::to_vec(&request.to_json(&self.settings.model)).map_err(|e| {
            // A serialization failure can only mean the prompt is not representable as JSON, which
            // is a bug in a caller, not in the user's data.
            self.faults.protocol(format!("cannot encode the request: {e}"))
        })?;
        // Bound outside the array so the borrow outlives the statement that builds it. This `String`
        // is the only place in the crate where the token exists outside `SecretKey`, and it is dropped
        // at the end of `ask`.
        let authorization = format!("Bearer {}", self.settings.api_key.expose());
        let headers = [
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
            // The one header the token is ever formatted into.
            ("Authorization", authorization.as_str()),
            ("User-Agent", "windai/0.1 (Windrecorder)"),
        ];

        let response = self.transport.post(&url, &headers, &body).map_err(|e| self.faults.transport(e))?;
        if !(200..300).contains(&response.status) {
            // Status and body only: the body is the endpoint's own explanation of itself, and it is
            // the one field copied from outside that could carry our header back to us. `Faults::http`
            // redacts before it is stored.
            return Err(self.faults.http(response.status, &response.body));
        }
        parse_completion(&response.body, &self.faults)
    }

    /// A single round trip that proves the configuration end to end: real TLS, real auth, real model.
    /// Used by `windai doctor`. The prompt carries no user data.
    pub fn ping(&self) -> Result<Completion, AiError> {
        self.ask(&ChatRequest {
            system: "You are a connectivity check. Reply with the single word: ok",
            user: "ping",
            temperature: 0.0,
            json_mode: false,
        })
    }
}

/// Pull the assistant text out of a chat-completions reply.
///
/// Written against the shape the family agrees on, but defensively at every step, because the
/// implementations disagree about failure: a gateway returning HTTP 200 with `{"error": …}` is common
/// enough to be a supported case, and `content` being `null` (a reasoning model that filled the token
/// budget before emitting anything) is common enough that it must not become a panic or an empty
/// answer mistaken for a real one.
pub fn parse_completion(body: &[u8], faults: &Faults) -> Result<Completion, AiError> {
    if body.is_empty() {
        return Err(faults.protocol("the endpoint replied with no body at all"));
    }
    let value: Value = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(e) => {
            // A non-JSON reply from a `/v1` path is almost always a captive portal, a login page, or
            // a base URL pointing at the wrong service. The first 200 characters say which.
            let preview = String::from_utf8_lossy(&body[..body.len().min(200)]);
            return Err(faults.protocol(format!("reply is not JSON ({e}): {preview}")));
        }
    };
    if let Some(error) = value.get("error") {
        return Err(faults.protocol(format!("the endpoint reported an error: {}", clip_text(error))));
    }
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .ok_or_else(|| faults.protocol(format!("reply carries no `choices` array: {}", clip_text(&value))))?;
    let first = choices
        .first()
        .ok_or_else(|| faults.protocol("reply carries an empty `choices` array"))?;
    // `finish_reason` is deliberately not inspected. Upstream does not, and a `length` truncation is
    // caught more reliably by the JSON parser in `plan` than by a field some gateways omit.
    let message = first.get("message").ok_or_else(|| {
        faults.protocol(format!("the first choice carries no `message`: {}", clip_text(first)))
    })?;
    let text = content_text(message.get("content")).ok_or_else(|| {
        faults.protocol(format!("the reply carries no text content: {}", clip_text(message)))
    })?;
    if text.trim().is_empty() {
        return Err(faults.protocol("the model returned an empty answer"));
    }
    Ok(Completion { text, usage: read_usage(&value) })
}

/// `content` as a string, or as an array of `{"type":"text","text":…}` parts.
///
/// The array form is what the newer revisions of the API return for multimodal-capable models, and a
/// client that only handles the string form reports those models as broken.
fn content_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                match part.get("text").and_then(Value::as_str) {
                    Some(text) => {
                        if !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(text);
                    }
                    None => return None,
                }
            }
            if out.is_empty() {
                None
            } else {
                Some(out)
            }
        }
        _ => None,
    }
}

fn read_usage(value: &Value) -> Option<Usage> {
    let usage = value.get("usage")?;
    let number = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
    Some(Usage {
        prompt_tokens: number("prompt_tokens"),
        completion_tokens: number("completion_tokens"),
        total_tokens: number("total_tokens"),
    })
}

/// A short rendering of a JSON fragment, for error messages that have to show what arrived.
fn clip_text(value: &Value) -> String {
    let text = value.to_string();
    if text.len() > 400 {
        let mut end = 400;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &text[..end])
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_url(url: &str) -> Settings {
        crate::test_support::settings("client-url", &json!({ "open_ai_base_url": url }))
    }

    #[test]
    fn the_request_document_is_the_shape_the_family_expects() {
        let request = ChatRequest { system: "sys", user: "usr", temperature: 0.2, json_mode: true };
        let body = request.to_json("gpt-4o");
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], false);
        assert_eq!(body["response_format"]["type"], "json_object");
        assert_eq!(body["temperature"], 0.2);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "system then user, as upstream sends");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["content"], "usr");

        let plain = ChatRequest { system: "s", user: "u", temperature: 0.7, json_mode: false }.to_json("m");
        assert!(plain.get("response_format").is_none(), "JSON mode is opt-in per call");
    }

    #[test]
    fn an_out_of_range_temperature_is_omitted_rather_than_sent() {
        // Several hosted endpoints 400 on a temperature they do not support at all; sending a value
        // the schema forbids would turn a configuration question into a request failure.
        for temperature in [-1.0, 2.5, f64::NAN] {
            let body = ChatRequest { system: "s", user: "u", temperature, json_mode: false }.to_json("m");
            assert!(body.get("temperature").is_none(), "{temperature}");
        }
    }

    #[test]
    fn a_reply_is_read_out_of_the_documented_shape() {
        let faults = Faults::anonymous();
        let body = br#"{"choices":[{"message":{"role":"assistant","content":"  hello  "}}],
            "usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}}"#;
        let completion = parse_completion(body, &faults).unwrap();
        assert_eq!(completion.text, "  hello  ", "whitespace is the caller's business, not ours");
        assert_eq!(completion.usage, Some(Usage { prompt_tokens: 11, completion_tokens: 3, total_tokens: 14 }));
    }

    #[test]
    fn a_part_array_content_is_joined() {
        let faults = Faults::anonymous();
        let body = br#"{"choices":[{"message":{"content":[{"type":"text","text":"one"},{"type":"text","text":"two"}]}}]}"#;
        assert_eq!(parse_completion(body, &faults).unwrap().text, "one\ntwo");
    }

    #[test]
    fn every_way_a_reply_can_be_unusable_is_reported_not_panicked() {
        let faults = Faults::anonymous();
        let cases: Vec<(&[u8], &str)> = vec![
            (b"", "no body"),
            (b"<html>sign in</html>", "not JSON"),
            (b"[]", "not an object"),
            (br#"{"error":{"message":"insufficient_quota"}}"#, "insufficient_quota"),
            (br#"{"id":"chatcmpl-1"}"#, "no `choices`"),
            (br#"{"choices":[]}"#, "empty `choices`"),
            (br#"{"choices":[{"message":{}}]}"#, "no text content"),
            (br#"{"choices":[{"message":{"content":null}}]}"#, "no text content"),
            (br#"{"choices":[{"message":{"content":"   "}}]}"#, "empty answer"),
        ];
        for (body, _) in cases {
            let error = parse_completion(body, &faults).expect_err("must be refused");
            assert_eq!(error.kind(), crate::error::ErrorKind::Protocol, "{}", String::from_utf8_lossy(body));
        }
    }

    #[test]
    fn a_cleartext_address_is_sent_to_rather_than_refused() {
        // The rule this replaces refused `http://` beyond loopback inside `ask`, before a hostname was
        // even resolved: a self-hosted gateway on the user's own network could not be used at all, and
        // the refusal arrived labelled as a configuration fault, which is how a working install reads as
        // a broken one. The claim now is the opposite, and it is observable rather than inferred — the
        // transport is handed the cleartext URL, and whatever happens next is the network's answer.
        let spy = Spy::default();
        let client = Client::with_transport(base_url("http://192.0.2.10:3321/v1"), spy.clone());
        let completion = client
            .ask(&ChatRequest { system: "s", user: "u", temperature: 0.0, json_mode: false })
            .expect("a stub that replies has replied, whatever the scheme");
        assert_eq!(completion.text, "ok");
        assert_eq!(spy.last_url(), "http://192.0.2.10:3321/v1/chat/completions", "the address as typed, plus the endpoint");

        // And a host nobody can reach is a transport failure rather than a policy refusal.
        let unreachable = Client::with_transport(base_url("http://api.somewhere.test/v1"), FailOnce);
        let error = unreachable
            .ask(&ChatRequest { system: "s", user: "u", temperature: 0.0, json_mode: false })
            .expect_err("the stub transport fails by design");
        assert_eq!(error.kind(), crate::error::ErrorKind::Transport, "nothing stands in front of the socket any more");
    }

    #[test]
    fn an_unconfigured_install_is_refused_before_the_socket_is_touched() {
        let mut settings = Settings::read(&crate::test_support::repo_config());
        settings.base_url = String::new();
        let client = Client::with_transport(settings, FailOnce);
        let error = client
            .ask(&ChatRequest { system: "s", user: "u", temperature: 0.0, json_mode: false })
            .expect_err("no base url means no request");
        assert!(error.to_string().contains("open_ai_base_url"), "{error}");
    }

    /// A transport that never touches a socket, for asserting the checks that run before one — which
    /// after the scheme gate went is exactly one thing: `Settings::require_usable`.
    struct FailOnce;
    impl Transport for FailOnce {
        fn post(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &[u8],
        ) -> Result<http::Response, http::TransportError> {
            Err(http::TransportError("stub transport: not reached".to_string()))
        }
    }

    /// A transport that answers and remembers the URL it answered, so the scheme the request carried
    /// can be asserted rather than assumed from a lack of error.
    #[derive(Clone, Default)]
    struct Spy(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl Spy {
        fn last_url(&self) -> String {
            let seen = self.0.lock().expect("spy log");
            seen.last().cloned().expect("the spy saw nothing")
        }
    }

    impl Transport for Spy {
        fn post(&self, url: &str, _headers: &[(&str, &str)], _body: &[u8]) -> Result<http::Response, http::TransportError> {
            self.0.lock().expect("spy log").push(url.to_string());
            Ok(http::Response {
                status: 200,
                body: crate::test_support::completion_body("ok").into_bytes(),
            })
        }
    }
}
