//! WP-01 packaging gates. These are the greps the PLAN's acceptance list asks the `arch` CI job to
//! run: they catch the regressions that only show up at `docker run` time, where a test is
//! expensive and a mistake is silent.
//!
//! `tests/arch.rs` (the DESIGN §3 dependency-direction rules A1–A5) is a separate file owned by
//! WP-03; this one deliberately only reads packaging files.

use std::path::{Path, PathBuf};

/// The workspace root, derived from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{} must exist: {e}", p.display()))
}

#[test]
fn the_healthcheck_is_the_subcommand_not_a_shell_curl() {
    // Regression test for C16: a shell `curl` interpolating a raw ${URL_PREFIX} bypasses the
    // §17.1 normalisation and yields `http://127.0.0.1:8081metubehealthz`.
    let dockerfile = read("docker/Dockerfile");
    assert!(
        dockerfile.contains(r#"CMD ["/usr/local/bin/aulos-server","healthcheck"]"#),
        "the HEALTHCHECK must exec the `healthcheck` subcommand"
    );
    let healthcheck_line = dockerfile
        .lines()
        .skip_while(|l| !l.starts_with("HEALTHCHECK"))
        .take(2)
        .collect::<String>();
    assert!(
        !healthcheck_line.contains("curl") && !healthcheck_line.contains("URL_PREFIX"),
        "the HEALTHCHECK must not shell out to curl or interpolate URL_PREFIX: {healthcheck_line}"
    );
}

#[test]
fn the_image_defaults_to_serve_through_the_entrypoint() {
    let dockerfile = read("docker/Dockerfile");
    assert!(dockerfile.contains(r#"CMD ["serve"]"#));
    assert!(
        dockerfile.contains(
            r#"ENTRYPOINT ["/usr/bin/tini","-g","--","/usr/local/bin/aulos-entrypoint"]"#
        )
    );
    let entrypoint = read("docker/entrypoint.sh");
    assert!(
        entrypoint.matches(r#"aulos-server "$@""#).count() == 2,
        "both the gosu and the non-root branch must forward \"$@\""
    );
}

#[test]
fn the_entrypoint_logs_the_chown_mode_for_all_three_values() {
    let entrypoint = read("docker/entrypoint.sh");
    for arm in ["false)", "recursive)", "*)"] {
        assert!(
            entrypoint.contains(arm),
            "CHOWN_DIRS case is missing the {arm} arm"
        );
    }
    // Each arm must log; a silent arm makes the mode unobservable in `docker logs`.
    let case_body = entrypoint
        .split("case \"${CHOWN_DIRS:-true}\" in")
        .nth(1)
        .unwrap_or_default()
        .split("esac")
        .next()
        .unwrap_or_default();
    assert_eq!(
        case_body.matches("echo ").count(),
        3,
        "every CHOWN_DIRS arm must log the mode it picked"
    );
}

#[test]
fn the_ytdlp_pin_appears_exactly_once_in_the_repo() {
    // update-yt-dlp.yml rewrites a single `ARG YTDLP_VERSION=` line; a duplicate pin would make
    // the automation silently half-apply.
    let dockerfile = read("docker/Dockerfile");
    assert_eq!(
        dockerfile.matches("ARG YTDLP_VERSION=").count(),
        1,
        "the yt-dlp pin must live in exactly one place"
    );
    // The single install must go through the ARG, never spell a literal version out again.
    assert_eq!(
        dockerfile.matches("yt-dlp==").count(),
        1,
        "`yt-dlp==` must appear once, as the ${{YTDLP_VERSION}} reference"
    );
    assert!(dockerfile.contains(r#""yt-dlp==${YTDLP_VERSION}""#));
}

#[test]
fn the_sidecar_versions_are_pinned_not_resolved_at_build_time() {
    let dockerfile = read("docker/Dockerfile");
    for arg in [
        "ARG BGUTIL_TAG=",
        "ARG NM3U8DL_VERSION=",
        "ARG NM3U8DL_BUILD=",
    ] {
        assert!(dockerfile.contains(arg), "{arg} must be pinned");
    }
    assert!(
        !dockerfile.contains("releases/latest"),
        "resolving `latest` at build time makes the image non-reproducible (DESIGN §18.1)"
    );
}

#[test]
fn docker_workflow_builds_amd64_only() {
    // BRIEF §16: no QEMU, no arm64. The Dockerfile stays arch-parametrised so arm64 is a one-line
    // change later, but the workflow must not build it yet.
    let wf = read(".github/workflows/docker.yml");
    assert!(wf.contains("platforms: linux/amd64"));
    assert!(
        !wf.contains("linux/arm64"),
        "docker.yml must build linux/amd64 only"
    );
    assert!(
        !wf.contains("setup-qemu-action"),
        "docker.yml must not set up QEMU"
    );
}

#[test]
fn only_the_workflows_the_brief_keeps_are_present() {
    // BRIEF scope trims: dev-build, update-sidecars, upstream-sync-* and the deny/coverage/
    // schema/gitleaks jobs are CUT for v1.0.
    let dir = repo_root().join(".github/workflows");
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{} must exist: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "ci.yml".to_owned(),
            "docker.yml".to_owned(),
            "release.yml".to_owned(),
            "update-yt-dlp.yml".to_owned(),
        ]
    );
}

#[test]
fn the_workspace_pins_edition_2024_and_rust_1_95() {
    let root = read("Cargo.toml");
    assert!(root.contains(r#"resolver = "3""#));
    assert!(root.contains(r#"edition = "2024""#));
    assert!(root.contains(r#"rust-version = "1.95""#));
    assert!(root.contains(r#"unwrap_used = "deny""#));
    assert!(root.contains(r#"expect_used = "warn""#));
    assert!(root.contains(r#"disallowed_methods = "deny""#));
}

#[test]
fn every_crate_in_the_design_layout_exists_and_is_a_member() {
    let expected = [
        "aulos-core",
        "aulos-store",
        "aulos-provider",
        "aulos-provider-ytdlp",
        "aulos-provider-sc",
        "aulos-queue",
        "aulos-api",
        "aulos-telegram",
        "aulos-subscriptions",
        "aulos-hooks",
        "aulos-server",
        "aulos-workspace-tests",
    ];
    for name in expected {
        let manifest = repo_root().join("crates").join(name).join("Cargo.toml");
        assert!(
            manifest.is_file(),
            "{} must exist (WP-01: nothing in wave 1 needs to create a crate)",
            manifest.display()
        );
    }
    // Twelve members, no more: an accidental thirteenth crate is an architecture change.
    let count = std::fs::read_dir(repo_root().join("crates"))
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .count()
        })
        .unwrap_or_default();
    assert_eq!(count, expected.len(), "unexpected crate directory count");
}

#[test]
fn the_workspace_tests_crate_has_no_src_dir() {
    // DESIGN §3: it exists only to own the workspace-wide gates.
    assert!(
        !Path::new(env!("CARGO_MANIFEST_DIR")).join("src").exists(),
        "aulos-workspace-tests must have no src/"
    );
}
