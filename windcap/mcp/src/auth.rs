//! The bearer gate, and the rules that decide whether a bind may open at all.
//!
//! The bridge used to be stdio-only, where the peer was by definition the process that spawned it.
//! A listening port has no such guarantee, so everything in this file exists to keep "whoever can
//! reach this socket" from becoming "whoever may read this person's screen history".
//!
//! Three rules, in order of how badly each breaks if they are ever relaxed:
//!
//!   1. the service is **off** unless the user says otherwise, and **loopback** unless they say
//!      otherwise twice;
//!   2. authentication may be switched off, but only on a loopback bind — off plus a network
//!      address would hand the whole record to every device on the segment, and this repository is
//!      distributed;
//!   3. a secret arrives in a request *header* or nowhere. Not in the query string, which lands in
//!      proxy logs, browser history and `netstat`-adjacent tooling; not in a command-line argument,
//!      which every process on the machine can enumerate; not in the environment, which is read by
//!      every child process spawned afterwards.

use std::net::IpAddr;

/// Shorter than this and a "token" is a guessable string, not a secret. Generated values are
/// longer, so this only ever catches a hand-typed one.
pub const TOKEN_MIN_CHARS: usize = 24;

/// The refusal type. `serve` turns one of these into exit status 2 and a message, *before* a
/// socket exists.
#[derive(Debug, PartialEq, Eq)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Is this host one that only this machine can reach?
pub fn is_loopback(host: &str) -> bool {
    match classify(host) {
        Some(address) => address.is_loopback(),
        // `localhost` is the one name that is not an address, and a hosts file could point it
        // elsewhere. It is still what a user types, and the risk of honouring it is a refused
        // connection, not a disclosure: the guard that matters is the bind itself.
        None => host.eq_ignore_ascii_case("localhost"),
    }
}

/// `0.0.0.0` / `::` — every interface, which means the LAN is reachable and that must be said out
/// loud rather than discovered later.
pub fn is_wildcard(host: &str) -> bool {
    match classify(host) {
        Some(address) => address.is_unspecified(),
        None => matches!(host, "0.0.0.0" | "::"),
    }
}

fn classify(host: &str) -> Option<IpAddr> {
    host.trim().parse::<IpAddr>().ok().or_else(|| {
        // `::1` and `0.0.0.0` are what the config holds; a user may paste `[::1]` from a URL.
        let stripped = host.trim().strip_prefix('[')?.strip_suffix(']')?;
        stripped.parse::<IpAddr>().ok()
    })
}

/// Validate the requested bind, and return the line to log about what was opened.
///
/// Raising here rather than starting open is the point: a half-configured resident server that
/// quietly listens without a secret is worse than one that refuses to start.
pub fn startup_guard(host: &str, port: i64, token: &str, auth_required: bool) -> Result<String, Refused> {
    if !(1..=65535).contains(&port) {
        return Err(Refused(format!("mcp_server_port must be between 1 and 65535, got {port}.")));
    }
    if auth_required && token.chars().count() < TOKEN_MIN_CHARS {
        return Err(Refused(format!(
            "mcp_server_token must be at least {TOKEN_MIN_CHARS} characters (got {}). Type one in \
             the window's AI page, under MCP bridge, or switch authentication off - which is only \
             allowed while mcp_server_host is a loopback address.",
            token.chars().count()
        )));
    }
    if !auth_required && !is_loopback(host) {
        return Err(Refused(format!(
            "authentication is off but mcp_server_host is {host}, which would let anything on that \
             network read the whole record without a secret. Set the host back to 127.0.0.1, or \
             turn the token requirement on."
        )));
    }
    let access = if auth_required { "bearer token required" } else { "no token, this machine only" };
    if is_loopback(host) {
        return Ok(format!("http://{}/mcp (loopback only, {access})", crate::runtime::format_authority(host, port)));
    }
    if is_wildcard(host) {
        return Ok(format!(
            "http://{}/mcp ({access}) -- REACHABLE FROM THE LOCAL NETWORK. Traffic is plain HTTP, so \
             the bearer token is visible to anyone who can capture the segment; use this on a \
             trusted network only.",
            crate::runtime::format_authority(host, port)
        ));
    }
    Ok(format!(
        "http://{}/mcp ({access}) -- reachable as {host}, plain HTTP.",
        crate::runtime::format_authority(host, port)
    ))
}

