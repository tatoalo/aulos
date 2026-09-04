//! The one HTTP seam the whole scrape pipeline talks through (DESIGN §10.1).
//!
//! Legacy used `curl_cffi` with `impersonate="chrome"`, and it needed to: vixcloud sits behind
//! Cloudflare and the fronting is TLS-fingerprint sensitive, so a plain rustls client can be
//! turned away where a Chrome-shaped one is let through. Two implementations therefore live
//! behind one trait:
//!
//! | Implementation | Feature | Selected by |
//! |---|---|---|
//! | [`WreqClient`] — BoringSSL, Chrome cipher/curve/sigalg lists, Chrome HTTP/2 settings | `sc-impersonate` (default on) | `AULOS_SC_HTTP=auto` (when compiled in) or `=impersonate` |
//! | [`PlainClient`] — `reqwest` + rustls with hand-set Chrome headers | always | `AULOS_SC_HTTP=plain`, or `auto` with the feature off |
//!
//! Everything above this module is client-agnostic, which is what makes the pipeline testable
//! against an in-process mock with no network at all.
//!
//! # Cookies
//!
//! [`ScHttp::cookie_header`] is an **addition** to the DESIGN §10.1 trait, which lists only `get`
//! and `impersonating`. It exists because [`crate::jit::fresh_stream`] must hand
//! `N_m3u8DL-RE`/ffmpeg the session's cookies as one `Cookie:` header — legacy read them straight
//! off `curl_cffi`'s session jar (`streamingcommunity.py:442`). It has a default implementation
//! returning an empty string, so the design signature is still all an implementor has to provide.

use std::sync::Mutex;

use async_trait::async_trait;
use aulos_core::config::{Config, ScHttpMode};
use url::Url;

use crate::error::{ScError, ScInitError};

/// The Chrome user agent legacy advertised, kept byte-identical so a server-side UA allowlist
/// keeps behaving (`streamingcommunity.py:18`).
pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
     Chrome/131.0.0.0 Safari/537.36";

/// The per-request deadline both clients apply.
///
/// The scrape is six small requests at worst and the resolve task already has an
/// `AULOS_RESOLVE_TIMEOUT_SECS` deadline above it; this is the inner guard that keeps one wedged
/// socket from eating the whole budget.
pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The connect deadline both clients apply.
pub const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// One outbound `GET`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScReq {
    /// Where to go.
    pub url: Url,
    /// Extra request headers, in order. The client's own Chrome header set is merged underneath.
    pub headers: Vec<(Box<str>, Box<str>)>,
}

impl ScReq {
    /// A bare `GET`.
    #[must_use]
    pub const fn get(url: Url) -> Self {
        Self {
            url,
            headers: Vec::new(),
        }
    }

    /// Adds one header.
    #[must_use]
    pub fn header(mut self, name: &str, value: impl Into<Box<str>>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The S2 Inertia request: the two `x-inertia*` headers plus a JSON `Accept`
    /// (DESIGN §10.2).
    #[must_use]
    pub fn inertia(url: Url, version: &str) -> Self {
        Self::get(url)
            .header("x-inertia", "true")
            .header("x-inertia-version", version)
            .header("accept", "application/json")
    }
}

/// One response, body already read as text.
///
/// The bodies in this pipeline are one HTML page or one JSON document, never a stream, so buffering
/// is the honest shape and it keeps the trait object-safe without a `Stream` associated type.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ScRes {
    /// The status code.
    pub status: u16,
    /// The URL the body actually came from, after redirects.
    pub url: Url,
    /// The body.
    pub body: String,
}

impl ScRes {
    /// Whether the status is 2xx.
    #[must_use]
    pub const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }

    /// The legacy `raise_for_status()`.
    ///
    /// # Errors
    /// [`ScError::Status`] for any non-2xx.
    pub fn error_for_status(self) -> Result<Self, ScError> {
        if self.is_success() {
            Ok(self)
        } else {
            Err(ScError::Status {
                url: self.url.to_string(),
                status: self.status,
            })
        }
    }

    /// Parses the body as JSON.
    ///
    /// # Errors
    /// [`ScError::BadJson`] when the body is not JSON.
    pub fn json(&self) -> Result<serde_json::Value, ScError> {
        serde_json::from_str(&self.body).map_err(|e| ScError::BadJson {
            url: self.url.to_string(),
            message: e.to_string(),
        })
    }
}

