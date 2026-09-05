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

// ---------------------------------------------------------------------------------------------
// The entrypoint's directory/ownership contract (DESIGN §18.2), exercised for real rather than
// grepped: the script is run under `sh` with `id`, `chown` and `gosu` stubbed on PATH, so the
// root branch is taken on an unprivileged test host and every `chown` it would have issued is
// recorded. A static grep would not have caught the audio root missing from the chown list.
// ---------------------------------------------------------------------------------------------

/// Runs `docker/entrypoint.sh` with stubbed `id`/`chown`/`gosu` and returns the recorded
/// `chown` argument lines, one per invocation.
#[cfg(unix)]
fn run_entrypoint(dirs: &EntrypointDirs<'_>, chown_dirs: &str) -> Vec<String> {
    use std::os::unix::fs::PermissionsExt;

    let tmp = dirs.root;
    let bin = tmp.join("stubbin");
    std::fs::create_dir_all(&bin).unwrap_or_else(|e| panic!("stub bin: {e}"));
    let log = tmp.join("chown.log");
    // `id` always answers 0 so the script takes its root branch; `chown` only records; `gosu`
    // stands in for the final `exec`, which would otherwise need the real binary.
    let stubs = [
        ("id", "#!/bin/sh\necho 0\n".to_owned()),
        (
            "chown",
            format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n", log.display()),
        ),
        ("gosu", "#!/bin/sh\nexit 0\n".to_owned()),
    ];
    for (name, body) in stubs {
        let p = bin.join(name);
        std::fs::write(&p, body).unwrap_or_else(|e| panic!("write {name}: {e}"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("chmod {name}: {e}"));
    }

    let script = repo_root().join("docker/entrypoint.sh");
    let mut cmd = std::process::Command::new("sh");
    cmd.arg(&script).arg("serve").env_clear();
    cmd.env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    cmd.env("UMASK", "022");
    cmd.env("PUID", "1000");
    cmd.env("PGID", "1000");
    cmd.env("CHOWN_DIRS", chown_dirs);
    cmd.env("DOWNLOAD_DIR", &dirs.download);
    cmd.env("STATE_DIR", &dirs.state);
    cmd.env("TEMP_DIR", &dirs.temp);
    if let Some(audio) = &dirs.audio {
        cmd.env("AUDIO_DOWNLOAD_DIR", audio);
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("running {}: {e}", script.display()));
    assert!(
        out.status.success(),
        "the entrypoint exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    // Every root must exist afterwards, whatever the CHOWN_DIRS mode.
    for d in dirs.all() {
        assert!(d.is_dir(), "{} was not created", d.display());
    }
    std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The four roots the entrypoint owns, as absolute paths under one scratch directory.
#[cfg(unix)]
struct EntrypointDirs<'a> {
    root: &'a Path,
    download: PathBuf,
    state: PathBuf,
    temp: PathBuf,
    audio: Option<PathBuf>,
}

#[cfg(unix)]
impl<'a> EntrypointDirs<'a> {
    /// The shipped-compose shape: a split audio root nested in the download root.
    fn split(root: &'a Path) -> Self {
        Self {
            root,
            download: root.join("downloads"),
            state: root.join("downloads/.metube"),
            temp: root.join("downloads/.tmp"),
            audio: Some(root.join("downloads/audio")),
        }
    }

    fn all(&self) -> Vec<PathBuf> {
        let mut v = vec![self.download.clone(), self.state.clone(), self.temp.clone()];
        v.extend(self.audio.clone());
        v
    }
}

#[cfg(unix)]
#[test]
fn the_entrypoint_chowns_every_root_it_creates_even_when_chown_dirs_is_false() {
    // Regression test for ops-1: docker/compose.example.yml ships `CHOWN_DIRS=false` **and** a
    // split `AUDIO_DOWNLOAD_DIR`. The entrypoint created /downloads/audio as root:root and never
    // handed it over, so the server booted green and every audio download failed with EACCES
    // while video downloads (into the chowned /downloads) worked.
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dirs = EntrypointDirs::split(tmp.path());
    let chowns = run_entrypoint(&dirs, "false");
    for d in dirs.all() {
        let want = d.display().to_string();
        assert!(
            chowns.iter().any(|line| line.contains(&want)),
            "{want} was created by the entrypoint but never chowned to PUID:PGID; chowns: {chowns:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn the_entrypoint_chowns_the_audio_root_in_both_chown_arms() {
    // The other half of ops-1: on a volume where the roots already exist, the audio root must
    // still be in the `true` (default) and `recursive` chown lists.
    for mode in ["true", "recursive"] {
        let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
        let dirs = EntrypointDirs::split(tmp.path());
        for d in dirs.all() {
            std::fs::create_dir_all(&d).unwrap_or_else(|e| panic!("pre-create: {e}"));
        }
        let chowns = run_entrypoint(&dirs, mode);
        let audio = dirs.audio.clone().unwrap_or_default().display().to_string();
        assert!(
            chowns.iter().any(|line| line.contains(&audio)),
            "CHOWN_DIRS={mode} must chown the audio root {audio}; chowns: {chowns:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn the_entrypoint_is_a_no_op_on_ownership_when_docker_set_the_user() {
    // The non-root branch must not attempt any chown (it would fail), but must still create the
    // roots -- including the audio one.
    let tmp = tempfile::tempdir().unwrap_or_else(|e| panic!("tempdir: {e}"));
    let dirs = EntrypointDirs::split(tmp.path());
    // `id` is not stubbed here: run the script directly so it sees the real (non-root) uid.
    let script = repo_root().join("docker/entrypoint.sh");
    let out = std::process::Command::new("sh")
        .arg(&script)
        .arg("--version")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("UMASK", "022")
        .env("PUID", "1000")
        .env("PGID", "1000")
        .env("DOWNLOAD_DIR", &dirs.download)
        .env("STATE_DIR", &dirs.state)
        .env("TEMP_DIR", &dirs.temp)
        .env("AUDIO_DOWNLOAD_DIR", dirs.audio.clone().unwrap_or_default())
        .output()
        .unwrap_or_else(|e| panic!("running {}: {e}", script.display()));
    // The final `exec /usr/local/bin/aulos-server` cannot succeed in the test sandbox; what
    // matters is that everything before it ran.
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("User set by docker"),
        "expected the non-root branch, got: {stdout}"
    );
    for d in dirs.all() {
        assert!(d.is_dir(), "{} was not created", d.display());
    }
}

#[test]
fn the_compose_example_outlives_the_shutdown_sequence() {
    // Regression test for ops-3: Docker's default stop timeout is 10 s, so without an explicit
    // `stop_grace_period` a plain `docker compose down` SIGKILLs the container in the middle of
    // the DESIGN §21.4 grace poll -- before in-flight jobs are cancelled, before the interrupted
    // rows get `msg="Interrupted by shutdown"`, and before wal_checkpoint(TRUNCATE).
    let compose = read("docker/compose.example.yml");
    let secs = compose
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("stop_grace_period:"))
        .map(|v| v.trim().trim_end_matches('s').to_owned())
        .unwrap_or_else(|| panic!("docker/compose.example.yml must declare a stop_grace_period"))
        .parse::<u64>()
        .unwrap_or_else(|e| panic!("stop_grace_period must be a whole number of seconds: {e}"));

    let default = |key: &str| -> u64 {
        aulos_core::config::DEFAULTS
            .iter()
            .find(|(k, _)| *k == key)
            .and_then(|(_, v)| v.parse::<u64>().ok())
            .unwrap_or_else(|| panic!("{key} must have a numeric default"))
    };
    // aulos-server's `WS_CLOSE_GRACE` (2) + `TRACKER_CEILING` (10); that crate is not a
    // dependency of this one, so the two ceilings are spelled out here and in DESIGN §19.
    const POST_KILL_CEILINGS: u64 = 12;
    let needed = default("AULOS_SHUTDOWN_GRACE_SECS")
        + default("AULOS_KILL_GRACE_MS") / 1000
        + POST_KILL_CEILINGS;
    assert!(
        secs > needed,
        "stop_grace_period is {secs}s but the shutdown sequence can take {needed}s"
    );

    // The README quickstart is the snippet operators actually copy; it must carry it too.
    assert!(
        read("README.md").contains("stop_grace_period:"),
        "the README quickstart must set stop_grace_period as well"
    );
}
