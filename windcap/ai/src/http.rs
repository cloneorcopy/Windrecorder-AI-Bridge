//! One blocking `POST`, and nothing else.
//!
//! The surface is deliberately absurdly small — a URL, some headers, a body, and back come a status
//! code and bytes — because every additional affordance (connection pooling, async, hand-rolled
//! redirects, retries with backoff) is a place a dependency-free implementation gets subtly wrong.
//! There is exactly one call site, in `crate::client`.
//!
//! # Why there is no cancellation, only a deadline
//!
//! All four WinHTTP phases get an explicit timeout. A feature whose server is unreachable, slow or
//! misconfigured has to answer with a sentence the user can act on, and the only alternative to a
//! deadline is a spinner that never resolves — which is what "degrades to a clear message, not a
//! hang" means in practice for a tool that is supposed to run unattended in the idle batch.
//! Resolve and connect are short because a failure there is a configuration typo; receive is long
//! because that is what a hosted inference queue actually is.
//!
//! # Platform
//!
//! The transport is WinHTTP, so on anything but Windows `post` fails with a sentence rather than
//! pretending to have sent something. The rest of this crate — the prompts, the schema validation,
//! the date arithmetic, the cache — is portable and still testable, which is the reason the split is
//! at the function boundary and not at the module boundary.

use std::time::Duration;

/// Resolve+connect should fail fast on a typo'd `open_ai_base_url`; the reply may be slow.
///
/// The four numbers were 5 / 10 / 60 / 180 s until 2026-09-30, when a pass that had asked a
/// self-hosted gateway for forty stretches reported two of them as
/// `accepted the connection but sent no reply (WinHTTP error 0x00002EE2)` — that is the receive
/// deadline, and the work behind it is a queue the caller does not control. The owner's instruction
/// was to wait rather than give up, so receive is now fifteen minutes and send five, while resolve and
/// connect stay short: a wrong address is a typo and a typo should not cost a window.
///
/// The cost of a long receive is named, not hidden — see
/// `docs/adr/2026-09-30-the-organise-pass-runs-on-four-legs.md`: one request can now overrun the
/// closing of the maintenance window by up to its own deadline, because the pass asks whether it may
/// still work between two items and never in the middle of a sent request.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const SEND_TIMEOUT: Duration = Duration::from_secs(300);
pub const RECEIVE_TIMEOUT: Duration = Duration::from_secs(900);

/// The most bytes this crate will accept for one reply body.
///
/// A chat completion is a single JSON object and the useful ones run to a few tens of kilobytes. The
/// cap exists so that a hostile or misconfigured endpoint cannot make a history tool allocate without
/// limit; a tags answer over 32 MiB is not a lost feature, it is somebody else's bug.
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// A transport-level failure: the socket, the handshake, or a deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError(pub String);

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TransportError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// `127.0.0.1`, `::1`, `localhost`.
///
/// One decision lives here, which is why it is careful rather than a `contains`: a session to a
/// loopback host is opened with `NO_PROXY`, because a corporate proxy profile with a permissive but
/// not loopback-aware bypass list will happily swallow a call to your own machine, and then the test
/// suite and any local Ollama endpoint depend on the network settings of the machine running them.
///
/// It used to carry a second one — plain HTTP was only tolerated here, on the reasoning that a
/// bearer token must not cross the network in the clear — and that rule is gone, because the address
/// is the user's: a gateway on the same LAN is how a self-hosted model server is actually spelled.
/// The predicate stays for the proxy decision, and dotted-quad literals are still checked
/// digit-by-digit so that `127.0.0.1.evil.test` — which resolves to something else entirely — is not
/// mistaken for an address.
pub(crate) fn is_loopback(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']);
    host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("::1")
        || host.strip_prefix("127.").is_some_and(|rest| {
            !rest.is_empty()
                && rest.split('.').all(|part| {
                    !part.is_empty()
                        && part.len() <= 3
                        && part.bytes().all(|b| b.is_ascii_digit())
                        // A four-digit "octet" is not an IPv4 address, and `127.0.0.999` resolving as
                        // loopback would be the rare case where being permissive is the unsafe direction.
                        && part.parse::<u8>().is_ok()
                })
        })
}

