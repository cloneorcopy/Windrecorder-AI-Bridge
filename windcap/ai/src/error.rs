//! Every failure this crate can produce, and the one rule that makes them safe to print.
//!
//! # The redaction rule
//!
//! A request to an LLM endpoint carries a bearer token, and error text is precisely the output most
//! likely to end up on a screen, in a console scroll-back, in a screenshot the user posts to a forum,
//! or in a log line the next debugging session reads back. So the token is removed where the error is
//! *built*, not where it is printed. That order matters: redacting at print time means the caller has
//! to remember to call the right method, and `{err:?}` — which is what half of Rust's diagnostics
//! actually use — would bypass it and print the raw payload.
//!
//! [`Faults`] is the only constructor for an [`AiError`] that can carry text from outside this
//! crate, and it holds the key. What comes out the other side is already clean, which is why
//! `AiError` is free to implement `Display` and `Debug` normally and why a future caller cannot
//! print an unredacted message by reaching for the wrong formatter.
//!
//! Three further places the key is kept out of, all of them upstream's defaults rather than mine:
//!   * **argv** — there is no `--api-key` flag. Windows exposes another process's command line
//!     through WMI and Process Explorer, so a key passed as an argument is readable by anything the
//!     user later runs, and it lands in shell history and in the process list of every crash report.
//!   * **environment** — `OPENAI_API_KEY` is deliberately *not* consulted. The child-process
//!     environment is inherited by the OCR helper, ffmpeg and any extension the user installs.
//!   * **logs** — `client` logs byte counts and a key fingerprint, never a body. Upstream logs the
//!     whole message array at INFO level, which is a separate and worse leak; see `client`.
//!
//! The key lives where upstream keeps it: `userdata/config_user.json`, read through
//! `wind_base::Config`.

use std::path::Path;

use crate::settings::SecretKey;

/// The placeholder standing in for a bearer token wherever it would otherwise be printed.
pub const REDACTED: &str = "[REDACTED]";

/// Where a failure came from, in one phrase, for the CLI's prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ErrorKind {
    /// A required `open_ai_*` key is missing, or still holds the placeholder the default config ships.
    Unconfigured,
    /// The feature is switched off in configuration.
    Disabled,
    /// The socket, TLS handshake or deadline failed. No response bytes crossed the wire.
    Transport,
    /// The server answered with a status that is not 2xx.
    HttpStatus,
    /// The server answered 2xx with something that is not a usable chat completion.
    Protocol,
    /// The model answered, and the answer is not something this crate will act on.
    Model,
    /// The index could not be read.
    Store,
    /// A file could not be read or written.
    Io,
    /// The user's invocation was wrong.
    Usage,
}

impl ErrorKind {
    pub fn label(self) -> &'static str {
        match self {
            ErrorKind::Unconfigured => "not configured",
            ErrorKind::Disabled => "disabled",
            ErrorKind::Transport => "network",
            ErrorKind::HttpStatus => "endpoint",
            ErrorKind::Protocol => "protocol",
            ErrorKind::Model => "model output",
            ErrorKind::Store => "index",
            ErrorKind::Io => "file",
            ErrorKind::Usage => "usage",
        }
    }
}

/// A failure from this crate. Every `String` in it has already passed through [`redact`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AiError {
    Unconfigured(String),
    Disabled(String),
    Transport(String),
    HttpStatus { status: u16, body: String },
    Protocol(String),
    Model(String),
    Store(String),
    /// The path is stored already-rendered and already-redacted: `Faults::io` cannot know whether a
    /// directory someone typed into a config file contains their token, and neither can the printer.
    Io(String, String),
    Usage(String),
}

