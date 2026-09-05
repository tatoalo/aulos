//! The hardened SSRF guard shared by every surface that accepts a download URL
//! (DESIGN §12.3 steps 1–3, §16.6, §17.3).
//!
//! The classifier lives in `aulos-core` because two surfaces must run **the same** one: the
//! Telegram bot, which is always guarded, and the v1/v2 API adds, which are guarded whenever
//! `AULOS_ALLOW_PRIVATE_TARGETS=false`. A second copy of an IP table is a copy that drifts.
//!
//! Every reason string is legacy's, byte-for-byte, because the bot echoes it back to the user as
//! `Ignored invalid links:\n- <url> (<reason>)`.
//!
//! `aulos_telegram::urls` still carries its own copy of this table plus the message-scanning
//! [`extract`](../../aulos_telegram/urls/fn.extract.html) port; the follow-up is to make that
//! module a re-export of this one.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// Why a URL was refused. `Display` is the legacy reason string, byte-for-byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
pub enum Reject {
    /// The string is not a URL at all.
    #[error("invalid URL format")]
    Malformed,
    /// Not `http`/`https`.
    #[error("only http/https URLs are allowed")]
    Scheme,
    /// No authority component.
    #[error("URL host is missing")]
    HostMissing,
    /// An authority with an empty host.
    #[error("URL host is empty")]
    HostEmpty,
    /// `localhost` or a `*.local` name.
    #[error("local network hosts are not allowed")]
    LocalName,
    /// A loopback, private, link-local, multicast, reserved, unspecified or unique-local IP.
    ///
    /// **Added** over legacy: `0.0.0.0/8`, an IPv4-mapped IPv6 address whose inner v4 address is
    /// private, and `[::1]`.
    #[error("private/local IP targets are not allowed")]
    PrivateIp,
}

/// Parses `raw` and refuses every private, local or otherwise non-routable target.
///
/// # Errors
/// One [`Reject`], whose `Display` is the reason string the user sees.
pub fn validate(raw: &str) -> Result<Url, Reject> {
    let url = Url::parse(raw.trim()).map_err(|_| Reject::Malformed)?;
    check(&url)?;
    Ok(url)
}

/// The host classification of [`validate`], on an **already-parsed** URL.
///
/// This is the entry point the API adds use: they have already turned the string into a [`Url`]
/// (and already answered `unsupported_url` for a scheme no provider can take), so re-parsing it
/// would only be a chance for the two parses to disagree.
///
/// # Errors
/// One [`Reject`].
pub fn check(url: &Url) -> Result<(), Reject> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Reject::Scheme);
    }
    let Some(host) = url.host() else {
        return Err(Reject::HostMissing);
    };
    match host {
        Host::Domain(name) => {
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty() {
                return Err(Reject::HostEmpty);
            }
            if name == "localhost" || name.ends_with(".local") {
                return Err(Reject::LocalName);
            }
            // `url` normalises a bracketed literal into `Host::Ipv6`, but a host that merely
            // *parses* as an IP while still being classified as a domain is still checked.
            if let Ok(ip) = name.parse::<IpAddr>()
                && is_blocked(ip)
            {
                return Err(Reject::PrivateIp);
            }
        }
        Host::Ipv4(v4) => {
            if is_blocked_v4(v4) {
                return Err(Reject::PrivateIp);
            }
        }
        Host::Ipv6(v6) => {
            if is_blocked_v6(v6) {
                return Err(Reject::PrivateIp);
            }
        }
    }
    Ok(())
}

/// Whether this address must never be a download target.
#[must_use]
pub fn is_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

/// The legacy IPv4 set, plus the whole of `0.0.0.0/8`.
///
/// Legacy used `ip.is_unspecified()`, which is only `0.0.0.0` itself; every other address in
/// `0.0.0.0/8` is "this network" and, on Linux, routes to the local host.
#[must_use]
pub fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    v4.octets()[0] == 0
        || v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_multicast()
        || v4.is_broadcast()
        || v4.is_documentation()
        // Legacy's `is_reserved` — 240.0.0.0/4 — plus the shared-address and benchmarking blocks.
        || v4.octets()[0] >= 240
        || matches!(v4.octets(), [100, b, _, _] if (64..128).contains(&b))
        || matches!(v4.octets(), [198, 18 | 19, _, _])
}