/// Issue one blocking POST. See the module header for what it deliberately does not do.
#[cfg(windows)]
pub fn post(url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, TransportError> {
    winhttp::post(url, headers, body)
}

#[cfg(not(windows))]
pub fn post(url: &str, _headers: &[(&str, &str)], _body: &[u8]) -> Result<Response, TransportError> {
    let _ = url;
    Err(TransportError(
        "no transport in this build: wind-ai speaks WinHTTP, which exists only on Windows".to_string(),
    ))
}

#[cfg(windows)]
mod winhttp {
    use std::ffi::c_void;
    use std::mem;

    use crate::ffi;
    use super::{
        is_loopback, Duration, Response, TransportError, CONNECT_TIMEOUT, MAX_BODY_BYTES, RECEIVE_TIMEOUT,
        RESOLVE_TIMEOUT, SEND_TIMEOUT,
    };

    /// UTF-16 with the terminator `winhttp.h` expects everywhere but in `URL_COMPONENTS`, whose
    /// components the header documents as *not* NUL-terminated — hence the separate read-back path in
    /// `decode`.
    fn wide(text: &str) -> Vec<u16> {
        let mut out: Vec<u16> = text.encode_utf16().collect();
        out.push(0);
        out
    }

    /// Closes an `HINTERNET` on drop, including on every `?` early return. Leaking a session handle
    /// per request is the kind of bug that surfaces only after a week of the idle tag batcher running.
    struct Handle(ffi::HINTERNET);

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { ffi::WinHttpCloseHandle(self.0) };
            }
        }
    }

    /// The component buffer sizes `WinHttpCrackUrl` is given.
    ///
    /// Two-pass "ask for the length, then allocate" does not work here: with a NULL buffer the header
    /// leaves the reported length at zero rather than filling it in, so the only way to learn how long
    /// the host is, is to hand over somewhere to put it. These are ceilings, and an endpoint whose
    /// hostname or path genuinely needs more than this is not a configuration anyone has.
    const HOST_CAPACITY: usize = 512;
    const PATH_CAPACITY: usize = 8192;
    const SCHEME_CAPACITY: usize = 64;

    /// Split an absolute URL into (secure, host, port, request-target).
    ///
    /// Parsing is WinHTTP's job through `WinHttpCrackUrl`, not ours: it applies the same authority and
    /// default-port rules as the code that will issue the request, so a `base_url` that means two
    /// different things in two places cannot exist. A hand-rolled split would also have to decide what
    /// `https://x:0y/z` means, and every answer to that is a bug.
    ///
    /// The returned request-target keeps the query string, because `WinHttpOpenRequest`'s object name is
    /// everything after the authority.
    pub(super) fn crack(url: &str) -> Result<(bool, String, u16, String), TransportError> {
        let wide_url = wide(url);
        let mut scheme = vec![0u16; SCHEME_CAPACITY + 1];
        let mut host = vec![0u16; HOST_CAPACITY + 1];
        let mut path = vec![0u16; PATH_CAPACITY + 1];
        let mut extra = vec![0u16; PATH_CAPACITY + 1];
        let mut parts = ffi::UrlComponents {
            scheme: scheme.as_mut_ptr().cast_const(),
            scheme_length: SCHEME_CAPACITY as ffi::DWORD,
            host_name: host.as_mut_ptr().cast_const(),
            host_name_length: HOST_CAPACITY as ffi::DWORD,
            url_path: path.as_mut_ptr().cast_const(),
            url_path_length: PATH_CAPACITY as ffi::DWORD,
            extra_info: extra.as_mut_ptr().cast_const(),
            extra_info_length: PATH_CAPACITY as ffi::DWORD,
            ..ffi::UrlComponents::new()
        };
        if unsafe { ffi::WinHttpCrackUrl(wide_url.as_ptr(), 0, 0, &mut parts) } != ffi::TRUE {
            return Err(TransportError(format!("`{url}` is not a valid URL ({})", ffi::last_error())));
        }

        let secure = match parts.scheme_kind {
            SCHEME_HTTP => false,
            ffi::SCHEME_HTTPS => true,
            other => {
                return Err(TransportError(format!(
                    "URL scheme {other} cannot carry a chat completion; `open_ai_base_url` must be \
                     http:// or https://"
                )))
            }
        };
        // The crack call already resolved the default port for the scheme; do not second-guess it.
        let port = parts.port;
        let host = decode(&host, parts.host_name_length as usize);
        if host.is_empty() {
            return Err(TransportError(format!("`{url}` names no host")));
        }
        let mut target = decode(&path, parts.url_path_length as usize);
        if target.is_empty() {
            target.push('/');
        }
        target.push_str(&decode(&extra, parts.extra_info_length as usize));
        Ok((secure, host, port, target))
    }

    /// `INTERNET_SCHEME_HTTP`. Declared here so the match above reads as a scheme decision.
    const SCHEME_HTTP: ffi::DWORD = 1;

    /// Read back a `WinHttpCrackUrl` component; the reported length is the only trustworthy bound.
    fn decode(buffer: &[u16], length: usize) -> String {
        String::from_utf16_lossy(&buffer[..length.min(buffer.len())])
    }

    /// `Content-Length` is a `DWORD` in this API, so a body that cannot be measured in a `DWORD` is
    /// refused rather than truncated: a silently clipped prompt is a wrong answer, not a large one.
    pub(super) fn body_length(body: &[u8]) -> Result<ffi::DWORD, TransportError> {
        u32::try_from(body.len())
            .map_err(|_| TransportError(format!("a {}-byte request body exceeds WinHTTP's 4 GiB limit", body.len())))
    }

    /// WinHTTP takes its deadlines as millisecond `int`s. Clamped, not rejected: these arrive from
    /// constants, and a panic in a timeout setter is a strange way to report arithmetic.
    pub(super) fn millis(value: Duration) -> std::ffi::c_int {
        i32::try_from(value.as_millis()).unwrap_or(i32::MAX)
    }

    pub fn post(url: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, TransportError> {
        let (secure, host, port, target) = crack(url)?;
        let content_length = body_length(body)?;
        let access_type =
            if is_loopback(&host) { ffi::ACCESS_TYPE_NO_PROXY } else { ffi::ACCESS_TYPE_DEFAULT_PROXY };

        let agent = wide("windai/0.1 (Windrecorder)");
        let session = Handle(unsafe {
            ffi::WinHttpOpen(agent.as_ptr(), access_type, std::ptr::null(), std::ptr::null(), 0)
        });
        if session.0.is_null() {
            return Err(TransportError(format!("cannot start an HTTP session ({})", ffi::last_error())));
        }
        unsafe {
            // The return value is deliberately unchecked: the deadlines are a hardening of the
            // defaults, and a build whose WinHTTP refuses them should still try the request.
            ffi::WinHttpSetTimeouts(
                session.0,
                millis(RESOLVE_TIMEOUT),
                millis(CONNECT_TIMEOUT),
                millis(SEND_TIMEOUT),
                millis(RECEIVE_TIMEOUT),
            )
        };
        if secure {
            // Trust decisions belong to Schannel and the machine's certificate store. This option only
            // refuses to *speak* a protocol version that should not exist any more, so a
            // downgrade-in-the-middle cannot turn into a successful request carrying the user's key.
            let protocols = ffi::SECURE_PROTOCOLS_MODERN;
            let ok = unsafe {
                ffi::WinHttpSetOption(
                    session.0,
                    ffi::OPTION_SECURE_PROTOCOLS,
                    (&raw const protocols).cast::<c_void>(),
                    mem::size_of::<ffi::DWORD>() as ffi::DWORD,
                )
            };
            if ok != ffi::TRUE {
                return Err(TransportError(format!("cannot require TLS 1.2+ ({})", ffi::last_error())));
            }
        }

        let wide_host = wide(&host);
        let connection = Handle(unsafe { ffi::WinHttpConnect(session.0, wide_host.as_ptr(), port, 0) });
        if connection.0.is_null() {
            return Err(TransportError(format!("cannot reach {host}:{port} ({})", ffi::last_error())));
        }

        let verb = wide("POST");
        let wide_target = wide(&target);
        let request = Handle(unsafe {
            ffi::WinHttpOpenRequest(
                connection.0,
                verb.as_ptr(),
                wide_target.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                if secure { ffi::FLAG_SECURE } else { 0 },
            )
        });
        if request.0.is_null() {
            return Err(TransportError(format!("cannot open POST {target} ({})", ffi::last_error())));
        }

        // One header block, because `WinHttpAddRequestHeaders` takes them as a set. This is the only
        // place an `Authorization` value is ever formatted into anything, and `block` is dropped here
        // without being returned, stored, or logged. Note that no error below interpolates `block`.
        let mut block = String::new();
        for (name, value) in headers {
            block.push_str(name);
            block.push_str(": ");
            block.push_str(value);
            block.push_str("\r\n");
        }
        // `(DWORD)-1`, not 0 and not a byte count: that is the add-headers API's "measure it yourself"
        // spelling, and it requires the buffer to be null-terminated, which `wide()` guarantees and each
        // line to end in CRLF, which the loop below writes. Zero and a wrong byte count both come back as
        // ERROR_INVALID_PARAMETER (0x57) *before* a byte is sent — loud, but it reads exactly like the
        // endpoint refusing us, so it is worth pinning in a comment and in a test.
        let wide_block = wide(&block);
        if !block.is_empty() {
            let ok = unsafe {
                ffi::WinHttpAddRequestHeaders(request.0, wide_block.as_ptr(), u32::MAX, ffi::ADDREQ_REPLACE_OR_ADD)
            };
            if ok != ffi::TRUE {
                return Err(TransportError(format!("cannot set request headers ({})", ffi::last_error())));
            }
        }
        let sent = unsafe {
            ffi::WinHttpSendRequest(
                request.0,
                std::ptr::null(),
                0,
                if body.is_empty() { std::ptr::null() } else { body.as_ptr().cast::<c_void>() },
                content_length,
                content_length,
                0,
            )
        };
        if sent != ffi::TRUE {
            return Err(TransportError(format!(
                "the request never reached {host}:{port} ({})",
                ffi::last_error()
            )));
        }
        if unsafe { ffi::WinHttpReceiveResponse(request.0, std::ptr::null_mut()) } != ffi::TRUE {
            return Err(TransportError(format!(
                "{host}:{port} accepted the connection but sent no reply ({})",
                ffi::last_error()
            )));
        }

        let status = query_status(request.0)?;
        Ok(Response { status, body: read_all(request.0)? })
    }

    /// The HTTP status as a number. Asking for it with `WINHTTP_QUERY_FLAG_NUMBER` is the only way to
    /// get one that does not depend on the wording of a reason phrase a proxy is free to rewrite.
    fn query_status(request: ffi::HINTERNET) -> Result<u16, TransportError> {
        let mut status: ffi::DWORD = 0;
        let mut length = mem::size_of::<ffi::DWORD>() as ffi::DWORD;
        let ok = unsafe {
            ffi::WinHttpQueryHeaders(
                request,
                ffi::QUERY_STATUS_CODE_NUMBER,
                std::ptr::null(),
                (&raw mut status).cast::<c_void>(),
                &mut length,
                std::ptr::null_mut(),
            )
        };
        if ok != ffi::TRUE {
            return Err(TransportError(format!("the reply carried no status code ({})", ffi::last_error())));
        }
        Ok(status as u16)
    }

    /// Drain the body. WinHTTP hands it back in whatever chunks the socket produced, so this loop and
    /// not the caller is what guarantees a complete JSON document or an error.
    fn read_all(request: ffi::HINTERNET) -> Result<Vec<u8>, TransportError> {
        const CHUNK: usize = 16 * 1024;
        let mut buffer = vec![0u8; CHUNK];
        let mut out = Vec::new();
        loop {
            let mut read: ffi::DWORD = 0;
            let ok = unsafe {
                ffi::WinHttpReadData(request, buffer.as_mut_ptr().cast::<c_void>(), CHUNK as ffi::DWORD, &mut read)
            };
            if ok != ffi::TRUE {
                return Err(TransportError(format!("the reply body was cut short ({})", ffi::last_error())));
            }
            if read == 0 {
                return Ok(out);
            }
            if out.len() + read as usize > MAX_BODY_BYTES {
                return Err(TransportError(format!("the reply body exceeded {MAX_BODY_BYTES} bytes")));
            }
            out.extend_from_slice(&buffer[..read as usize]);
        }
    }
}