impl AiError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            AiError::Unconfigured(_) => ErrorKind::Unconfigured,
            AiError::Disabled(_) => ErrorKind::Disabled,
            AiError::Transport(_) => ErrorKind::Transport,
            AiError::HttpStatus { .. } => ErrorKind::HttpStatus,
            AiError::Protocol(_) => ErrorKind::Protocol,
            AiError::Model(_) => ErrorKind::Model,
            AiError::Store(_) => ErrorKind::Store,
            AiError::Io(_, _) => ErrorKind::Io,
            AiError::Usage(_) => ErrorKind::Usage,
        }
    }

    /// `kind — detail`, the form the CLI prints and tests assert on.
    pub fn message(&self) -> String {
        let detail = match self {
            AiError::Unconfigured(m)
            | AiError::Disabled(m)
            | AiError::Transport(m)
            | AiError::Protocol(m)
            | AiError::Model(m)
            | AiError::Store(m)
            | AiError::Usage(m) => m.clone(),
            // The body is included because that is where an endpoint actually explains itself
            // ("insufficient_quota", "model not found") — and it is also the only field this crate
            // copies verbatim from a remote host. Both facts matter, hence the clip and the redaction
            // applied at construction.
            AiError::HttpStatus { status, body } => format!("HTTP {status}: {body}"),
            AiError::Io(path, m) => format!("{path}: {m}"),
        };
        format!("{} — {detail}", self.kind().label())
    }

    /// The endpoint is **busy**: it took the connection, answered, and asked to be called later.
    ///
    /// Exactly `429` (rate limited) and `503` (temporarily unavailable), read off the status number the
    /// transport already carries in [`AiError::HttpStatus`] — the classification needs no new field and
    /// no look at the body. Matching on prose instead would decide that a gateway's `insufficient_quota`,
    /// another's `服务繁忙` and a captive portal's HTML are all "busy", and the pass would slow itself
    /// for a rate limit that does not exist. A refusal that *means* never (401, 402, 404) is the opposite
    /// of busy and must not be paced down on someone's way to fixing their key.
    pub fn is_busy(&self) -> bool {
        matches!(self, AiError::HttpStatus { status: 429 | 503, .. })
    }

    /// Nothing came back: the address did not resolve, the socket died, or a deadline expired.
    ///
    /// [`ErrorKind::Transport`]'s own definition says it — "no response bytes crossed the wire" — so this
    /// is the one failure shape that may count toward a *silent* wave, and everything that answered at all
    /// (a 500, a body that is not JSON, an empty answer) is excluded. A slow queue that eventually replies
    /// is not an endpoint that has stopped replying, and the give-up rule in `summarize` is only about the
    /// second one.
    pub fn is_silent(&self) -> bool {
        self.kind() == ErrorKind::Transport
    }
}

impl std::fmt::Display for AiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for AiError {}

/// Remove every occurrence of the bearer token from `text`, in the two forms a server could echo it
/// back in: as written, and percent-encoded.
///
/// The encoded variant is not speculation. Gateways that reflect a request into their own error body
/// frequently re-encode it, and a filter that misses one form produces a message whose comment claims
/// it is safe and whose text is not. The encoder is deliberately *more* aggressive than RFC 3986 — it
/// escapes every non-alphanumeric byte, including `-`, `_` and `.`, which a strict encoder leaves
/// alone — because the direction of the mistake matters: over-escaping only ever removes text that was
/// already the secret, while under-escaping leaks it.
pub fn redact(text: &str, secret: &str) -> String {
    if secret.trim().is_empty() {
        // Substituting an empty needle would rewrite every position in the string.
        return text.to_string();
    }
    let mut out = replace_all(text, secret, REDACTED);
    let encoded = percent_encode(secret);
    if encoded != secret {
        out = replace_all(&out, &encoded, REDACTED);
        // Hex digits in a percent-escape are case-free, and the RFC only *recommends* uppercase.
        // A gateway that echoes the Authorization header back lower-cased (`%2b` for `+`) would
        // otherwise slip through a filter whose own documentation claims both forms are covered —
        // and the leak lands in cache/logs, where nobody looks until the key is already burned.
        let lowered = encoded.to_ascii_lowercase();
        if lowered != encoded {
            out = replace_all(&out, &lowered, REDACTED);
        }
    }
    out
}

fn replace_all(haystack: &str, needle: &str, with: &str) -> String {
    let mut out = String::with_capacity(haystack.len());
    let mut rest = haystack;
    while let Some(at) = rest.find(needle) {
        out.push_str(&rest[..at]);
        out.push_str(with);
        rest = &rest[at + needle.len()..];
    }
    out.push_str(rest);
    out
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' => out.push(*byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Clips a remote body to something a terminal can show without the useful part scrolling away.
///
/// `str::floor_char_boundary` is unstable, and slicing at a raw byte index panics — on a Chinese
/// error message from a Chinese-hosted gateway, which is a normal Friday for this product.
fn clip(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes total)", &text[..end], text.len())
}