/// The legacy IPv6 set, plus IPv4-mapped addresses.
///
/// `::ffff:10.0.0.1` is not loopback, not unique-local and not link-local as an IPv6 address, so
/// legacy waved it through — and then the HTTP stack connected to `10.0.0.1`.
#[must_use]
pub fn is_blocked_v6(v6: Ipv6Addr) -> bool {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_blocked_v4(v4);
    }
    // A v6-compatible address (`::a.b.c.d`) is the same trick with an older spelling.
    if let Some(v4) = v6.to_ipv4() {
        return is_blocked_v4(v4);
    }
    v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        // Unique-local, `fc00::/7`.
        || (v6.segments()[0] & 0xfe00) == 0xfc00
        // Link-local, `fe80::/10`.
        || (v6.segments()[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_public_urls_are_accepted() {
        for raw in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "http://example.com",
            "https://8.8.8.8/x",
            "https://[2001:4860:4860::8888]/x",
            "https://sub.domain.example.co.uk/a/b?c=d#e",
            "http://[::ffff:8.8.8.8]/x",
        ] {
            assert!(validate(raw).is_ok(), "{raw} should be accepted");
        }
    }

    #[test]
    fn the_reject_table_and_its_reason_strings_are_stable() {
        let cases: [(&str, Reject, &str); 19] = [
            ("not a url", Reject::Malformed, "invalid URL format"),
            (
                "ftp://a.test/x",
                Reject::Scheme,
                "only http/https URLs are allowed",
            ),
            (
                "file:///etc/passwd",
                Reject::Scheme,
                "only http/https URLs are allowed",
            ),
            (
                "http://localhost/x",
                Reject::LocalName,
                "local network hosts are not allowed",
            ),
            (
                "http://nas.local/x",
                Reject::LocalName,
                "local network hosts are not allowed",
            ),
            (
                "http://127.0.0.1:8081/x",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
            (
                "http://10.0.0.5/x",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
            (
                "http://192.168.1.1/x",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
            (
                "http://172.16.4.4/x",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
            (
                "http://169.254.169.254/latest/meta-data",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
            (
                "http://224.0.0.1/x",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
            (
                "http://[fe80::1]/x",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
            (
                "http://0.0.0.0/x",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
            (
                "http://0.1.2.3/x",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
            (
                "http://[::ffff:10.0.0.1]/x",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
            (
                "http://[::1]:9000/x",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
            (
                "http://100.100.100.200/",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
            (
                "http://198.18.0.1/",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
            (
                "http://240.0.0.1/",
                Reject::PrivateIp,
                "private/local IP targets are not allowed",
            ),
        ];
        for (raw, want, reason) in cases {
            let got = validate(raw).expect_err(raw);
            assert_eq!(got, want, "{raw}");
            assert_eq!(got.to_string(), reason, "{raw}");
        }
        assert_eq!(Reject::HostMissing.to_string(), "URL host is missing");
        assert_eq!(Reject::HostEmpty.to_string(), "URL host is empty");
    }

    /// [`check`] and [`validate`] must never disagree — the API path uses the first, the bot the
    /// second, and DESIGN §17.3 says they are the same guard.
    #[test]
    fn check_agrees_with_validate_on_an_already_parsed_url() {
        for raw in [
            "http://169.254.169.254/",
            "https://example.com/x",
            "http://[::1]/x",
        ] {
            let parsed = Url::parse(raw).expect(raw);
            assert_eq!(check(&parsed).is_ok(), validate(raw).is_ok(), "{raw}");
        }
    }

    #[test]
    fn is_blocked_agrees_for_both_families() {
        assert!(is_blocked("127.0.0.1".parse().expect("ip")));
        assert!(is_blocked("::1".parse().expect("ip")));
        assert!(!is_blocked("1.1.1.1".parse().expect("ip")));
        assert!(!is_blocked("2606:4700::1".parse().expect("ip")));
    }
}