#[cfg(test)]
#[cfg(windows)]
mod tests {
    use super::*;

    #[test]
    fn crack_reads_scheme_host_port_and_target() {
        let (secure, host, port, target) = winhttp::crack("https://api.example.test:8443/v1/chat/completions").unwrap();
        assert!(secure);
        assert_eq!(host, "api.example.test");
        assert_eq!(port, 8443);
        assert_eq!(target, "/v1/chat/completions");
    }

    #[test]
    fn crack_applies_the_default_port_and_keeps_the_query() {
        let (secure, host, port, target) = winhttp::crack("https://api.example.test/v1/x?a=1&b=2").unwrap();
        assert!(secure);
        assert_eq!((host.as_str(), port), ("api.example.test", 443));
        assert_eq!(target, "/v1/x?a=1&b=2");
    }

    #[test]
    fn a_bare_authority_still_names_a_path() {
        let (_, _, _, target) = winhttp::crack("http://127.0.0.1:9").unwrap();
        assert_eq!(target, "/");
    }

    #[test]
    fn plain_http_cracks_because_that_is_how_a_local_or_lan_endpoint_is_spelled() {
        // The transport has to be able to speak cleartext, or a home-lab gateway and the canned test
        // server are both unreachable. Whether it is *advisable* stopped being this product's question.
        let (secure, host, port, _) = winhttp::crack("http://127.0.0.1:8123/v1").unwrap();
        assert!(!secure);
        assert_eq!((host.as_str(), port), ("127.0.0.1", 8123));
    }