/// The only thing allowed to build an [`AiError`] that carries outside text.
///
/// Holding the key here rather than threading it to every `format!` is what makes the redaction
/// un-bypassable: it is impossible to construct the error without going through a method that has
/// already scrubbed the string.
#[derive(Clone, Debug)]
pub struct Faults {
    key: SecretKey,
}

/// A body over this many bytes is clipped before it can reach a terminal.
const BODY_LIMIT: usize = 2000;

impl Faults {
    pub fn new(key: &SecretKey) -> Faults {
        Faults { key: key.clone() }
    }

    /// For the code that runs before a key is even known — configuration loading, argument parsing.
    /// Redaction is still applied; it simply has nothing to remove.
    pub fn anonymous() -> Faults {
        Faults { key: SecretKey::new("") }
    }

    fn clean(&self, text: &str) -> String {
        redact(text, self.key.expose())
    }

    pub fn unconfigured(&self, what: impl Into<String>) -> AiError {
        AiError::Unconfigured(self.clean(&what.into()))
    }

    pub fn disabled(&self, what: impl Into<String>) -> AiError {
        AiError::Disabled(self.clean(&what.into()))
    }

    pub fn usage(&self, what: impl Into<String>) -> AiError {
        AiError::Usage(self.clean(&what.into()))
    }

    pub fn transport(&self, reason: impl std::fmt::Display) -> AiError {
        AiError::Transport(self.clean(&reason.to_string()))
    }

    /// An HTTP status plus whatever the endpoint chose to say. `body` is bytes because the transport
    /// hands back raw bytes and a non-UTF-8 error page must still be reportable.
    pub fn http(&self, status: u16, body: &[u8]) -> AiError {
        let text = String::from_utf8_lossy(body);
        AiError::HttpStatus { status, body: self.clean(&clip(&text, BODY_LIMIT)) }
    }

    pub fn protocol(&self, reason: impl Into<String>) -> AiError {
        AiError::Protocol(self.clean(&reason.into()))
    }

    /// The model said something we refuse to act on. The offending text is quoted back because the
    /// user's only recourse is to rephrase, but it is clipped first: a model that emits a 500 kB
    /// refusal should not be able to fill a console.
    pub fn model(&self, reason: impl Into<String>) -> AiError {
        AiError::Model(self.clean(&clip(&reason.into(), BODY_LIMIT)))
    }

    pub fn store(&self, reason: impl std::fmt::Debug) -> AiError {
        AiError::Store(self.clean(&format!("{reason:?}")))
    }

    pub fn io(&self, path: impl AsRef<Path>, reason: impl std::fmt::Display) -> AiError {
        let path = path.as_ref().display().to_string();
        AiError::Io(self.clean(&path), self.clean(&reason.to_string()))
    }

