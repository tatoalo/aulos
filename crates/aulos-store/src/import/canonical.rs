//! The dedupe key (DESIGN §8.5).
//!
//! It lives here because the importer needs it — `items.canonical_key` is `NOT NULL`, and
//! DESIGN §7.6.2 step 5 requires the imported value to be computed by "the same function used at
//! runtime". The runtime caller is `aulos-queue::canonical_key`, whose PLAN signature takes a
//! `&Url`; this one takes `&str` so the function can live in the crate that is *upstream* of both
//! (`aulos-store`'s DESIGN §3 row deliberately does not budget for `url`, which is why the store
//! rehydrates a `Url` through serde — see [`crate::json`]). `aulos-queue` re-exports it:
//!
//! ```ignore
//! pub fn canonical_key(p: &ProviderId, url: &Url, media_id: Option<&str>) -> Box<str> {
//!     aulos_store::canonical_key(p.as_str(), url.as_str(), media_id)
//! }
//! ```
//!
//! Two implementations of a dedupe key is the failure mode this arrangement exists to prevent: an
//! imported row that hashes differently from a freshly added one would silently defeat dedupe for
//! every pre-cutover URL.

/// The separator between the provider id and the normalised target: ASCII unit separator, which
/// cannot occur in a provider id (`[A-Za-z0-9._:-]`) or in a percent-encoded URL.
const SEP: char = '\u{1f}';

/// Query parameters dropped from a YouTube URL before it becomes a key (DESIGN §8.5).
///
/// The same list the iOS share extension applies, so the client can stop doing it.
const YOUTUBE_TRACKING_PARAMS: [&str; 3] = ["si", "feature", "pp"];

/// The dedupe key for one target (DESIGN §8.5).
///
/// `provider` + `\u{1f}` + the normalised target, where the target is the provider's own canonical
/// id (`media_id`) once resolution has produced one, and otherwise the normalised URL: lower-cased
/// scheme and host, no default port, no fragment, no trailing `/`, and the per-host tracking
/// parameters removed.
///
/// It is **not** unique in the database: a channel page may legitimately list the same video
/// twice, and the same video as `mp3` and as `mp4` is two items. Dedupe is a policy over
/// `(canonical_key, selection)` for user adds only.
#[must_use]
pub fn canonical_key(provider: &str, url: &str, media_id: Option<&str>) -> Box<str> {
    let target = match media_id.map(str::trim).filter(|m| !m.is_empty()) {
        Some(id) => id.to_owned(),
        None => normalize_url(url),
    };
    format!("{provider}{SEP}{target}").into_boxed_str()
}

/// The normalised URL half of a key.
///
/// Hand-rolled rather than `Url`-based for the dependency reason in the module docs. Everything it
/// does is idempotent on a string that already came out of `Url::as_str`, which is the production
/// input.
#[must_use]
pub fn normalize_url(url: &str) -> String {
    let raw = url.trim();
    // 1. Drop the fragment: it never identifies a different video.
    let raw = raw.split('#').next().unwrap_or(raw);

    // 2. Split scheme, authority, path and query.
    let (scheme, rest) = match raw.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => return raw.to_ascii_lowercase(),
    };
    let (authority, after) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let authority = authority.to_ascii_lowercase();
    let (path, query) = match after.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (after, None),
    };

    // 3. Strip the default port.
    let host = strip_default_port(&authority, &scheme);

    // 4. Strip trailing slashes from the path.
    let path = path.trim_end_matches('/');

    // 5. Drop the tracking parameters, keeping the rest in their original order.
    let mut out = format!("{scheme}://{host}{path}");
    if let Some(q) = query {
        let kept: Vec<&str> = q
            .split('&')
            .filter(|pair| !pair.is_empty())
            .filter(|pair| !is_tracking_param(host, pair))
            .collect();
        if !kept.is_empty() {
            out.push('?');
            out.push_str(&kept.join("&"));
        }
    }
    out
}

/// `:80` on `http` and `:443` on `https` carry no information.
fn strip_default_port<'a>(authority: &'a str, scheme: &str) -> &'a str {
    match scheme {
        "http" => authority.strip_suffix(":80").unwrap_or(authority),
        "https" => authority.strip_suffix(":443").unwrap_or(authority),
        _ => authority,
    }
}