/// The token a request carries, or `None`.
///
/// Header only, and that restriction is the interface: the query string and the argument vector are
/// both deliberately unread, so no caller can grow a way in through them later by accident.
pub fn supplied_token(headers: &[(String, String)]) -> Option<&str> {
    // HTTP header names are case-insensitive, and this must not depend on the transport having
    // already folded them: a gate that reads `authorization` but not `Authorization` is a gate that
    // opens the moment somebody hand-writes a client.
    let value = headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("authorization"))?.1.as_str();
    let (scheme, rest) = value.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = rest.trim();
    (!token.is_empty()).then_some(token)
}

/// Compare a supplied secret against the expected one without a time-or-length side channel, and
/// without ever letting an empty expectation mean "allow".
///
/// The empty check is not defensive padding: `userdata/config_user.json` is rewritten by the app
/// without atomicity, so a transient parse failure reads back as "no token". If an empty expected
/// value authenticated, that flicker would open the server to the network.
pub fn authenticates(supplied: Option<&str>, expected: &str) -> bool {
    let Some(supplied) = supplied else { return false };
    !expected.is_empty() && constant_time_eq(supplied.as_bytes(), expected.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    // Lengths are compared up front because a short-circuit on the first differing byte is what a
    // probe measures. The length itself is not secret here: the expected value is a fixed string
    // the client is meant to know, so leaking how long it is leaks nothing.
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

/// The origin and `Host` values a request may carry.
///
/// DNS rebinding is the attack: a page on `evil.example` can be made to resolve to `127.0.0.1`, and
/// a browser will then happily send same-origin requests to it. Checking the `Host` header is what
/// stops that, because the forged request carries `evil.example` as its host. A loopback bind
/// therefore accepts only loopback hosts; a LAN bind accepts that address plus loopback, so the
/// user's own browser still works.
///
/// A wildcard port is spelled `host:*`, the same shape the Python SDK's transport security used.
pub fn allowed_hosts(host: &str, port: i64) -> (Vec<String>, Vec<String>) {
    let loopback_hosts = vec!["127.0.0.1:*".to_string(), "localhost:*".to_string(), "[::1]:*".to_string()];
    let loopback_origins =
        vec!["http://127.0.0.1:*".to_string(), "http://localhost:*".to_string(), "http://[::1]:*".to_string()];
    if is_wildcard(host) {
        // Bound to everything: there is no single address to name, so the check has nothing to
        // match against and the caller must not enforce one. This is the documented consequence of
        // opting into LAN access, not an oversight.
        return (Vec::new(), Vec::new());
    }
    if is_loopback(host) {
        return (loopback_hosts, loopback_origins);
    }
    let hosts = [vec![format!("{host}:{port}"), host.to_string()], loopback_hosts].concat();
    let origins = [vec![format!("http://{host}:{port}"), format!("http://{host}")], loopback_origins].concat();
    (hosts, origins)
}

/// The authority without its trailing `:port`, which for a bracketed IPv6 literal is the whole
/// string. HTTP requires the brackets, so `[::1]:21120` splits at the colon after `]`.
fn strip_port(authority: &str) -> &str {
    match authority.rsplit_once(':') {
        Some((head, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => head,
        _ => authority,
    }
}

/// Does this `Host` value pass the allow-list?
pub fn host_is_allowed(allow_list: &[String], presented: &str) -> bool {
    if allow_list.is_empty() {
        return true;
    }
    let presented = presented.trim();
    let authority = strip_port(presented);
    allow_list.iter().any(|allowed| match allowed.strip_suffix(":*") {
        Some(base) => base == authority,
        None => allowed == presented,
    })
}

/// The `Origin` a browser may send. An absent `Origin` is allowed: a native MCP client sends none,
/// and only a browser can be the victim of a rebinding attack.
pub fn origin_is_allowed(allow_list: &[String], presented: Option<&str>) -> bool {
    if allow_list.is_empty() {
        return true;
    }
    let Some(origin) = presented.map(str::trim).filter(|o| !o.is_empty()) else {
        return true;
    };
    let authority = strip_port(origin);
    allow_list.iter().any(|allowed| match allowed.strip_suffix(":*") {
        Some(base) => base == authority,
        None => allowed == origin,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "a-token-of-sufficient-length-indeed";

    #[test]
    fn loopback_is_a_set_of_spells_not_one_string() {
        for host in ["127.0.0.1", "127.1.2.3", "::1", "localhost", "LOCALHOST"] {
            assert!(is_loopback(host), "{host} is loopback");
        }
        for host in ["0.0.0.0", "::", "192.168.1.10", "10.0.0.1", "example.com"] {
            assert!(!is_loopback(host), "{host} is not loopback");
        }
    }

    #[test]
    fn every_interface_is_named_out_loud() {
        assert!(is_wildcard("0.0.0.0"));
        assert!(is_wildcard("::"));
        assert!(!is_wildcard("127.0.0.1"));
        assert!(!is_wildcard("192.168.1.10"));
    }

    /// The rule with no exception: off is only legal on a machine-local bind.
    #[test]
    fn auth_off_is_refused_on_anything_the_network_can_reach() {
        assert!(startup_guard("192.168.1.10", 21120, GOOD, false).is_err());
        assert!(startup_guard("0.0.0.0", 21120, GOOD, false).is_err());
        assert!(startup_guard("::", 21120, "", false).is_err());
        assert!(startup_guard("127.0.0.1", 21120, "", false).is_ok(), "loopback without auth is the one legal off");
        assert!(startup_guard("localhost", 21120, "", false).is_ok());
    }

    #[test]
    fn a_requested_bind_without_a_usable_secret_refuses_to_open() {
        for (host, token) in [("127.0.0.1", ""), ("127.0.0.1", "abc"), ("0.0.0.0", ""), ("0.0.0.0", "short"), ("192.168.1.10", "")] {
            let refused = startup_guard(host, 21120, token, true).unwrap_err().to_string();
            assert!(refused.contains("mcp_server_token"), "{host}/{token:?} gave: {refused}");
        }
        assert!(startup_guard("127.0.0.1", 21120, GOOD, true).is_ok());
    }

    #[test]
    fn a_port_that_cannot_be_bound_is_refused_rather_than_moved() {
        for port in [0, -1, 65536, 1 << 20] {
            let refused = startup_guard("127.0.0.1", port, GOOD, true).unwrap_err().to_string();
            assert!(refused.contains("mcp_server_port"), "{port} gave: {refused}");
        }
    }

    #[test]
    fn the_banner_says_what_was_really_opened() {
        let loopback = startup_guard("127.0.0.1", 21120, GOOD, true).unwrap();
        assert!(loopback.contains("loopback only") && loopback.contains("bearer token required"), "{loopback}");
        assert!(!loopback.contains(GOOD), "the banner must not print the secret");
        let lan = startup_guard("192.168.1.10", 21120, GOOD, true).unwrap();
        assert!(lan.contains("reachable as 192.168.1.10"), "{lan}");
        let wildcard = startup_guard("0.0.0.0", 21120, GOOD, true).unwrap();
        assert!(wildcard.contains("REACHABLE FROM THE LOCAL NETWORK"), "{wildcard}");
        let quiet = startup_guard("127.0.0.1", 21120, "", false).unwrap();
        assert!(quiet.contains("this machine only") && quiet.contains("no token"), "{quiet}");
    }

    /// The substitution this whole module exists to prevent, tested as behaviour rather than as a
    /// comment: a token in a place that leaks must not authenticate.
    #[test]
    fn only_the_authorization_header_carries_a_token() {
        let header = vec![("authorization".to_string(), format!("Bearer {GOOD}"))];
        assert_eq!(supplied_token(&header), Some(GOOD));
        // Mixed case scheme, and padding, which is what an HTTP client actually sends.
        let odd = vec![("Authorization".to_string(), format!("BEARER  {GOOD}  "))];
        assert_eq!(supplied_token(&odd), Some(GOOD));
        // Anything else is not a credential.
        let query = vec![("host".to_string(), "127.0.0.1:21120".to_string())];
        assert_eq!(supplied_token(&query), None);
        for wrong in [
            vec![("authorization".to_string(), format!("Basic {GOOD}"))],
            vec![("authorization".to_string(), GOOD.to_string())],
            vec![("authorization".to_string(), "Bearer".to_string())],
            vec![("authorization".to_string(), "Bearer   ".to_string())],
            vec![("x-api-key".to_string(), GOOD.to_string())],
            vec![("authorization".to_string(), String::new())],
        ] {
            assert_eq!(supplied_token(&wrong), None, "{wrong:?} must not be read as a credential");
        }
    }

    #[test]
    fn gate_behaviour_is_a_matrix_not_a_vibe() {
        assert!(authenticates(Some(GOOD), GOOD));
        assert!(!authenticates(None, GOOD), "no header is not a pass");
        assert!(!authenticates(Some("not-the-token-but-long-enough"), GOOD));
        assert!(!authenticates(Some(&GOOD[..GOOD.len() - 2]), GOOD), "a near miss is a miss");
        assert!(!authenticates(Some(&format!("{GOOD} ")), GOOD), "trailing whitespace is a different secret");
        assert!(!authenticates(Some(GOOD), ""), "an empty expectation denies, never allows");
        assert!(!authenticates(None, ""), "and so the config-flicker case stays shut");
    }

    #[test]
    fn a_loopback_bind_only_answers_to_loopback_hosts() {
        let (hosts, origins) = allowed_hosts("127.0.0.1", 21120);
        assert!(host_is_allowed(&hosts, "127.0.0.1:21120"));
        assert!(host_is_allowed(&hosts, "localhost:21120"), "the user's own browser uses the name");
        assert!(host_is_allowed(&hosts, "[::1]:21120"));
        assert!(!host_is_allowed(&hosts, "evil.example"), "a rebound DNS name must be refused");
        assert!(!host_is_allowed(&hosts, "192.168.1.10:21120"));
        assert!(origin_is_allowed(&origins, Some("http://localhost:21120")));
        assert!(origin_is_allowed(&origins, None), "a non-browser client sends no Origin");
        assert!(!origin_is_allowed(&origins, Some("http://evil.example")));
    }

    #[test]
    fn a_lan_bind_answers_to_that_address_and_still_to_loopback() {
        let (hosts, _) = allowed_hosts("192.168.1.10", 21120);
        assert!(host_is_allowed(&hosts, "192.168.1.10:21120"));
        assert!(host_is_allowed(&hosts, "192.168.1.10"));
        assert!(host_is_allowed(&hosts, "127.0.0.1:21120"));
        assert!(!host_is_allowed(&hosts, "evil.example"));
    }

    #[test]
    fn a_wildcard_bind_declines_the_host_check_because_it_has_no_single_name() {
        let (hosts, origins) = allowed_hosts("0.0.0.0", 21120);
        assert!(hosts.is_empty() && origins.is_empty());
        assert!(host_is_allowed(&hosts, "anything.example"));
        assert!(origin_is_allowed(&origins, Some("http://anything.example")));
    }

    #[test]
    fn a_bracketed_ipv6_config_value_is_still_the_loopback_address() {
        assert!(is_loopback("[::1]"));
        assert!(startup_guard("[::1]", 21120, GOOD, true).unwrap().contains("[::1]:21120"));
    }
}