/// The client seam (DESIGN §10.1).
#[async_trait]
pub trait ScHttp: Send + Sync {
    /// Performs one `GET`, following redirects, and reads the whole body.
    ///
    /// A non-2xx status is **returned, not raised**: S2's version-drift retry has to inspect the
    /// status before deciding, so raising here would force it to unwrap an error type.
    ///
    /// # Errors
    /// [`ScError::Transport`] or [`ScError::Timeout`].
    async fn get(&self, req: ScReq) -> Result<ScRes, ScError>;

    /// Whether this client presents a Chrome TLS/HTTP2 fingerprint.
    ///
    /// Reported in `healthz` and in `GET api/v2/providers` so a silent downgrade is visible
    /// (DESIGN §10.1).
    fn impersonating(&self) -> bool;

    /// The session's cookies as one `Cookie:` header value, or an empty string.
    ///
    /// See the module docs: this is an addition to the DESIGN §10.1 trait, with a default
    /// implementation, because the download engines need the jar as a header.
    fn cookie_header(&self) -> String {
        String::new()
    }
}

/// The Chrome request headers the plain client hand-sets (DESIGN §10.1).
///
/// `accept-encoding` is deliberately absent: neither client is built with a decompression feature,
/// so advertising an encoding would hand us a body we cannot read.
const CHROME_HEADERS: &[(&str, &str)] = &[
    (
        "accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,\
         image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7",
    ),
    ("accept-language", "it-IT,it;q=0.9,en-US;q=0.8,en;q=0.7"),
    (
        "sec-ch-ua",
        "\"Chromium\";v=\"131\", \"Not_A Brand\";v=\"24\"",
    ),
    ("sec-ch-ua-mobile", "?0"),
    ("sec-ch-ua-platform", "\"Windows\""),
    ("sec-fetch-dest", "document"),
    ("sec-fetch-mode", "navigate"),
    ("sec-fetch-site", "none"),
    ("sec-fetch-user", "?1"),
    ("upgrade-insecure-requests", "1"),
];

/// An insertion-ordered `name=value` jar, fed from every response's `Set-Cookie`.
///
/// Both clients keep their own real cookie store for *sending* cookies; this exists only so
/// [`ScHttp::cookie_header`] can hand the jar to an external downloader as a header, which is
/// what legacy did off `curl_cffi`'s session (`streamingcommunity.py:442`).
#[derive(Debug, Default)]
pub(crate) struct CookieRecorder {
    /// A `Vec` rather than a map: a session holds a handful of cookies, so a linear scan is
    /// faster than hashing and keeps insertion order without a dependency the DESIGN §3 budget for
    /// this crate does not carry.
    jar: Mutex<Vec<(String, String)>>,
}

impl CookieRecorder {
    /// Records one `name=value` pair, replacing an earlier value in place.
    fn record(&self, name: &str, value: &str) {
        // A poisoned lock only means another thread panicked while recording a cookie; the jar is
        // still structurally sound and losing it would break every download.
        let mut jar = self
            .jar
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = jar.iter_mut().find(|(k, _)| k == name) {
            slot.1 = value.to_owned();
        } else {
            jar.push((name.to_owned(), value.to_owned()));
        }
    }

    /// The jar as a `Cookie:` header value.
    fn header(&self) -> String {
        let jar = self
            .jar
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        jar.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

// ---------------------------------------------------------------------------
// The plain `reqwest` client — always compiled in (DESIGN §10.1).
// ---------------------------------------------------------------------------

/// `reqwest` + rustls with hand-set Chrome headers: the always-present fallback and the only
/// client the test matrix uses (there is no BoringSSL in CI).
pub struct PlainClient {
    client: reqwest::Client,
    cookies: CookieRecorder,
}

impl PlainClient {
    /// Builds the client.
    ///
    /// # Errors
    /// [`ScInitError::Client`] when `reqwest` cannot build a TLS backend.
    pub fn new() -> Result<Self, ScInitError> {
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in CHROME_HEADERS {
            if let (Ok(n), Ok(v)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                headers.insert(n, v);
            }
        }
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .cookie_store(true)
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| ScInitError::Client(e.to_string()))?;
        Ok(Self {
            client,
            cookies: CookieRecorder::default(),
        })
    }
}

#[async_trait]
impl ScHttp for PlainClient {
    async fn get(&self, req: ScReq) -> Result<ScRes, ScError> {
        let mut builder = self.client.get(req.url.clone());
        for (name, value) in &req.headers {
            builder = builder.header(&**name, &**value);
        }
        let res = builder
            .send()
            .await
            .map_err(|e| classify_reqwest(&req.url, &e))?;
        let status = res.status().as_u16();
        let url = res.url().clone();
        for c in res.cookies() {
            self.cookies.record(c.name(), c.value());
        }
        let body = res
            .text()
            .await
            .map_err(|e| classify_reqwest(&req.url, &e))?;
        Ok(ScRes { status, url, body })
    }

