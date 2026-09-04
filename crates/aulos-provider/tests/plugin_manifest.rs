//! One test per rejection reason in DESIGN §6.5.2, plus the clamps, the catalogue and the
//! `[[hook]]` schema (PLAN WP-10 acceptance list).
//!
//! Each rejection asserts the **`Degraded` message** an operator would read in `healthz`, not
//! merely that an error happened: the whole point of load-time validation is that the reason is
//! actionable, and a test that only checks `is_err()` would let the message rot.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use aulos_core::catalog::NamingPolicy;
use aulos_core::selection::DownloadType;
use aulos_core::status::TerminalStatus;
use aulos_provider::command::{
    DEFAULT_MAX_OUTPUT_BYTES, HookAction, HttpMethod, MAX_CONCURRENT_CAP, MAX_TIMEOUT_SECS,
};
use common::{Plugin, shipped_example};

/// A manifest that loads, so each test can change exactly one thing.
const OK: &str = r#"
manifest_version = 1
name    = "Example"
version = "1.0.0"

[match]
hosts = ["example.test"]

[download]
command = ["/bin/sh", "-c", "true"]
"#;

/// `OK` with `extra` appended — i.e. inside the trailing `[download]` table, or opening a new
/// table of its own.
fn with(extra: &str) -> String {
    format!("{OK}{extra}")
}

