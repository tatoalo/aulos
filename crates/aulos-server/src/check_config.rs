//! `aulos-server check-config` (DESIGN §3.1, §17.1).
//!
//! Parses the environment and `YTDL_OPTIONS*` exactly as `serve` would, prints the effective
//! configuration with every secret rendered as `«redacted»`, and exits 0 on success or 1 on
//! invalid configuration. It binds no port and touches no database, so it is safe to run against a
//! production compose file.
//!
//! Two properties are deliberate:
//!
//! - **Every** error is reported, not the first one. Fixing a compose file one boot at a time is
//!   the experience `aulos_core::config::load` was written to avoid (DESIGN §17.1 step 9), and this
//!   command is where that pays off.
//! - `YTDL_OPTIONS_FILE` **is** read here. It is the one part of the configuration that needs the
//!   filesystem, and it is also the part most likely to be wrong, so leaving it out would make a
//!   green `check-config` a weaker promise than it looks.

use std::fmt::Write as _;

use aulos_core::config::{self, ConfigWarning, RawEnv};
use aulos_core::ytdl_options::YtdlOptions;

/// The exit code `main` returns for a valid configuration.
pub const EXIT_OK: i32 = 0;

/// The exit code for an invalid one (DESIGN §3.1: "exit 0 on success and 1 on invalid config").
pub const EXIT_INVALID: i32 = 1;

/// Runs the check against an explicit environment and returns the rendered report plus the exit
/// code.
///
/// Split from [`run`] so the acceptance tests can drive it without touching the process
/// environment.
#[must_use]
pub fn check(env: &RawEnv) -> (String, i32) {
    let mut out = String::with_capacity(4_096);
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();

    let cfg = match config::load_with_warnings(env) {
        Ok((cfg, warns)) => {
            warnings.extend(warns.iter().map(ConfigWarning::to_string));
            Some(cfg)
        }
        Err(errs) => {
            errors.extend(errs.iter().map(std::string::ToString::to_string));
            None
        }
    };

    // Step 8 of the loading algorithm: the two option files, whose errors join the same report.
    if let Some(cfg) = cfg.as_ref()
        && let Err(e) = YtdlOptions::load(
            &cfg.ytdl_options,
            cfg.ytdl_options_file.as_deref(),
            &cfg.ytdl_options_presets,
            cfg.ytdl_options_presets_file.as_deref(),
        )
    {
        errors.push(e.to_string());
    }

    // The effective values, redacted. Available even when typing failed, because seeing the value
    // a key resolved to is usually how the operator finds the typo.
    match env.effective_redacted() {
        Ok(effective) => {
            let _ = writeln!(out, "effective configuration ({} keys):", effective.len());
            for (key, value) in &effective {
                let _ = writeln!(out, "  {key} = {value}");
            }
        }
        Err(errs) => {
            for e in &errs {
                let rendered = e.to_string();
                if !errors.contains(&rendered) {
                    errors.push(rendered);
                }
            }
        }
    }

    if !warnings.is_empty() {
        let _ = writeln!(out, "\nwarnings ({}):", warnings.len());
        for w in &warnings {
            let _ = writeln!(out, "  {w}");
        }
    }

    if errors.is_empty() {
        let _ = writeln!(out, "\nconfiguration is valid");
        (out, EXIT_OK)
    } else {
        let _ = writeln!(out, "\nerrors ({}):", errors.len());
        for e in &errors {
            let _ = writeln!(out, "  {e}");
        }
        let _ = writeln!(out, "\nconfiguration is INVALID");
        (out, EXIT_INVALID)
    }
}

/// Runs the check against the process environment, printing the report.
#[must_use]
pub fn run() -> i32 {
    let (report, code) = check(&RawEnv::from_process());
    print!("{report}");
    code
}

#[cfg(test)]
mod tests {
    use super::*;
    use aulos_core::REDACTED;

    fn env(pairs: &[(&str, &str)]) -> RawEnv {
        RawEnv::from_pairs(pairs.iter().copied())
    }

    #[test]
    fn a_valid_environment_exits_zero_and_prints_the_effective_values() {
        let (out, code) = check(&env(&[("DOWNLOAD_DIR", "/downloads"), ("PORT", "8081")]));
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("effective configuration"), "{out}");
        assert!(out.contains("PORT = 8081"), "{out}");
        assert!(out.contains("configuration is valid"), "{out}");
        assert!(!out.contains("INVALID"), "{out}");
    }

    #[test]
    fn secrets_are_redacted() {
        let (out, code) = check(&env(&[
            ("TELEGRAM_BOT_TOKEN", "123456:super-secret"),
            ("JELLYFIN_API_KEY", "abcdef"),
            ("AULOS_API_TOKEN", "hunter2"),
        ]));
        assert_eq!(code, EXIT_OK, "{out}");
        for secret in ["super-secret", "abcdef", "hunter2"] {
            assert!(!out.contains(secret), "{secret} leaked:\n{out}");
        }
        assert!(out.contains(REDACTED), "{out}");
    }

    #[test]
    fn an_invalid_environment_exits_one_and_reports_every_error() {
        let (out, code) = check(&env(&[
            ("PORT", "eighty"),
            ("AULOS_WS_BATCH_MS", "nope"),
            ("AULOS_TYPO_HERE", "1"),
        ]));
        assert_eq!(code, EXIT_INVALID, "{out}");
        assert!(out.contains("configuration is INVALID"), "{out}");
        assert!(out.contains("PORT"), "{out}");
        assert!(out.contains("AULOS_WS_BATCH_MS"), "{out}");
        assert!(out.contains("AULOS_TYPO_HERE"), "{out}");
    }

    #[test]
    fn a_bad_ytdl_options_file_is_part_of_the_same_report() {
        let (out, code) = check(&env(&[("YTDL_OPTIONS_FILE", "/nonexistent/opts.json")]));
        assert_eq!(code, EXIT_INVALID, "{out}");
        assert!(out.contains("not found"), "{out}");
    }

    #[test]
    fn a_lenient_value_is_a_warning_not_an_error() {
        let (out, code) = check(&env(&[("CLEAR_COMPLETED_AFTER", "banana")]));
        assert_eq!(code, EXIT_OK, "{out}");
        assert!(out.contains("warnings ("), "{out}");
        assert!(out.contains("CLEAR_COMPLETED_AFTER"), "{out}");
    }
}