    fn impersonating(&self) -> bool {
        false
    }

    fn cookie_header(&self) -> String {
        self.cookies.header()
    }
}

fn classify_reqwest(url: &Url, e: &reqwest::Error) -> ScError {
    if e.is_timeout() {
        ScError::Timeout {
            url: url.to_string(),
        }
    } else {
        ScError::Transport {
            url: url.to_string(),
            message: e.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// The Chrome-impersonating `wreq` client — feature `sc-impersonate` (DESIGN §10.1).
// ---------------------------------------------------------------------------

#[cfg(feature = "sc-impersonate")]
mod impersonate {
    use super::{
        CHROME_HEADERS, CONNECT_TIMEOUT, CookieRecorder, REQUEST_TIMEOUT, ScError, ScHttp,
        ScInitError, ScReq, ScRes, USER_AGENT, Url, async_trait,
    };

    /// Chrome's TLS 1.3 + TLS 1.2 cipher preference, in BoringSSL's cipher-list syntax.
    ///
    /// This is the list `curl-impersonate`/`curl_cffi`'s `chrome` target sends, which is what
    /// legacy was fingerprinted as.
    const CHROME_CIPHERS: &str = "TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384:\
         TLS_CHACHA20_POLY1305_SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:\
         ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-CHACHA20-POLY1305:\
         ECDHE-RSA-CHACHA20-POLY1305:ECDHE-RSA-AES128-SHA:ECDHE-RSA-AES256-SHA:\
         AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA";

    /// Chrome's supported-groups list. Deliberately the conservative three rather than a
    /// post-quantum group name a given BoringSSL vintage may not know: an unknown name makes
    /// `ClientBuilder::build()` fail, and a working fingerprint-shaped client beats no client.
    const CHROME_CURVES: &str = "X25519:P-256:P-384";

    /// Chrome's signature-algorithm list.
    const CHROME_SIGALGS: &str = "ecdsa_secp256r1_sha256:rsa_pss_rsae_sha256:rsa_pkcs1_sha256:\
         ecdsa_secp384r1_sha384:rsa_pss_rsae_sha384:rsa_pkcs1_sha384:rsa_pss_rsae_sha512:\
         rsa_pkcs1_sha512";

    /// Chrome's `SETTINGS_INITIAL_WINDOW_SIZE`.
    const H2_INITIAL_WINDOW: u32 = 6_291_456;
    /// Chrome's connection-level window.
    const H2_CONNECTION_WINDOW: u32 = 15_728_640;
    /// Chrome's `SETTINGS_HEADER_TABLE_SIZE`.
    const H2_HEADER_TABLE: u32 = 65_536;
    /// Chrome's `SETTINGS_MAX_HEADER_LIST_SIZE`.
    const H2_MAX_HEADER_LIST: u32 = 262_144;

    /// `wreq` over BoringSSL with a Chrome TLS and HTTP/2 profile.
    pub struct WreqClient {
        client: wreq::Client,
        cookies: CookieRecorder,
        emulated: bool,
    }

    fn chrome_headers() -> wreq::header::HeaderMap {
        let mut headers = wreq::header::HeaderMap::new();
        if let Ok(v) = wreq::header::HeaderValue::from_str(USER_AGENT) {
            headers.insert(wreq::header::USER_AGENT, v);
        }
        for (name, value) in CHROME_HEADERS {
            if let (Ok(n), Ok(v)) = (
                wreq::header::HeaderName::from_bytes(name.as_bytes()),
                wreq::header::HeaderValue::from_str(value),
            ) {
                headers.insert(n, v);
            }
        }
        headers
    }

    fn chrome_emulation() -> wreq::Emulation {
        let tls = wreq::tls::TlsOptions::builder()
            .cipher_list(CHROME_CIPHERS)
            .curves_list(CHROME_CURVES)
            .sigalgs_list(CHROME_SIGALGS)
            .grease_enabled(true)
            .permute_extensions(true)
            .enable_ocsp_stapling(true)
            .enable_signed_cert_timestamps(true)
            .session_ticket(true)
            .build();
        let http2 = wreq::http2::Http2Options::builder()
            .initial_window_size(H2_INITIAL_WINDOW)
            .initial_connection_window_size(H2_CONNECTION_WINDOW)
            .header_table_size(H2_HEADER_TABLE)
            .max_header_list_size(H2_MAX_HEADER_LIST)
            .build();
        wreq::Emulation::builder()
            .tls_options(tls)
            .http2_options(http2)
            .headers(chrome_headers())
            .build(wreq::Group::default())
    }

    impl WreqClient {
        /// Builds the Chrome-impersonating client.
        ///
        /// `ClientBuilder::build()` constructs the BoringSSL connector eagerly, so a cipher or
        /// curve name this BoringSSL vintage does not know is caught **here** rather than on the
        /// first download. When that happens the client is rebuilt with `wreq`'s default TLS
        /// options — still BoringSSL, so still much closer to Chrome than rustls — and
        /// [`ScHttp::impersonating`] reports `false` so the downgrade is visible in `healthz`.
        ///
        /// # Errors
        /// [`ScInitError::Client`] when even the plain BoringSSL client will not build.
        pub fn new() -> Result<Self, ScInitError> {
            match Self::build(true) {
                Ok(c) => Ok(c),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "the Chrome TLS profile was rejected by BoringSSL; \
                         falling back to wreq's default TLS options"
                    );
                    Self::build(false)
                }
            }
        }

        fn build(emulated: bool) -> Result<Self, ScInitError> {
            let builder = wreq::Client::builder()
                .cookie_store(true)
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(CONNECT_TIMEOUT);
            let builder = if emulated {
                builder.emulation(chrome_emulation())
            } else {
                builder.default_headers(chrome_headers())
            };
            let client = builder
                .build()
                .map_err(|e| ScInitError::Client(e.to_string()))?;
            Ok(Self {
                client,
                cookies: CookieRecorder::default(),
                emulated,
            })
        }
    }

    #[async_trait]
    impl ScHttp for WreqClient {
        async fn get(&self, req: ScReq) -> Result<ScRes, ScError> {
            let mut builder = self.client.get(req.url.as_str());
            for (name, value) in &req.headers {
                builder = builder.header(&**name, &**value);
            }
            let res = builder.send().await.map_err(|e| ScError::Transport {
                url: req.url.to_string(),
                message: e.to_string(),
            })?;
            let status = res.status().as_u16();
            let url = Url::parse(&res.uri().to_string()).unwrap_or_else(|_| req.url.clone());
            for c in res.cookies() {
                self.cookies.record(c.name(), c.value());
            }
            let body = res.text().await.map_err(|e| ScError::Transport {
                url: req.url.to_string(),
                message: e.to_string(),
            })?;
            Ok(ScRes { status, url, body })
        }

        fn impersonating(&self) -> bool {
            self.emulated
        }

        fn cookie_header(&self) -> String {
            self.cookies.header()
        }
    }
}

#[cfg(feature = "sc-impersonate")]
pub use impersonate::WreqClient;

/// Builds the client `AULOS_SC_HTTP` asks for (DESIGN §10.1).
///
/// | Mode | `sc-impersonate` on | `sc-impersonate` off |
/// |---|---|---|
/// | `auto` (default) | [`WreqClient`] | one boot `WARN`, then [`PlainClient`] |
/// | `impersonate` | [`WreqClient`] | [`ScInitError::ImpersonateUnavailable`] ⇒ the caller registers `Degraded` |
/// | `plain` | [`PlainClient`] | [`PlainClient`] |
///
/// `auto` never fails to start over a missing feature: the provider stays `Ready` with
/// `impersonating: false`, which is exactly what DESIGN §10.1 prescribes.
///
/// # Errors
/// [`ScInitError`] per the table above.
pub fn build_client(cfg: &Config) -> Result<std::sync::Arc<dyn ScHttp>, ScInitError> {
    match cfg.sc_http {
        ScHttpMode::Plain => Ok(std::sync::Arc::new(PlainClient::new()?)),
        ScHttpMode::Impersonate => {
            #[cfg(feature = "sc-impersonate")]
            {
                Ok(std::sync::Arc::new(WreqClient::new()?))
            }
            #[cfg(not(feature = "sc-impersonate"))]
            {
                Err(ScInitError::ImpersonateUnavailable)
            }
        }
        ScHttpMode::Auto => {
            #[cfg(feature = "sc-impersonate")]
            {
                match WreqClient::new() {
                    Ok(c) => Ok(std::sync::Arc::new(c)),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "AULOS_SC_HTTP=auto: the impersonating client would not build; \
                             StreamingCommunity will use the plain HTTP client and may be \
                             blocked by Cloudflare"
                        );
                        Ok(std::sync::Arc::new(PlainClient::new()?))
                    }
                }
            }
            #[cfg(not(feature = "sc-impersonate"))]
            {
                tracing::warn!(
                    "AULOS_SC_HTTP=auto but this build has no `sc-impersonate` feature; \
                     StreamingCommunity will use the plain HTTP client and may be blocked by \
                     Cloudflare"
                );
                Ok(std::sync::Arc::new(PlainClient::new()?))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use aulos_core::config::{self, RawEnv};

    use super::*;

    fn cfg(mode: &str) -> Config {
        config::load(&RawEnv::from_pairs([("AULOS_SC_HTTP", mode)])).expect("config")
    }

    #[test]
    fn an_inertia_request_carries_the_three_design_headers() {
        let req = ScReq::inertia(
            Url::parse("https://sc.test/it/watch/1").expect("url"),
            "abc123",
        );
        let names: Vec<&str> = req.headers.iter().map(|(k, _)| &**k).collect();
        assert_eq!(names, ["x-inertia", "x-inertia-version", "accept"]);
        assert_eq!(&*req.headers[1].1, "abc123");
        assert_eq!(&*req.headers[2].1, "application/json");
    }

    #[test]
    fn error_for_status_reproduces_raise_for_status() {
        let ok = ScRes {
            status: 200,
            url: Url::parse("https://sc.test/it").expect("url"),
            body: "{}".to_owned(),
        };
        assert!(ok.clone().error_for_status().is_ok());
        let bad = ScRes { status: 503, ..ok };
        let err = bad.error_for_status().expect_err("503 must raise");
        assert_eq!(err.code(), crate::ScErrorCode::Status);
    }

    #[test]
    fn a_non_json_body_is_a_distinct_error() {
        let res = ScRes {
            status: 200,
            url: Url::parse("https://sc.test/it").expect("url"),
            body: "<html>nope</html>".to_owned(),
        };
        assert_eq!(
            res.json().expect_err("html is not json").code(),
            crate::ScErrorCode::BadJson
        );
    }

    #[test]
    fn the_cookie_recorder_is_insertion_ordered_and_last_write_wins() {
        let jar = CookieRecorder::default();
        jar.record("a", "1");
        jar.record("b", "2");
        jar.record("a", "3");
        assert_eq!(jar.header(), "a=3; b=2");
        assert_eq!(CookieRecorder::default().header(), "");
    }

    #[test]
    fn the_plain_client_builds_and_reports_no_impersonation() {
        let c = PlainClient::new().expect("plain client");
        assert!(!c.impersonating());
        assert_eq!(c.cookie_header(), "");
    }

    #[test]
    fn plain_mode_always_yields_the_plain_client() {
        let c = build_client(&cfg("plain")).expect("plain");
        assert!(!c.impersonating());
    }

    #[test]
    fn auto_never_fails_over_a_missing_feature() {
        // With the feature on this is the impersonating client; with it off it is the plain one
        // plus a boot WARN. Either way `auto` starts.
        assert!(build_client(&cfg("auto")).is_ok());
    }

    #[test]
    #[cfg(not(feature = "sc-impersonate"))]
    fn explicit_impersonate_without_the_feature_is_an_init_error() {
        assert_eq!(
            build_client(&cfg("impersonate")).err(),
            Some(ScInitError::ImpersonateUnavailable)
        );
    }

    #[test]
    #[cfg(feature = "sc-impersonate")]
    fn the_chrome_profile_is_accepted_by_boringssl() {
        // `ClientBuilder::build()` constructs the connector eagerly, so this is a real assertion
        // that the cipher, curve and sigalg strings parse — the failure mode that would otherwise
        // only show up on the first download in production.
        let c = WreqClient::new().expect("wreq client");
        assert!(
            c.impersonating(),
            "the Chrome TLS profile was rejected and the client silently downgraded"
        );
        let selected = build_client(&cfg("impersonate")).expect("impersonate");
        assert!(selected.impersonating());
    }
}