    pub fn json(&self, path: impl AsRef<Path>, reason: impl std::fmt::Display) -> AiError {
        self.io(path, format!("not valid JSON: {reason}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "sk-SECRETEXAMPLE1234567890abcdefghij";

    fn faults() -> Faults {
        Faults::new(&SecretKey::new(TOKEN))
    }

    /// The requirement, stated as the spec states it: after rendering, the literal key string must
    /// not be present — for any variant, including the one that copies a remote body verbatim.
    #[test]
    fn the_literal_key_is_absent_from_every_rendered_error() {
        let reflected = format!(r#"{{"error":{{"message":"bad auth for {TOKEN}"}}}}"#);
        let cases = vec![
            faults().http(401, reflected.as_bytes()),
            faults().transport(format!("dial {TOKEN} failed")),
            faults().protocol(format!("no choices in {reflected}")),
            faults().model(format!("rejected {TOKEN}")),
            faults().store(format!("sqlite near {TOKEN}")),
            faults().io(Path::new(TOKEN), "denied"),
            faults().unconfigured("open_ai_api_key"),
            faults().disabled("enable_ai_extract_tag"),
            faults().usage(format!("expected --root, saw {TOKEN}")),
        ];
        for case in cases {
            let rendered = case.message();
            assert!(!rendered.contains(TOKEN), "leaked from {:?}", case.kind());
            assert!(!format!("{case:?}").contains(TOKEN), "Debug leaked {:?}", case.kind());
            assert!(!case.to_string().contains(TOKEN), "Display leaked {:?}", case.kind());
            assert!(rendered.starts_with(case.kind().label()), "{rendered}");
        }
    }

    /// A gateway that reflects a query parameter back in its error body usually re-encodes it, and one
    /// substitution that misses the encoded spelling leaves the token in a message whose code comment
    /// says it was scrubbed. Both hex cases appear in the wild.
    #[test]
    fn the_percent_encoded_reflection_is_removed_in_either_hex_case() {
        let key = SecretKey::new("sk+ab/cd=1");
        let faults = Faults::new(&key);
        let message = faults
            .http(400, b"rejected sk%2Bab%2Fcd%3D1 and also sk%2bab%2fcd%3d1".as_slice())
            .message();
        assert!(!message.contains("sk%2Bab%2Fcd%3D1"), "{message}");
        assert!(!message.contains("sk%2bab%2fcd%3d1"), "{message}");
        assert!(!message.contains("sk+ab/cd=1"), "{message}");
        // Two placeholders, not three: this message carries the key only in its two encoded forms,
        // so the plain form above is asserted absent rather than replaced. Counting the assertions
        // instead of the occurrences is what made this read as a leak.
        assert_eq!(message.matches(REDACTED).count(), 2, "{message}");
    }

    #[test]
    fn the_key_is_still_usable_after_all_this() {
        let key = SecretKey::new(TOKEN);
        assert_eq!(key.expose(), TOKEN);
        assert!(format!("{key:?}").contains(REDACTED));
        assert!(!format!("{key:?}").contains(TOKEN), "Debug must not print the value");
    }

    #[test]
    fn an_empty_secret_cannot_swallow_a_message() {
        assert_eq!(redact("plain text", ""), "plain text");
        assert_eq!(redact("plain text", "   "), "plain text");
    }

    #[test]
    fn anonymous_faults_report_unchanged_text() {
        let error = Faults::anonymous().http(503, b"gateway down".as_slice());
        assert_eq!(error.message(), "endpoint — HTTP 503: gateway down");
    }

    #[test]
    fn a_multibyte_body_is_clipped_on_a_char_boundary() {
        let body = "服务器错误".repeat(1000);
        let error = faults().http(500, body.as_bytes());
        let rendered = error.message();
        assert!(rendered.contains("bytes total"), "{rendered}");
        assert!(rendered.len() < BODY_LIMIT + 200);
        assert!(!rendered.contains('\u{FFFD}'), "the clip must not split a character");
    }

    #[test]
    fn busy_and_silent_are_told_apart_by_the_number_and_by_nothing_else() {
        // The pass paces itself on `is_busy` and closes the leg on `is_silent`, so both predicates are
        // load-bearing and both have to refuse the shapes that merely look similar.
        let f = Faults::anonymous();
        for status in [429u16, 503] {
            let busy = f.http(status, b"slow down".as_slice());
            assert!(busy.is_busy(), "{status} is the family's spelling of busy");
            assert!(!busy.is_silent(), "and it did answer, so it is not silence");
        }
        for status in [400, 401, 402, 404, 500, 502] {
            let answered = f.http(status, b"no".as_slice());
            assert!(!answered.is_busy(), "{status} is a refusal or a fault, not a rate limit");
            assert!(!answered.is_silent(), "{status} came back over the wire");
        }
        assert!(f.transport("deadline").is_silent(), "nothing answered");
        assert!(!f.transport("deadline").is_busy(), "and nothing was said about being busy");
        assert!(!f.protocol("not JSON").is_silent(), "a reply that makes no sense is still a reply");
    }

    #[test]
    fn kinds_have_distinct_labels_for_the_cli_prefix() {
        let f = Faults::anonymous();
        let labels = [
            f.unconfigured("a").kind(),
            f.disabled("a").kind(),
            f.transport("a").kind(),
            f.http(1, b"a").kind(),
            f.protocol("a").kind(),
            f.model("a").kind(),
            f.store("a").kind(),
            f.io("a", "b").kind(),
            f.usage("a").kind(),
        ];
        let unique: std::collections::BTreeSet<_> = labels.iter().copied().collect();
        assert_eq!(labels.len(), unique.len(), "{labels:?}");
    }
}