/// Whether `host` is one of YouTube's.
fn is_youtube(host: &str) -> bool {
    let host = host.strip_prefix("www.").unwrap_or(host);
    host == "youtube.com"
        || host == "youtu.be"
        || host == "youtube-nocookie.com"
        || host.ends_with(".youtube.com")
}

/// Whether one `k=v` pair is tracking noise for this host.
fn is_tracking_param(host: &str, pair: &str) -> bool {
    let key = pair.split('=').next().unwrap_or(pair);
    let lower = key.to_ascii_lowercase();
    if lower.starts_with("utm_") {
        return true;
    }
    is_youtube(host) && YOUTUBE_TRACKING_PARAMS.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resolved_media_id_is_the_target() {
        let k = canonical_key("ytdlp", "https://youtu.be/abc?si=xyz", Some("abc"));
        assert_eq!(&*k, "ytdlp\u{1f}abc");
        // Which is what collapses the three spellings of one video.
        for url in [
            "https://youtu.be/abc",
            "https://www.youtube.com/watch?v=abc",
            "https://www.youtube.com/watch?v=abc&t=30",
        ] {
            assert_eq!(canonical_key("ytdlp", url, Some("abc")), k);
        }
    }

    #[test]
    fn an_unresolved_url_is_normalised() {
        assert_eq!(
            &*canonical_key(
                "ytdlp",
                "HTTPS://WWW.YouTube.com:443/watch?v=abc&si=xyz",
                None
            ),
            "ytdlp\u{1f}https://www.youtube.com/watch?v=abc"
        );
        assert_eq!(
            &*canonical_key(
                "streamingcommunity",
                "https://sc.test/it/watch/9?e=77",
                None
            ),
            "streamingcommunity\u{1f}https://sc.test/it/watch/9?e=77"
        );
    }

    #[test]
    fn normalisation_is_idempotent_and_covers_the_documented_rules() {
        let cases = [
            // fragment
            ("https://x.test/a#t=10", "https://x.test/a"),
            // trailing slash
            ("https://x.test/a/", "https://x.test/a"),
            ("https://x.test/", "https://x.test"),
            // default ports
            ("http://x.test:80/a", "http://x.test/a"),
            ("https://x.test:443/a", "https://x.test/a"),
            // a non-default port is information
            ("https://x.test:8443/a", "https://x.test:8443/a"),
            // utm_* everywhere
            (
                "https://x.test/a?utm_source=n&keep=1",
                "https://x.test/a?keep=1",
            ),
            // youtube-only params, on youtube
            (
                "https://youtu.be/abc?si=xyz&feature=share&pp=1&t=30",
                "https://youtu.be/abc?t=30",
            ),
            // ... and nowhere else
            ("https://x.test/a?si=xyz", "https://x.test/a?si=xyz"),
            // an all-tracking query leaves no `?`
            ("https://youtu.be/abc?si=xyz", "https://youtu.be/abc"),
            // case
            ("HTTPS://X.TEST/A", "https://x.test/A"),
        ];
        for (input, want) in cases {
            let got = normalize_url(input);
            assert_eq!(got, want, "{input}");
            assert_eq!(normalize_url(&got), want, "{input} must be idempotent");
        }
    }

    #[test]
    fn a_schemeless_or_odd_target_still_produces_a_key() {
        assert_eq!(
            &*canonical_key("ytdlp", "not a url", None),
            "ytdlp\u{1f}not a url"
        );
        assert_eq!(&*canonical_key("ytdlp", "", None), "ytdlp\u{1f}");
        // An empty media_id falls back to the URL rather than keying everything the same.
        assert_eq!(
            canonical_key("ytdlp", "https://x.test/a", Some("  ")),
            canonical_key("ytdlp", "https://x.test/a", None)
        );
    }

    #[test]
    fn the_provider_is_part_of_the_key() {
        assert_ne!(
            canonical_key("ytdlp", "https://x.test/a", None),
            canonical_key("streamingcommunity", "https://x.test/a", None)
        );
    }

    #[test]
    fn youtube_host_variants_are_recognised() {
        for host in [
            "youtube.com",
            "www.youtube.com",
            "m.youtube.com",
            "music.youtube.com",
            "youtu.be",
            "www.youtu.be",
            "youtube-nocookie.com",
        ] {
            assert!(is_youtube(host), "{host}");
        }
        for host in ["notyoutube.com", "youtube.com.evil.test", "x.test"] {
            assert!(!is_youtube(host), "{host}");
        }
    }
}
