//! WinHTTP, declared by hand.
//!
//! This is the transport decision, and it is the same one `windcap/core` already made for GDI and
//! user32: raw `extern "system"` declarations instead of a bindings crate or an HTTP crate.
//!
//! The reasoning is specific to this workspace. `Cargo.lock` contains no HTTP client at all — no
//! reqwest, no ureq, no hyper, no rustls — because nothing in the recorder has ever needed one.
//! Adding one would mean either vendoring a transitive tree (a tokio/openssl-scale download for a
//! `POST`) or adding a single-crate dependency whose presence in the cargo cache becomes an
//! undeclared build prerequisite for every future `cargo build --offline`. WinHTTP ships with the
//! OS and its import library is in the Windows SDK that is already required to link, so the
//! dependency cost is exactly zero and the offline property is untouched.
//!
//! The part people assume you have to write yourself — certificate validation, chain building,
//! trust anchors, TLS version negotiation — is not implemented here and must never be. WinHTTP
//! performs it through Schannel against the *machine's* certificate store, which is the same trust
//! boundary a browser uses. That is delegation, not hand-rolled crypto: a revoked or self-signed
//! chain fails the call with `ERROR_WINHTTP_INSECURE_SERVER_CERT` and this crate surfaces that as
//! an error, because an HTTPS client that silently trusts everything would leak the user's API key
//! to whoever is on the network.

#![cfg(windows)]

use core::ffi::{c_int, c_void};

pub type HINTERNET = *mut c_void;
pub type BOOL = i32;
pub type DWORD = u32;
pub type LPCWSTR = *const u16;
#[allow(non_camel_case_types)]
pub type INTERNET_PORT = u16;

pub const TRUE: BOOL = 1;

/// `WINHTTP_ACCESS_TYPE_DEFAULT_PROXY`: honour whatever proxy the user's session is configured for.
/// A corporate laptop reaching a hosted endpoint needs this; it is also why loopback sessions are
/// opened with `NO_PROXY` instead (see `http::session_for`).
pub const ACCESS_TYPE_DEFAULT_PROXY: DWORD = 0;
/// `WINHTTP_ACCESS_TYPE_NO_PROXY`.
pub const ACCESS_TYPE_NO_PROXY: DWORD = 1;
/// `WINHTTP_FLAG_SECURE` on the request handle: run this connection over TLS.
///
/// `0x00800000` — bit 23 — and not `0x08000000`, which is bit 27 and belongs to no request flag at all.
/// The direction of this mistake is the one that matters: the wrong bit leaves the secure flag *clear*, so
/// an `https://` `open_ai_base_url` is cracked as secure, connects to port 443, and sends a plaintext
/// `POST` with the bearer key in a header, past every hop between here and the endpoint. A refused request
/// would have been the better failure. Nothing in the loopback suite can observe this, because a loopback
/// test endpoint is plain HTTP by design — which is why the value is pinned as a literal below.
pub const FLAG_SECURE: DWORD = 0x0080_0000;

/// `WINHTTP_OPTION_SECURE_PROTOCOLS`. It is 84, not 8: with 8 the protocol mask is written to an
/// unrelated knob (or the call is refused, which `http::post` reports as "cannot require TLS 1.2+"), and
/// the session then negotiates whatever the machine default allows — including the versions the mask
/// above exists to exclude.
pub const OPTION_SECURE_PROTOCOLS: DWORD = 84;
const FLAG_TLS12: DWORD = 0x0000_0800;
const FLAG_TLS13: DWORD = 0x0000_2000;
/// TLS 1.0 and 1.1 are excluded on purpose — including `WINHTTP_FLAG_SECURE_PROTOCOL_TLS1_1` here would
/// contradict the sentence above it, and the mistake is silent: the request still succeeds, over a
/// protocol a downgrade-in-the-middle can force, carrying the user's bearer key. Only 1.2 and 1.3 speak.
pub const SECURE_PROTOCOLS_MODERN: DWORD = FLAG_TLS12 | FLAG_TLS13;

/// `WINHTTP_ADDREQ_FLAG_REPLACE | WINHTTP_ADDREQ_FLAG_ADD`: set the header, adding it if absent.
pub const ADDREQ_REPLACE_OR_ADD: DWORD = 0x8000_0000 | 0x2000_0000;

