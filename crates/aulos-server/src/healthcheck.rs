//! `aulos-server healthcheck` — the container `HEALTHCHECK` (DESIGN §3.1, §18.1).
//!
//! It loads the configuration exactly as `serve` does and builds the URL through the same
//! [`aulos_core::prefix::Prefix`] newtype. That is the whole reason it is a subcommand rather than
//! a `curl` in the Dockerfile: a shell interpolating a raw `${URL_PREFIX}` bypasses the DESIGN
//! §17.1 normalisation and, for `URL_PREFIX=metube`, asks for
//! `http://127.0.0.1:8081metubehealthz` — which never answers, so the container is restarted
//! forever (regression C16).
//!
//! Exit 0 for any **2xx**: a missing `deno` must not make Docker kill a server that is downloading
//! fine, and DESIGN §16.3 defines the HTTP status precisely — `503` **only** when the store is
//! unusable or the WAL exceeds 256 MB, i.e. only when the service is actually useless. The body's
//! `status` word is reported as the reason rather than used as the verdict, because
//! [`aulos_core::HealthRegistry`] rolls the view up to its *worst* component: one `down` optional
//! component makes the body say `"status":"down"` while `healthz` still answers `200`, which is a
//! payload DESIGN §16.3's own example shows (`"pot": {"status":"down"}` under a top-level
//! `"status":"degraded"`) but the roll-up cannot currently produce. See
//! `docs/INTEGRATION-NOTES.md`, WP-17.

use std::time::Duration;

use aulos_core::config::{self, Config, RawEnv};

/// The server answered `ok` or `degraded`.
pub const EXIT_HEALTHY: i32 = 0;

/// Anything else: unreachable, a non-2xx, or an unparseable body.
pub const EXIT_UNHEALTHY: i32 = 1;

/// The DESIGN §3.1 request timeout.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// The loopback URL of this process's own `healthz`.
///
/// `HOST` is deliberately ignored: a server bound to `0.0.0.0` is reachable on the loopback, and a
/// server bound to a specific interface is still reachable there in the container's network
/// namespace. Using `HOST` verbatim would break the common `HOST=0.0.0.0` case, because
/// `http://0.0.0.0:8081/` is not a valid destination on every stack.
#[must_use]
pub fn healthz_url(cfg: &Config) -> String {
    let scheme = if cfg.https { "https" } else { "http" };
    format!(
        "{scheme}://127.0.0.1:{}{}",
        cfg.port,
        cfg.url_prefix.route("healthz")
    )
}

/// Requests `healthz` and decides the exit code. Returns `(message, code)`.
pub async fn check(cfg: &Config) -> (String, i32) {
    let url = healthz_url(cfg);
    let client = match reqwest::Client::builder()
        .timeout(TIMEOUT)
        // A `HTTPS=true` deployment usually terminates TLS with its own certificate, which the
        // loopback probe has no reason to validate: it is talking to the very process it is a
        // health check for.
        .danger_accept_invalid_certs(cfg.https)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                format!("could not build an HTTP client: {e}"),
                EXIT_UNHEALTHY,
            );
        }
    };

    let response = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return (format!("{url}: {e}"), EXIT_UNHEALTHY),
    };
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let health: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                format!("{url}: HTTP {status}, unparseable body: {e}"),
                EXIT_UNHEALTHY,
            );
        }
    };
    let state = health
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("<none>");
    if status.is_success() {
        (
            format!("{url}: HTTP {status}, status={state}"),
            EXIT_HEALTHY,
        )
    } else {
        (
            format!("{url}: HTTP {status}, status={state}"),
            EXIT_UNHEALTHY,
        )
    }
}

/// Loads the configuration from the environment, probes, prints one line, returns the exit code.
pub fn run() -> i32 {
    let cfg = match config::load(&RawEnv::from_process()) {
        Ok(cfg) => cfg,
        Err(errs) => {
            eprintln!("configuration is invalid ({} errors):", errs.len());
            for e in &errs {
                eprintln!("  {e}");
            }
            return EXIT_UNHEALTHY;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("could not start a runtime: {e}");
            return EXIT_UNHEALTHY;
        }
    };
    let (message, code) = runtime.block_on(check(&cfg));
    if code == EXIT_HEALTHY {
        println!("{message}");
    } else {
        eprintln!("{message}");
    }
    code
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> Config {
        config::load(&RawEnv::from_pairs(pairs.iter().copied())).expect("valid env")
    }

    #[test]
    fn the_url_goes_through_the_prefix_newtype() {
        assert_eq!(
            healthz_url(&cfg(&[("PORT", "8081")])),
            "http://127.0.0.1:8081/healthz"
        );
        // C16: the raw-`${URL_PREFIX}` shell form produced `…:8081metubehealthz`.
        assert_eq!(
            healthz_url(&cfg(&[("PORT", "8081"), ("URL_PREFIX", "metube")])),
            "http://127.0.0.1:8081/metube/healthz"
        );
        assert_eq!(
            healthz_url(&cfg(&[("PORT", "9000"), ("URL_PREFIX", "/a/b/")])),
            "http://127.0.0.1:9000/a/b/healthz"
        );
    }

    #[test]
    fn https_switches_the_scheme() {
        let c = cfg(&[("HTTPS", "true"), ("PORT", "8443")]);
        assert_eq!(healthz_url(&c), "https://127.0.0.1:8443/healthz");
    }

    #[tokio::test]
    async fn a_stopped_server_exits_one() {
        // Port 1 is privileged and never bound by this suite.
        let (msg, code) = check(&cfg(&[("PORT", "1")])).await;
        assert_eq!(code, EXIT_UNHEALTHY, "{msg}");
    }

    /// A one-shot HTTP server that answers `http` with `body`, and its port.
    ///
    /// Hand-rolled rather than `wiremock`: this crate has no such dev-dependency and the whole
    /// need is one status line and one body.
    async fn stub(http: u16, body: &'static str) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let task = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut scratch = [0_u8; 1024];
                let _ = socket.read(&mut scratch).await;
                let head = format!("HTTP/1.1 {http} X\r\n");
                let headers = format!(
                    "content-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(headers.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        (port, task)
    }

    /// The verdict is the HTTP status, not the body's word.
    ///
    /// This is the trap the HTTPS smoke run caught: `HealthRegistry` rolls the view up to its
    /// worst component, so a machine without `deno` serves `200 {"status":"down"}`. Deciding on the
    /// body would then restart a perfectly working container every two minutes.
    #[tokio::test]
    async fn a_200_is_healthy_whatever_the_body_says_and_a_503_is_not() {
        for (http, body, want) in [
            (200_u16, r#"{"status":"ok"}"#, EXIT_HEALTHY),
            (200, r#"{"status":"degraded"}"#, EXIT_HEALTHY),
            (200, r#"{"status":"down"}"#, EXIT_HEALTHY),
            (503, r#"{"status":"down"}"#, EXIT_UNHEALTHY),
        ] {
            let (port, served) = stub(http, body).await;
            let (msg, code) = check(&cfg(&[("PORT", &port.to_string())])).await;
            assert_eq!(code, want, "HTTP {http} {body}: {msg}");
            let _ = served.await;
        }
    }

    #[tokio::test]
    async fn an_unparseable_body_is_unhealthy() {
        let (port, served) = stub(200, "nope!").await;
        let (msg, code) = check(&cfg(&[("PORT", &port.to_string())])).await;
        assert_eq!(code, EXIT_UNHEALTHY, "{msg}");
        assert!(msg.contains("unparseable"), "{msg}");
        let _ = served.await;
    }
}
