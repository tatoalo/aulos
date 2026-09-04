//! URL extraction and the hardened SSRF guard (DESIGN §12.3 steps 1–3).
//!
//! [`extract`] is a byte-for-byte port of legacy `_extract_urls`; [`validate`] is a port of
//! `_validate_url` **plus** the three rejections legacy missed. Every reason string is legacy's,
//! because the bot echoes it back to the user as
//! `Ignored invalid links:\n- <url> (<reason>)`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// The legacy pattern this module implements, for the record: `https?://[^\s<>()\[\]{}"']+`
/// (`app/telegram_bot.py:30`).
///
/// It is scanned by hand rather than compiled, because DESIGN §3 does not budget `regex` for this
/// crate and the pattern is a literal prefix followed by a "run until one of these" class — which
/// is exactly what [`extract`] does, with the same results and no dependency.
pub const URL_PATTERN: &str = r#"https?://[^\s<>()\[\]{}"']+"#;

/// The characters that end a match, i.e. the pattern's negated class plus whitespace.
pub const URL_DELIMITERS: &str = "<>()[]{}\"'";

/// The characters legacy stripped from the end of every match (`TRAILING_PUNCTUATION`).
pub const TRAILING_PUNCTUATION: &str = ".,;:!?)]}>'\"";

/// The two prefixes a match can start with, longest first so `https` is never read as `http`.
const SCHEMES: [&str; 2] = ["https://", "http://"];

/// Every URL in `text`, right-trimmed of [`TRAILING_PUNCTUATION`] and deduped **preserving
/// order** (DESIGN §12.3 step 1).
///
/// A verbatim port of legacy `_extract_urls`: find each `https?://…` run, `rstrip` the trailing
/// punctuation, then drop repeats while keeping the first sighting's position.
#[must_use]
pub fn extract(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < text.len() {
        if !text.is_char_boundary(i) {
            i += 1;
            continue;
        }
        let rest = &text[i..];
        let Some(scheme) = SCHEMES.iter().find(|s| rest.starts_with(**s)) else {
            i += 1;
            continue;
        };
        let body = &rest[scheme.len()..];
        // The negated class, `[^\s<>()\[\]{}"']`, applied greedily.
        let run = body
            .char_indices()
            .find(|(_, c)| c.is_whitespace() || URL_DELIMITERS.contains(*c))
            .map_or(body.len(), |(offset, _)| offset);
        if run == 0 {
            // `https://` with nothing after it does not match `+`.
            i += scheme.len();
            continue;
        }
        let end = i + scheme.len() + run;
        let trimmed = text[i..end].trim_end_matches(|c| TRAILING_PUNCTUATION.contains(c));
        if !trimmed.is_empty() && !out.iter().any(|u| u == trimmed) {
            out.push(trimmed.to_owned());
        }
        i = end;
    }
    out
}

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
    /// private, and `[::1]` (which legacy's `ip_address(host)` never saw, because `urlsplit`
    /// hands back `::1` while legacy compared the *bracketed* form against a name list).
    #[error("private/local IP targets are not allowed")]
    PrivateIp,
}