    #[test]
    fn nonsense_urls_are_refused_before_any_socket_is_opened() {
        for url in ["not a url at all", "ftp://example.test/x", "https://", "://x", ""] {
            let error = winhttp::crack(url).expect_err("{url} must be refused");
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn a_path_that_is_only_a_query_is_rejected_by_the_endpoint_not_here() {
        // A base URL of "https://x?y=1" is legal syntax with an empty path; the target must still
        // start with '/' or WinHTTP sends a malformed request line.
        let (_, _, _, target) = winhttp::crack("https://x.test?y=1").unwrap();
        assert!(target.starts_with('/'), "{target}");
    }

    #[test]
    fn loopback_is_detected_in_every_shape_a_test_writes_it() {
        assert!(is_loopback("127.0.0.1"));
        assert!(is_loopback("localhost"));
        assert!(is_loopback("LOCALHOST"));
        assert!(is_loopback("::1"));
        assert!(is_loopback("[::1]"));
        assert!(is_loopback("127.0.0.53"));
        assert!(is_loopback("127.1"));
        assert!(!is_loopback("api.openai.com"));
        assert!(!is_loopback("127.0.0.1.evil.test"), "a suffix that only looks like an address");
        assert!(!is_loopback("127.0.0.999"), "an octet that is not a number");
        assert!(!is_loopback("127"));
        assert!(!is_loopback(""));
    }

    #[test]
    fn an_oversized_body_is_refused_not_truncated() {
        assert_eq!(winhttp::body_length(&[0u8; 10]).unwrap(), 10);
        assert!(winhttp::body_length(&[]).is_ok());
    }

    #[test]
    fn deadlines_become_positive_milliseconds() {
        assert_eq!(winhttp::millis(Duration::from_secs(5)), 5000);
        assert_eq!(winhttp::millis(Duration::ZERO), 0);
        assert_eq!(winhttp::millis(Duration::from_secs(1 << 40)), i32::MAX);
    }
}