/// `OK` with `extra` added to the `[match]` table.
fn with_match(extra: &str) -> String {
    OK.replace(
        r#"hosts = ["example.test"]"#,
        &format!("hosts = [\"example.test\"]\n{extra}"),
    )
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

#[test]
fn manifest_version_must_be_one() {
    let p = Plugin::new(
        "example",
        &OK.replace("manifest_version = 1", "manifest_version = 2"),
    );
    assert_eq!(p.reason(), "manifest_version: unsupported manifest_version");
    // Absent is the same rejection: a manifest with no version is not a v1 manifest.
    let p = Plugin::new("example", &OK.replace("manifest_version = 1\n", ""));
    assert_eq!(p.reason(), "manifest_version: unsupported manifest_version");
}

#[test]
fn name_and_version_must_be_non_empty() {
    let p = Plugin::new(
        "example",
        &OK.replace(r#"name    = "Example""#, r#"name = """#),
    );
    assert_eq!(p.reason(), "name: must be a non-empty string");
    let p = Plugin::new(
        "example",
        &OK.replace(r#"version = "1.0.0""#, r#"version = "  ""#),
    );
    assert_eq!(p.reason(), "version: must be a non-empty string");
}

#[test]
fn the_directory_name_must_match_the_pattern() {
    for bad in ["Bandcamp", "my plugin", "_leading"] {
        let p = Plugin::new(bad, OK);
        assert_eq!(
            p.reason(),
            format!("plugin directory: {bad:?} does not match ^[a-z0-9][a-z0-9_-]{{0,31}}$")
        );
    }
    // …and a legal one loads.
    assert!(Plugin::new("my-plugin_2", OK).load().is_ok());
}

// ---------------------------------------------------------------------------
// [match]
// ---------------------------------------------------------------------------

#[test]
fn a_provider_needs_hosts_or_a_host_regex() {
    let p = Plugin::new("example", &OK.replace(r#"hosts = ["example.test"]"#, ""));
    assert_eq!(
        p.reason(),
        "match: at least one of match.hosts or match.host_regex is required"
    );
}

#[test]
fn a_provider_needs_a_match_section_and_a_download_section() {
    let no_match = Plugin::new(
        "example",
        r#"
manifest_version = 1
name    = "Example"
version = "1.0.0"
[download]
command = ["/bin/sh"]
"#,
    );
    assert_eq!(
        no_match.reason(),
        "match: a provider manifest needs a [match] section"
    );
    let no_download = Plugin::new(
        "example",
        r#"
manifest_version = 1
name    = "Example"
version = "1.0.0"
[match]
hosts = ["example.test"]
"#,
    );
    assert_eq!(
        no_download.reason(),
        "download: a provider manifest needs a [download] section"
    );
    // Neither a provider nor a hook is the third rejection.
    let nothing = Plugin::new(
        "example",
        "manifest_version = 1\nname = \"E\"\nversion = \"1\"\n",
    );
    assert_eq!(
        nothing.reason(),
        "match: manifest declares neither a provider ([match] + [download]) nor a [[hook]]"
    );
}

#[test]
fn every_regex_must_compile() {
    let p = Plugin::new("example", &with_match("host_regex = '(unclosed'"));
    let reason = p.reason();
    assert!(reason.starts_with("match.host_regex: "), "{reason}");
    assert!(reason.contains("does not compile"), "{reason}");

    let p = Plugin::new("example", &with_match("path_regex = '[[['"));
    assert!(
        p.reason().starts_with("match.path_regex: "),
        "{}",
        p.reason()
    );

    let p = Plugin::new("example", &with_match("exclude_path_regex = '*'"));
    assert!(
        p.reason().starts_with("match.exclude_path_regex: "),
        "{}",
        p.reason()
    );
}

#[test]
fn an_unanchored_host_regex_is_anchored_with_a_warning() {
    let p = Plugin::new("example", &with_match("host_regex = 'example\\.test'"));
    let manifest = p
        .load()
        .expect("an unanchored regex is a warning, not a rejection");
    let warning = manifest
        .warnings
        .iter()
        .find(|w| &*w.key == "match.host_regex")
        .expect("a warning about the anchor");
    assert!(warning.message.contains("not anchored"), "{warning:?}");
    // The compiled regex really is anchored: a longer host must not match.
    let spec = manifest.match_spec.as_ref().unwrap();
    let re = spec.host_regex.as_ref().unwrap();
    assert!(re.is_match("example.test"));
    assert!(!re.is_match("notexample.testing"));
}

#[test]
fn priority_must_fit_in_a_score() {
    let p = Plugin::new("example", &with_match("priority = 300"));
    assert_eq!(p.reason(), "match.priority: 300 is outside 0..=255");
    let manifest = Plugin::new("example", &with_match("priority = 201"))
        .load()
        .unwrap();
    assert_eq!(manifest.match_spec.unwrap().priority, Some(201));
}

// ---------------------------------------------------------------------------
// Commands and templates
// ---------------------------------------------------------------------------

#[test]
fn the_download_command_must_be_a_real_executable() {
    let p = Plugin::new(
        "example",
        &OK.replace(r#"command = ["/bin/sh", "-c", "true"]"#, "command = []"),
    );
    assert_eq!(
        p.reason(),
        "download.command: must be a non-empty argv array"
    );

    let p = Plugin::new(
        "example",
        &OK.replace(
            r#"command = ["/bin/sh", "-c", "true"]"#,
            r#"command = ["definitely-not-a-real-binary-42"]"#,
        ),
    );
    assert_eq!(
        p.reason(),
        "download.command[0]: \"definitely-not-a-real-binary-42\" is not an existing executable in the plugin directory or on PATH"
    );

    // A plugin-local script counts, once it is executable.
    let p = Plugin::new(
        "example",
        &OK.replace(
            r#"command = ["/bin/sh", "-c", "true"]"#,
            r#"command = ["dl.sh"]"#,
        ),
    )
    .file("dl.sh", "#!/bin/sh\n");
    assert!(p.reason().contains("is not an existing executable"));
    let p = Plugin::new(
        "example",
        &OK.replace(
            r#"command = ["/bin/sh", "-c", "true"]"#,
            r#"command = ["dl.sh"]"#,
        ),
    )
    .script("dl.sh", "#!/bin/sh\n");
    assert!(p.load().is_ok());
}

#[test]
fn the_program_name_may_not_be_templated() {
    let p = Plugin::new(
        "example",
        &OK.replace(
            r#"command = ["/bin/sh", "-c", "true"]"#,
            r#"command = ["{plugin_dir}/dl.sh"]"#,
        ),
    );
    assert_eq!(
        p.reason(),
        "download.command[0]: the program name may not contain a template token"
    );
}

#[test]
fn an_unknown_template_token_is_rejected_at_load_time() {
    let p = Plugin::new(
        "example",
        &OK.replace(
            r#"command = ["/bin/sh", "-c", "true"]"#,
            r#"command = ["/bin/sh", "-c", "cp {out_dirr}"]"#,
        ),
    );
    assert_eq!(
        p.reason(),
        "download.command[2]: unknown token {out_dirr} at offset 3"
    );
    // A hook-table token is not a provider token, and says so.
    let p = Plugin::new(
        "example",
        &OK.replace(
            r#"command = ["/bin/sh", "-c", "true"]"#,
            r#"command = ["/bin/sh", "-c", "{status}"]"#,
        ),
    );
    assert_eq!(
        p.reason(),
        "download.command[2]: token {status} at offset 0 is not available to a provider template"
    );
    // The same applies to `[headers]` and `[env]`.
    let p = Plugin::new("example", &with("[headers]\nReferer = \"{nope}\"\n"));
    assert_eq!(
        p.reason(),
        "headers.Referer: unknown token {nope} at offset 0"
    );
    let p = Plugin::new("example", &with("[env]\nset = { X = \"{nope}\" }\n"));
    assert_eq!(p.reason(), "env.set.X: unknown token {nope} at offset 0");
}

#[test]
fn a_state_field_token_needs_capabilities_resolve() {
    let p = Plugin::new(
        "example",
        &OK.replace(
            r#"command = ["/bin/sh", "-c", "true"]"#,
            r#"command = ["/bin/sh", "-c", "{state.stream_id}"]"#,
        ),
    );
    assert_eq!(
        p.reason(),
        "download.command[2]: token {state.stream_id} needs capabilities.resolve = true"
    );

    // With `resolve = true` (and therefore a `[resolve]` section) it is accepted.
    let manifest = Plugin::new(
        "example",
        r#"
manifest_version = 1
name    = "Example"
version = "1.0.0"
[match]
hosts = ["example.test"]
[capabilities]
resolve = true
[resolve]
command = ["/bin/sh", "-c", "true"]
[download]
command = ["/bin/sh", "-c", "{state.stream_id}"]
"#,
    )
    .load()
    .expect("a resolving plugin may read {state.<key>}");
    assert!(manifest.capabilities.resolve);
    assert!(manifest.resolve.is_some());
}

#[test]
fn a_resolving_plugin_needs_a_resolve_section() {
    let p = Plugin::new("example", &with("[capabilities]\nresolve = true\n"));
    assert_eq!(
        p.reason(),
        "resolve: capabilities.resolve = true needs a [resolve] section"
    );
}

#[test]
fn enum_valued_keys_name_their_alternatives() {
    let cases = [
        (
            "[capabilities]\ncancel = \"maybe\"\n",
            "capabilities.cancel: \"maybe\" is not one of process_group | cooperative | none",
        ),
        (
            "expect_output = \"whatever\"\n",
            "download.expect_output: \"whatever\" is not one of path_template | result_frame | newest_in_dir",
        ),
        (
            "stdin = \"yaml\"\n",
            "download.stdin: \"yaml\" is not one of none | json",
        ),
    ];
    for (extra, want) in cases {
        // The `[download]`-scoped keys have to land inside that table, which `with` appends after.
        let manifest = if extra.starts_with('[') {
            with(extra)
        } else {
            OK.replace(
                r#"command = ["/bin/sh", "-c", "true"]"#,
                &format!("command = [\"/bin/sh\", \"-c\", \"true\"]\n{extra}"),
            )
        };
        assert_eq!(Plugin::new("example", &manifest).reason(), want);
    }
}

// ---------------------------------------------------------------------------
// [progress]
// ---------------------------------------------------------------------------

#[test]
fn progress_patterns_are_validated_at_load_time() {
    let p = Plugin::new(
        "example",
        &with("[progress]\nkind = \"regex\"\npatterns = ['(?P<percent>[']\n"),
    );
    assert!(
        p.reason().starts_with("progress.patterns[0]: "),
        "{}",
        p.reason()
    );

    let p = Plugin::new(
        "example",
        &with("[progress]\nkind = \"regex\"\npatterns = ['(?P<pct>[0-9]+)%']\n"),
    );
    assert!(
        p.reason().contains("capture group \"pct\""),
        "{}",
        p.reason()
    );

    let p = Plugin::new(
        "example",
        &with("[progress]\nkind = \"regex\"\npatterns = ['[0-9]+%']\n"),
    );
    assert!(
        p.reason().contains("no named capture group"),
        "{}",
        p.reason()
    );

    let p = Plugin::new("example", &with("[progress]\nkind = \"regex\"\n"));
    assert_eq!(
        p.reason(),
        "progress.patterns: progress.kind = \"regex\" needs at least one pattern"
    );

    let p = Plugin::new(
        "example",
        &with("[progress]\nkind = \"none\"\nstatus_map = { fetch = \"running\" }\n"),
    );
    assert!(
        p.reason().starts_with("progress.status_map.fetch: "),
        "{}",
        p.reason()
    );
}

// ---------------------------------------------------------------------------
// [catalog]
// ---------------------------------------------------------------------------

#[test]
fn catalog_ids_must_be_lower_snake_and_unique() {
    let bad_id = with(
        r#"
[[catalog.download_types]]
id = "Audio"
  [[catalog.download_types.formats]]
  id = "flac"
"#,
    );
    assert_eq!(
        Plugin::new("example", &bad_id).reason(),
        "catalog.download_types.id: \"Audio\" does not match ^[a-z0-9_]+$"
    );

    let duplicate = with(
        r#"
[[catalog.download_types]]
id = "audio"
  [[catalog.download_types.formats]]
  id = "flac"
  [[catalog.download_types.formats]]
  id = "flac"
"#,
    );
    assert_eq!(
        Plugin::new("example", &duplicate).reason(),
        "catalog.download_types.formats.id: \"flac\" is declared twice"
    );

    let empty = with("[[catalog.download_types]]\nid = \"audio\"\n");
    assert_eq!(
        Plugin::new("example", &empty).reason(),
        "catalog.download_types.audio.formats: must declare at least one format"
    );

    let unknown_default = with(
        r#"
[[catalog.download_types]]
id = "audio"
default_format = "mp3"
  [[catalog.download_types.formats]]
  id = "flac"
"#,
    );
    assert_eq!(
        Plugin::new("example", &unknown_default).reason(),
        "catalog.download_types.default_format: \"mp3\" is not one of [\"flac\"]"
    );
}

#[test]
fn a_manifest_without_a_catalog_gets_an_honest_single_entry_one() {
    let manifest = Plugin::new(
        "example",
        &OK.replace(
            r#"command = ["/bin/sh", "-c", "true"]"#,
            "command = [\"/bin/sh\", \"-c\", \"true\"]\noutput_ext = \"flac\"",
        ),
    )
    .load()
    .unwrap();
    let catalog = &manifest.catalog;
    assert_eq!(catalog.naming, NamingPolicy::Template);
    assert_eq!(catalog.download_types.len(), 1);
    let dt = &catalog.download_types[0];
    assert_eq!(&*dt.id, "video");
    assert_eq!(dt.formats.len(), 1);
    assert_eq!(&*dt.formats[0].id, "flac");
    assert_eq!(&*dt.default_format, "flac");
    assert_eq!(&*dt.formats[0].default_quality, "best");
    let notice = dt.formats[0].notice.as_deref().expect("an honest notice");
    assert!(notice.contains("no [catalog]"), "{notice}");
}

// ---------------------------------------------------------------------------
// [limits]
// ---------------------------------------------------------------------------

#[test]
fn limits_outside_the_hard_caps_are_clamped_with_a_warning() {
    let manifest = Plugin::new(
        "example",
        &with(
            r#"
[limits]
max_concurrent             = 512
max_concurrent_resolves    = 0
download_hard_timeout_secs = 999999
max_output_bytes           = 7
"#,
        ),
    )
    .load()
    .expect("a bad limit is a clamp, not a rejection");
    assert_eq!(manifest.limits.max_concurrent, MAX_CONCURRENT_CAP);
    assert_eq!(manifest.limits.max_concurrent_resolves, 1);
    assert_eq!(manifest.limits.download_hard_timeout_secs, MAX_TIMEOUT_SECS);
    assert_eq!(manifest.limits.max_output_bytes, 4096);
    let keys: Vec<&str> = manifest.warnings.iter().map(|w| &*w.key).collect();
    assert_eq!(
        keys,
        [
            "limits.max_concurrent",
            "limits.max_concurrent_resolves",
            "limits.download_hard_timeout_secs",
            "limits.max_output_bytes"
        ]
    );
}

#[test]
fn the_limit_defaults_are_the_design_table() {
    let manifest = Plugin::new("example", OK).load().unwrap();
    let l = manifest.limits;
    assert_eq!(l.max_concurrent, 1);
    assert!(l.uses_global_slot);
    assert_eq!(l.max_concurrent_resolves, 1);
    assert_eq!(l.min_request_interval_ms, 0);
    assert_eq!(l.resolve_timeout_secs, 60);
    assert_eq!(l.download_stall_secs, 600);
    assert_eq!(l.download_hard_timeout_secs, 0);
    assert_eq!(l.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    assert_eq!(l.memory_bytes, 0);
    assert_eq!(l.cpu_secs, 0);
    assert_eq!(l.nofile, 1024);
    assert!(manifest.warnings.is_empty());
    // …and the `[download]` defaults.
    let d = manifest.download.as_ref().unwrap();
    assert_eq!(&*d.output_ext, "mp4");
    assert!(d.overwrite);
}

// ---------------------------------------------------------------------------
// Filesystem safety
// ---------------------------------------------------------------------------

#[test]
fn a_world_writable_plugin_directory_is_refused() {
    let p = Plugin::new("example", OK).chmod_dir(0o777);
    let reason = p.reason();
    assert!(
        reason
            .starts_with("plugin directory: refusing to execute out of a world-writable directory"),
        "{reason}"
    );
    // Restore the mode so the temp dir can be cleaned up.
    let _ = p.chmod_dir(0o755);
}

#[test]
fn a_setuid_or_world_writable_file_is_refused() {
    let p = Plugin::new("example", OK)
        .script("dl.sh", "#!/bin/sh\n")
        .chmod_file("dl.sh", 0o4755);
    let reason = p.reason();
    assert!(reason.contains("dl.sh is setuid/setgid"), "{reason}");

    let p = Plugin::new("example", OK)
        .script("dl.sh", "#!/bin/sh\n")
        .chmod_file("dl.sh", 0o777);
    assert!(
        p.reason().contains("dl.sh is world-writable"),
        "{}",
        p.reason()
    );
}

// ---------------------------------------------------------------------------
// Syntax
// ---------------------------------------------------------------------------

#[test]
fn a_syntax_error_carries_the_toml_span_and_claims_no_urls() {
    let p = Plugin::new("example", "manifest_version = = 1\n");
    let reason = p.reason();
    assert!(reason.contains("plugin.toml"), "{reason}");
    // One line, so a `healthz` payload stays readable.
    assert!(!reason.contains('\n'), "{reason}");
    // A syntax error cannot be turned into a matcher, so the directory is a hard failure.
    let err = p.load().unwrap_err();
    assert!(!err.has_partial_match());

    let missing = Plugin::new("example", OK);
    std::fs::remove_file(missing.dir().join("plugin.toml")).unwrap();
    assert!(!missing.load().unwrap_err().has_partial_match());
}

// ---------------------------------------------------------------------------
// [[hook]]
// ---------------------------------------------------------------------------

#[test]
fn the_shipped_media_server_hooks_example_loads() {
    let example = shipped_example("media-server-hooks");
    let manifest = example
        .load()
        .unwrap_or_else(|e| panic!("plugins/examples/media-server-hooks: {}", e.reason()));

    // A hook-only manifest is valid and declares no provider (DESIGN §6.5.1).
    assert!(!manifest.declares_provider());
    assert!(manifest.match_spec.is_none());
    assert!(manifest.download.is_none());
    assert!(manifest.catalog.download_types.is_empty());

    let ids: Vec<&str> = manifest.hooks.iter().map(|h| &*h.id).collect();
    assert_eq!(
        ids,
        [
            "hook:media-server-hooks/plex",
            "hook:media-server-hooks/emby",
            "hook:media-server-hooks/ntfy",
            "hook:media-server-hooks/post-process"
        ]
    );

    let plex = &manifest.hooks[0];
    assert_eq!(plex.on, [TerminalStatus::Finished]);
    assert_eq!(plex.debounce_ms, 30_000);
    assert_eq!(plex.max_wait_ms, 300_000);
    assert_eq!(plex.ordering, 50);
    match &plex.action {
        HookAction::Http { method, url, .. } => {
            assert_eq!(*method, HttpMethod::Get);
            // `${PLEX_TOKEN}` interpolated at load time; unset in the test environment, so it
            // substitutes empty and warns rather than failing.
            assert!(url.as_str().contains("X-Plex-Token="), "{}", url.as_str());
        }
        other => panic!("expected an http hook, got {other:?}"),
    }
    assert!(
        manifest
            .warnings
            .iter()
            .any(|w| w.message.contains("PLEX_TOKEN")),
        "an unset ${{VAR}} must be visible: {:?}",
        manifest.warnings
    );

    let ntfy = &manifest.hooks[2];
    assert_eq!(ntfy.on, [TerminalStatus::Finished, TerminalStatus::Error]);
    assert_eq!(ntfy.debounce_ms, 0);

    let script = &manifest.hooks[3];
    assert_eq!(script.timeout_ms, 60_000);
    assert_eq!(script.when.download_type, [DownloadType::Video]);
    assert!(matches!(script.action, HookAction::Command { .. }));
}

#[test]
fn a_provider_may_also_declare_hooks() {
    let manifest = Plugin::new(
        "example",
        &with(
            r#"
[[hook]]
id = "ping"
on = ["finished", "error"]
http = { url = "https://hooks.example.test/aulos" }
"#,
        ),
    )
    .load()
    .expect("a manifest may be both a provider and a hook");
    assert!(manifest.declares_provider());
    assert_eq!(manifest.hooks.len(), 1);
    assert_eq!(&*manifest.hooks[0].id, "hook:example/ping");
}

#[test]
fn hook_rejections_name_the_hook_and_the_key() {
    let cases = [
        (
            "[[hook]]\nid = \"x\"\non = [\"downloading\"]\nhttp = { url = \"http://h/x\" }\n",
            "hook[0].on: \"downloading\" is not one of finished | error | canceled",
        ),
        (
            "[[hook]]\nid = \"x\"\non = [\"finished\"]\ndebounce_ms = 3600001\nhttp = { url = \"http://h/x\" }\n",
            "hook[0].debounce_ms: 3600001 exceeds the one-hour cap of 3600000",
        ),
    ];
    for (extra, want) in cases {
        assert_eq!(Plugin::new("example", &with(extra)).reason(), want);
    }
    let bad_url = Plugin::new(
        "example",
        &with("[[hook]]\nid = \"x\"\non = [\"finished\"]\nhttp = { url = \"nope\" }\n"),
    );
    assert!(
        bad_url.reason().starts_with("hook[0].http.url: "),
        "{}",
        bad_url.reason()
    );
}

// ---------------------------------------------------------------------------
// The provider id and the fingerprint
// ---------------------------------------------------------------------------

#[test]
fn the_provider_id_is_command_dirname_and_the_fingerprint_tracks_the_bytes() {
    let a = Plugin::new("bandcamp", OK).load().unwrap();
    assert_eq!(a.provider_id().as_str(), "command:bandcamp");
    let b = Plugin::new("bandcamp", OK).load().unwrap();
    assert_eq!(a.fingerprint, b.fingerprint, "same bytes, same fingerprint");
    let c = Plugin::new("bandcamp", &with("# a comment\n"))
        .load()
        .unwrap();
    assert_ne!(
        a.fingerprint, c.fingerprint,
        "changed bytes, new fingerprint"
    );
}