/// `WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER`.
///
/// `19 | 0x20000000`, and every digit of that matters. `WINHTTP_QUERY_STATUS_CODE` is 19, not 5 (5 is
/// `WINHTTP_QUERY_CONTENT_LENGTH`), and `WINHTTP_QUERY_FLAG_NUMBER` is `0x20000000`, not `0x2000`.
/// Combining the wrong pair is not a compile error either: `WinHttpQueryHeaders` refuses the unknown
/// flag with `ERROR_INVALID_PARAMETER` *after* a perfectly good reply has been received, which reads
/// like a malformed response from the server and is nothing of the kind. Pinned by
/// `the_query_levels_are_the_header_values` below.
pub const QUERY_STATUS_CODE_NUMBER: DWORD = 19 | 0x2000_0000;
/// `INTERNET_SCHEME_HTTPS`. `INTERNET_SCHEME_HTTP` is 1; the values arrive from `WinHttpCrackUrl`.
pub const SCHEME_HTTPS: DWORD = 2;

/// `URL_COMPONENTS`, field-for-field and in order.
///
/// `nScheme` is a C `enum`, which MSVC lays out as a 4-byte signed integer, and `nPort` is a
/// `WORD`. Getting either wrong shifts every field after it, so the layout is pinned by the
/// `struct_size_matches_the_header_layout` test rather than trusted.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct UrlComponents {
    pub header_info_size: DWORD,
    pub scheme: LPCWSTR,
    pub scheme_length: DWORD,
    pub scheme_kind: DWORD,
    pub host_name: LPCWSTR,
    pub host_name_length: DWORD,
    pub port: INTERNET_PORT,
    // Two bytes of tail padding follow `port` so the next pointer lands on 8.
    pub user_name: LPCWSTR,
    pub user_name_length: DWORD,
    pub password: LPCWSTR,
    pub password_length: DWORD,
    pub url_path: LPCWSTR,
    pub url_path_length: DWORD,
    pub extra_info: LPCWSTR,
    pub extra_info_length: DWORD,
}

impl UrlComponents {
    pub fn new() -> UrlComponents {
        UrlComponents {
            header_info_size: core::mem::size_of::<UrlComponents>() as DWORD,
            scheme: core::ptr::null(),
            scheme_length: 0,
            scheme_kind: 0,
            host_name: core::ptr::null(),
            host_name_length: 0,
            port: 0,
            user_name: core::ptr::null(),
            user_name_length: 0,
            password: core::ptr::null(),
            password_length: 0,
            url_path: core::ptr::null(),
            url_path_length: 0,
            extra_info: core::ptr::null(),
            extra_info_length: 0,
        }
    }
}

#[link(name = "winhttp")]
extern "system" {
    pub fn WinHttpOpen(
        user_agent: LPCWSTR,
        access_type: DWORD,
        proxy_name: LPCWSTR,
        proxy_bypass: LPCWSTR,
        flags: DWORD,
    ) -> HINTERNET;
    pub fn WinHttpConnect(
        session: HINTERNET,
        server: LPCWSTR,
        port: INTERNET_PORT,
        reserved: DWORD,
    ) -> HINTERNET;
    pub fn WinHttpOpenRequest(
        connection: HINTERNET,
        verb: LPCWSTR,
        object_name: LPCWSTR,
        version: LPCWSTR,
        referrer: LPCWSTR,
        accept_types: *const *const u16,
        flags: DWORD,
    ) -> HINTERNET;
    pub fn WinHttpAddRequestHeaders(request: HINTERNET, headers: LPCWSTR, length: DWORD, modifier: DWORD) -> BOOL;
    pub fn WinHttpSetOption(handle: HINTERNET, option: DWORD, information: *const c_void, length: DWORD) -> BOOL;
    pub fn WinHttpSetTimeouts(
        handle: HINTERNET,
        resolve_ms: c_int,
        connect_ms: c_int,
        send_ms: c_int,
        receive_ms: c_int,
    ) -> BOOL;
    pub fn WinHttpSendRequest(
        request: HINTERNET,
        headers: LPCWSTR,
        headers_length: DWORD,
        optional: *const c_void,
        optional_length: DWORD,
        total_length: DWORD,
        context: usize,
    ) -> BOOL;
    pub fn WinHttpReceiveResponse(request: HINTERNET, reserved: *mut c_void) -> BOOL;
    pub fn WinHttpQueryHeaders(
        request: HINTERNET,
        info_level: DWORD,
        name: LPCWSTR,
        buffer: *mut c_void,
        buffer_length: *mut DWORD,
        index: *mut DWORD,
    ) -> BOOL;
    pub fn WinHttpReadData(request: HINTERNET, buffer: *mut c_void, to_read: DWORD, read: *mut DWORD) -> BOOL;
    pub fn WinHttpCrackUrl(url: LPCWSTR, reserved: DWORD, scheme_type: DWORD, components: *mut UrlComponents) -> BOOL;
    pub fn WinHttpCloseHandle(handle: HINTERNET) -> BOOL;
    pub fn GetLastError() -> DWORD;
}