/// The legacy guard plus the three additions (DESIGN §12.3 step 3).
///
/// # Errors
/// One [`Reject`], whose `Display` is the reason string the user sees.
pub fn validate(raw: &str) -> Result<Url, Reject> {
    let url = Url::parse(raw.trim()).map_err(|_| Reject::Malformed)?;
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
            // *parses* as an IP while still being classified as a domain (there is none today,
            // and there was in `urlsplit`) is still checked.
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
    Ok(url)
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
fn is_blocked_v4(v4: Ipv4Addr) -> bool {
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
fn is_blocked_v6(v6: Ipv6Addr) -> bool {
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
mod tests {
    use super::*;

    #[test]
    fn extraction_finds_every_url_and_stops_at_the_delimiters() {
        let text = "see https://a.test/x and <https://b.test/y> plus (http://c.test/z)";
        assert_eq!(
            extract(text),
            vec![
                "https://a.test/x".to_owned(),
                "https://b.test/y".to_owned(),
                "http://c.test/z".to_owned(),
            ]
        );
    }

    #[test]
    fn trailing_punctuation_is_stripped() {
        for (raw, want) in [
            ("https://a.test/x.", "https://a.test/x"),
            ("https://a.test/x,", "https://a.test/x"),
            ("https://a.test/x;", "https://a.test/x"),
            ("https://a.test/x:", "https://a.test/x"),
            ("https://a.test/x!", "https://a.test/x"),
            ("https://a.test/x?", "https://a.test/x"),
            ("https://a.test/x'\"", "https://a.test/x"),
            ("https://a.test/x...", "https://a.test/x"),
        ] {
            assert_eq!(extract(raw), vec![want.to_owned()], "{raw}");
        }
        // A query string survives; only the *trailing* punctuation goes.
        assert_eq!(
            extract("https://a.test/x?v=1&t=2."),
            vec!["https://a.test/x?v=1&t=2".to_owned()]
        );
    }

    #[test]
    fn duplicates_are_dropped_and_order_is_preserved() {
        assert_eq!(
            extract("https://b.test/1 https://a.test/2 https://b.test/1"),
            vec!["https://b.test/1".to_owned(), "https://a.test/2".to_owned()]
        );
    }

    /// Parity, and worth pinning: a bracketed IPv6 URL is **not extracted** from a message,
    /// because `[` and `]` are in the pattern's negated class — legacy behaved the same way. It is
    /// still rejected by [`validate`], which is the entry point every other caller uses.
    #[test]
    fn a_bracketed_ipv6_url_is_not_extracted_but_is_still_rejected() {
        assert!(extract("see http://[::1]:9000/x").is_empty());
        assert_eq!(
            validate("http://[::1]:9000/x").expect_err("still guarded"),
            Reject::PrivateIp
        );
    }

    #[test]
    fn a_message_with_no_links_yields_nothing() {
        assert!(extract("hello there").is_empty());
        assert!(extract("").is_empty());
        assert!(
            extract("ftp://a.test/x").is_empty(),
            "the pattern is http(s)"
        );
    }

    /// The legacy accept set.
    #[test]
    fn ordinary_public_urls_are_accepted() {
        for raw in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "http://example.com",
            "https://8.8.8.8/x",
            "https://[2001:4860:4860::8888]/x",
            "https://sub.domain.example.co.uk/a/b?c=d#e",
        ] {
            assert!(validate(raw).is_ok(), "{raw} should be accepted");
        }
    }

    /// The legacy reject set, each with its reason string.
    #[test]
    fn the_legacy_reject_table_is_reproduced() {
        let cases: [(&str, Reject, &str); 12] = [
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
        ];
        for (raw, want, reason) in cases {
            let got = validate(raw).expect_err(raw);
            assert_eq!(got, want, "{raw}");
            assert_eq!(got.to_string(), reason, "{raw}");
        }
    }

    /// The three additions over legacy (DESIGN §12.3 step 3).
    #[test]
    fn the_new_rejections_are_covered() {
        for raw in [
            // `0.0.0.0/8` — legacy only caught `0.0.0.0` itself.
            "http://0.0.0.0/x",
            "http://0.1.2.3/x",
            "http://0.255.255.255/x",
            // IPv4-mapped IPv6.
            "http://[::ffff:10.0.0.1]/x",
            "http://[::ffff:127.0.0.1]/x",
            // `[::1]`.
            "http://[::1]/x",
            "http://[::1]:9000/x",
        ] {
            let got = validate(raw).expect_err(raw);
            assert_eq!(got, Reject::PrivateIp, "{raw}");
            assert_eq!(got.to_string(), "private/local IP targets are not allowed");
        }
        // An IPv4-mapped **public** address is still fine.
        assert!(validate("http://[::ffff:8.8.8.8]/x").is_ok());
    }

    #[test]
    fn cloud_metadata_and_reserved_ranges_are_blocked() {
        for raw in [
            "http://169.254.169.254/",
            "http://100.100.100.200/",
            "http://198.18.0.1/",
            "http://240.0.0.1/",
            "http://255.255.255.255/",
            "http://[fc00::1]/",
            "http://[::]/",
        ] {
            assert_eq!(validate(raw).expect_err(raw), Reject::PrivateIp, "{raw}");
        }
    }

    #[test]
    fn the_reason_strings_are_stable() {
        assert_eq!(Reject::HostMissing.to_string(), "URL host is missing");
        assert_eq!(Reject::HostEmpty.to_string(), "URL host is empty");
    }

    #[test]
    fn is_blocked_agrees_for_both_families() {
        assert!(is_blocked("127.0.0.1".parse().expect("ip")));
        assert!(is_blocked("::1".parse().expect("ip")));
        assert!(!is_blocked("1.1.1.1".parse().expect("ip")));
        assert!(!is_blocked("2606:4700::1".parse().expect("ip")));
    }
}
