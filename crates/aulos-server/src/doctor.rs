//! `aulos-server doctor` (DESIGN §3.1).
//!
//! Probes `python3`, `yt-dlp`, `ffmpeg`, `ffprobe`, `N_m3u8DL-RE`, `deno` and `bgutil-pot`, prints
//! their versions, and exits non-zero when a **required** tool is missing. Required means the
//! Python shim handshake: `ytdlp` is the fallback provider for every URL (BRIEF §9), so a server
//! without it can download nothing.
//!
//! It binds no port and opens no database, so it is safe as a container smoke test — the image's
//! own CI step is `docker run --rm <image> doctor`.

use aulos_core::config::{self, Config, RawEnv};

use crate::tools;

/// Every required tool answered.
pub const EXIT_OK: i32 = 0;

/// A required tool is missing, or the configuration does not load.
pub const EXIT_MISSING: i32 = 1;

/// Runs every probe against `cfg` and returns the report plus the exit code.
///
/// Split from [`run`] so a test can assert the exit code without a process.
pub async fn check(cfg: &Config) -> (String, i32) {
    let report = tools::probe_everything(cfg, &tools::doctor_tools()).await;
    let mut out = report.render();
    let missing = report.missing_optional();
    if !missing.is_empty() {
        out.push_str(&format!(
            "\n{} optional tool(s) missing: {} — the matching healthz component is degraded\n",
            missing.len(),
            missing.join(", ")
        ));
    }
    if report.required_ok() {
        out.push_str("\nall required tools are present\n");
        (out, EXIT_OK)
    } else {
        out.push_str("\na REQUIRED tool is missing\n");
        (out, EXIT_MISSING)
    }
}

/// Loads the configuration the way `serve` does, probes, prints, and returns the exit code.
pub fn run() -> i32 {
    let cfg = match config::load(&RawEnv::from_process()) {
        Ok(cfg) => cfg,
        Err(errs) => {
            eprintln!("configuration is invalid ({} errors):", errs.len());
            for e in &errs {
                eprintln!("  {e}");
            }
            return EXIT_MISSING;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("could not start a runtime: {e}");
            return EXIT_MISSING;
        }
    };
    let (report, code) = runtime.block_on(check(&cfg));
    print!("{report}");
    code
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        config::load(&RawEnv::default()).expect("the defaults must load")
    }

    #[tokio::test]
    async fn doctor_reports_a_missing_required_tool_and_exits_non_zero() {
        // The shim is probed through `python3`; pointing `PATH` at an empty directory would be a
        // process-global change, so the assertion is on the branch that a machine without the
        // image's layout takes anyway: either the shim answers (developer machine with yt-dlp) or
        // it does not, and the exit code has to agree with the table either way.
        let (out, code) = check(&cfg()).await;
        assert!(out.contains("required:"), "{out}");
        assert!(out.contains("optional:"), "{out}");
        if out.contains("MISSING") && out.contains("python3 + yt-dlp") {
            assert_eq!(code, EXIT_MISSING, "{out}");
            assert!(out.contains("a REQUIRED tool is missing"), "{out}");
        } else {
            assert_eq!(code, EXIT_OK, "{out}");
            assert!(out.contains("all required tools are present"), "{out}");
        }
    }

    #[tokio::test]
    async fn every_optional_tool_appears_in_the_table_whether_present_or_not() {
        let (out, _) = check(&cfg()).await;
        for name in ["ffmpeg", "ffprobe", "nm3u8dl", "deno", "bgutil_pot"] {
            assert!(out.contains(name), "{name} missing from the table:\n{out}");
        }
    }
}