/// Last-resort error text for a failed call. WinHTTP reports only the failure, not why, so
/// `GetLastError` is the difference between "the request failed" and "TLS handshake refused".
pub fn last_error() -> String {
    format!("WinHTTP error 0x{:08X}", unsafe { GetLastError() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    /// A wrong `nScheme` width shifts `host_name` and everything after it, and the symptom is a
    /// garbled hostname rather than a compiler error. Pin the layout instead.
    #[test]
    fn struct_size_matches_the_header_layout() {
        assert_eq!(size_of::<UrlComponents>(), 104, "x64 URL_COMPONENTS is 104 bytes");
        assert_eq!(offset_of!(UrlComponents, header_info_size), 0);
        assert_eq!(offset_of!(UrlComponents, scheme), 8);
        assert_eq!(offset_of!(UrlComponents, scheme_length), 16);
        assert_eq!(offset_of!(UrlComponents, scheme_kind), 20);
        assert_eq!(offset_of!(UrlComponents, host_name), 24);
        assert_eq!(offset_of!(UrlComponents, host_name_length), 32);
        assert_eq!(offset_of!(UrlComponents, port), 36);
        assert_eq!(offset_of!(UrlComponents, user_name), 40);
        assert_eq!(offset_of!(UrlComponents, url_path), 72);
        assert_eq!(offset_of!(UrlComponents, extra_info), 88);
    }

    #[test]
    fn a_zero_sized_struct_would_be_caught_by_the_constructor() {
        // `header_info_size` is the field WinHTTP itself validates; if it lies, CrackUrl fails
        // with ERROR_WINHTTP_INCORRECT_HANDLE_TYPE and nothing points at the cause.
        assert_eq!(UrlComponents::new().header_info_size as usize, size_of::<UrlComponents>());
    }

    /// Every constant above, re-checked as a literal.
    ///
    /// This crate's whole `extern "system"` block is a hand transcription of `winhttp.h`, and a typo in
    /// one of these is invisible to the compiler *and* to the loopback tests: `WinHttpQueryHeaders` takes
    /// a `DWORD`, so `0x2000 | 5` compiles, is accepted by the ABI, and comes back as
    /// `ERROR_INVALID_PARAMETER` at run time — which the client then reports as "the reply carried no
    /// status code", i.e. blames the server. The real values below are copied from
    /// `Windows Kits/10/Include/um/winhttp.h`; if one ever disagrees with this list, this test is where
    /// the disagreement surfaces, rather than in every test that waits for an HTTP reply.
    #[test]
    fn the_query_levels_are_the_header_values() {
        assert_eq!(QUERY_STATUS_CODE_NUMBER, 0x2000_0000 | 19, "FLAG_NUMBER | STATUS_CODE");
        assert_eq!(FLAG_SECURE, 0x0080_0000, "WINHTTP_FLAG_SECURE is bit 23");
        assert_eq!(OPTION_SECURE_PROTOCOLS, 84);
        assert_eq!(ADDREQ_REPLACE_OR_ADD, 0x8000_0000 | 0x2000_0000);
        assert_eq!((ACCESS_TYPE_DEFAULT_PROXY, ACCESS_TYPE_NO_PROXY), (0, 1));
        assert_eq!(SCHEME_HTTPS, 2, "INTERNET_SCHEME_HTTPS; HTTP is 1, and `http::crack` reads both");
        // TLS 1.0/1.1 must stay out of the mask, and the mask is the only thing keeping them out.
        assert_eq!(SECURE_PROTOCOLS_MODERN, 0x0000_0800 | 0x0000_2000, "TLS 1.2 and 1.3 only");
        assert_eq!(SECURE_PROTOCOLS_MODERN & 0x0000_0080, 0, "TLS 1.0 must not be offered");
        assert_eq!(SECURE_PROTOCOLS_MODERN & 0x0000_0200, 0, "TLS 1.1 must not be offered");
    }
}
