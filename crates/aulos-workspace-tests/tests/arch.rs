//! The DESIGN §3 dependency-direction gate (WP-03).
//!
//! This is the test that makes the crate table in DESIGN §3 an executable statement rather than a
//! paragraph everyone reads once. It parses every member `Cargo.toml` and enforces the five named
//! rules **plus** the subset rule: a crate may declare only the dependencies its row budgets for.
//!
//! Two things make it trustworthy rather than decorative:
//!
//! - **The rules are functions over a parsed model, not greps.** Each of the six checks takes a
//!   `&[CrateInfo]` and returns the violations it found, so every rule has a *negative* test that
//!   feeds it a deliberately broken tree (a `rusqlite` dep on `aulos-api`, an `axum` dep on
//!   `aulos-queue`, …) and asserts it fires. A gate with no failing case is a gate nobody has
//!   proven runs.
//! - **The exemption list is checked against the prose.** The "ubiquitous five" live in one
//!   `const` here, and [`the_ubiquitous_five_match_the_design_paragraph`] asserts DESIGN §3 still
//!   names exactly those five — so the document and the gate cannot drift apart silently.
#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: a panic IS the failure

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// The DESIGN §3 table, verbatim.
// ---------------------------------------------------------------------------

/// The dependencies DESIGN §3 permits in **every** crate, omits from every row, and instructs this
/// gate to ignore. They carry no architectural information: forbidding `serde_json` in a crate that
/// must name a `serde_json::Value` in a public signature would only push the same type through a
/// re-export.
const UBIQUITOUS_FIVE: [&str; 5] = ["serde", "serde_json", "thiserror", "tracing", "async-trait"];

/// One row of the DESIGN §3 table: the crate, and what it may depend on beyond the ubiquitous
/// five. `None` means "everything" — the row `aulos-server` has.
const TABLE: [(&str, Option<&[&str]>); 12] = [
    (
        "aulos-core",
        Some(&["ulid", "url", "time", "arc-swap", "regex", "tokio"]),
    ),
    (
        "aulos-store",
        Some(&[
            "aulos-core",
            "rusqlite",
            "rusqlite_migration",
            "ulid",
            "base64",
            "time",
            "tokio",
        ]),
    ),
    (
        "aulos-provider",
        Some(&[
            "aulos-core",
            "tokio",
            "tokio-util",
            "toml",
            "regex",
            "nix",
            "url",
        ]),
    ),
    (
        "aulos-provider-ytdlp",
        Some(&[
            "aulos-core",
            "aulos-provider",
            "tokio",
            "tokio-util",
            "nix",
            "command-fds",
            "url",
        ]),
    ),
    (
        "aulos-provider-sc",
        Some(&[
            "aulos-core",
            "aulos-provider",
            "tokio",
            "tokio-util",
            "wreq",
            // Amendment (wave-1 integration, at WP-08's request): `wreq` ships only
            // `Emulation`/`EmulationBuilder`; the named Chrome profiles — the real JA3/JA4 tables
            // and Chrome's HTTP/2 SETTINGS order — live in `wreq-util`. DESIGN §18.6 and the §3
            // row both omit it. See docs/INTEGRATION-NOTES.md, WP-08.
            "wreq-util",
            "reqwest",
            "scraper",
            "regex",
            "strip-ansi-escapes",
            "url",
            // Amendment (WP-01): the `ScHttp` trait's futures need `futures-util`, which §18.6
            // budgets for but the §3 row omits. See docs/INTEGRATION-NOTES.md, WP-03.
            "futures-util",
        ]),
    ),
    (
        "aulos-queue",
        Some(&[
            "aulos-core",
            "aulos-store",
            "aulos-provider",
            "tokio",
            "tokio-util",
            "arc-swap",
            "bytes",
            "smallvec",
            "indexmap",
            "rand",
            "url",
        ]),
    ),
    (
        "aulos-api",
        Some(&[
            "aulos-core",
            "aulos-store",
            "aulos-queue",
            // Amendment (WP-14): DESIGN §3's row omits `aulos-provider`, while PLAN WP-14's
            // `ApiState` types `registry: Arc<RwLock<Registry>>` and `GET api/v2/catalog`,
            // `providers` and `resolve-preview` are all projections of it. No §3 *rule* forbids
            // the edge (A1 is about provider crates depending on the store or the queue), so the
            // row gains the crate. See docs/INTEGRATION-NOTES.md, WP-14.
            "aulos-provider",
            "axum",
            "axum-server",
            "tower",
            "tower-http",
            "tokio",
            "tokio-util",
            "arc-swap",
            "bytes",
            "mime_guess",
            "percent-encoding",
            "sha2",
            "url",
            "rustls",
            "rustls-pemfile",
            // v1.0: the Prometheus endpoint is CUT (BRIEF), so `metrics` and
            // `metrics-exporter-prometheus` are budgeted but not declared. A subset check permits
            // fewer, so both stay listed for the day §16.7 comes back.
            "metrics",
            "metrics-exporter-prometheus",
            // Amendment (WP-01): `serde_with` serialises the v1 shim's legacy field shapes.
            // §18.6 budgets for it; the §3 row omits it. See docs/INTEGRATION-NOTES.md, WP-03.
            "serde_with",
        ]),
    ),
    (
        "aulos-telegram",
        Some(&[
            "aulos-core",
            "aulos-store",
            "aulos-queue",
            "teloxide",
            "governor",
            "indexmap",
            "rand",
            "tokio",
            "tokio-util",
            "url",
        ]),
    ),
    (
        "aulos-subscriptions",
        Some(&[
            "aulos-core",
            "aulos-store",
            "aulos-provider",
            "aulos-queue",
            "tokio",
            "tokio-util",
            "rand",
            "url",
        ]),
    ),
    (
        "aulos-hooks",
        Some(&[
            "aulos-core",
            "aulos-provider",
            "reqwest",
            "quick-xml",
            "time",
            "tokio",
            "tokio-util",
            "url",
        ]),
    ),
    // "everything" — the binary wires the whole graph together.
    ("aulos-server", None),
    // Dev-only, no `src/`, no normal dependencies at all.
    ("aulos-workspace-tests", Some(&[])),
];

/// The crates rule A1 protects: no provider crate may see the store or the queue.
const PROVIDER_CRATES: [&str; 3] = [
    "aulos-provider",
    "aulos-provider-ytdlp",
    "aulos-provider-sc",
];

// ---------------------------------------------------------------------------
// The parsed model.
// ---------------------------------------------------------------------------

/// One member crate's declared dependencies, by section.
#[derive(Clone, Debug, Default)]
struct CrateInfo {
    name: String,
    /// `[dependencies]`, plus any `[target.'cfg(…)'.dependencies]`.
    deps: BTreeSet<String>,
    /// `[dev-dependencies]`, plus the target-specific form.
    dev_deps: BTreeSet<String>,
    /// `[build-dependencies]`, plus the target-specific form.
    build_deps: BTreeSet<String>,
}

impl CrateInfo {
    /// Normal plus build dependencies: the crate's real, shipped surface.
    fn shipped(&self) -> BTreeSet<String> {
        self.deps.union(&self.build_deps).cloned().collect()
    }

    /// Everything, including test-only dependencies.
    fn all(&self) -> BTreeSet<String> {
        self.shipped().union(&self.dev_deps).cloned().collect()
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

/// Collects the dependency names from one manifest section, resolving `package = "…"` renames to
/// the real crate name — otherwise a rename would be a hole straight through the gate.
fn names_from(table: Option<&toml::Value>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(toml::Value::Table(t)) = table else {
        return out;
    };
    for (key, value) in t {
        let real = value
            .get("package")
            .and_then(toml::Value::as_str)
            .unwrap_or(key.as_str());
        out.insert(real.to_owned());
    }
    out
}

/// Parses every `crates/*/Cargo.toml` into the model the rules run over.
fn parse_workspace() -> Vec<CrateInfo> {
    let crates_dir = repo_root().join("crates");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&crates_dir)
        .unwrap_or_else(|e| panic!("{} must be readable: {e}", crates_dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("Cargo.toml").is_file())
        .collect();
    entries.sort();

    entries
        .iter()
        .map(|dir| {
            let manifest = dir.join("Cargo.toml");
            let src = std::fs::read_to_string(&manifest)
                .unwrap_or_else(|e| panic!("{} must be readable: {e}", manifest.display()));
            let doc: toml::Value = toml::from_str(&src)
                .unwrap_or_else(|e| panic!("{} must parse: {e}", manifest.display()));
            let name = doc
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(toml::Value::as_str)
                .unwrap_or_else(|| panic!("{} must name its package", manifest.display()))
                .to_owned();

            let mut info = CrateInfo {
                name,
                deps: names_from(doc.get("dependencies")),
                dev_deps: names_from(doc.get("dev-dependencies")),
                build_deps: names_from(doc.get("build-dependencies")),
            };
            // Target-specific sections count exactly the same: `[target.'cfg(unix)'.dependencies]`
            // is still a dependency.
            if let Some(toml::Value::Table(targets)) = doc.get("target") {
                for cfg in targets.values() {
                    info.deps.extend(names_from(cfg.get("dependencies")));
                    info.dev_deps
                        .extend(names_from(cfg.get("dev-dependencies")));
                    info.build_deps
                        .extend(names_from(cfg.get("build-dependencies")));
                }
            }
            // A dev-dependency on itself is the standard way to turn a feature on for the test
            // targets (`aulos-provider` does it for `fake`). It is not an architectural edge.
            info.dev_deps.remove(&info.name);
            info
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The six rules. Each returns a human-readable violation per offence.
// ---------------------------------------------------------------------------

/// A1 — no `aulos-provider*` crate may depend on `aulos-store` or `aulos-queue`.
///
/// This is the property that keeps providers replaceable and separately testable, so it covers
/// dev-dependencies too: a provider whose *tests* need the store is a provider that is not
/// separately testable.
fn rule_a1(crates: &[CrateInfo]) -> Vec<String> {
    let mut out = Vec::new();
    for c in crates
        .iter()
        .filter(|c| PROVIDER_CRATES.contains(&&*c.name))
    {
        for forbidden in ["aulos-store", "aulos-queue"] {
            if c.all().contains(forbidden) {
                out.push(format!("A1: {} must not depend on {forbidden}", c.name));
            }
        }
    }
    out
}

/// A2 — only `aulos-store` may depend on `rusqlite`. One storage engine, one place to change it.
fn rule_a2(crates: &[CrateInfo]) -> Vec<String> {
    crates
        .iter()
        .filter(|c| c.name != "aulos-store" && c.all().contains("rusqlite"))
        .map(|c| format!("A2: {} must not depend on rusqlite", c.name))
        .collect()
}

/// A3 — only `aulos-api` and `aulos-server` may depend on `axum`. Keeps the engine, the store and
/// the providers usable from a test harness and from the CLI.
fn rule_a3(crates: &[CrateInfo]) -> Vec<String> {
    crates
        .iter()
        .filter(|c| !matches!(&*c.name, "aulos-api" | "aulos-server") && c.all().contains("axum"))
        .map(|c| format!("A3: {} must not depend on axum", c.name))
        .collect()
}

/// A4 — nothing but `aulos-server` may depend on `aulos-api`. The API is a leaf.
fn rule_a4(crates: &[CrateInfo]) -> Vec<String> {
    crates
        .iter()
        .filter(|c| c.name != "aulos-server" && c.all().contains("aulos-api"))
        .map(|c| format!("A4: {} must not depend on aulos-api", c.name))
        .collect()
}

/// A5 — only `aulos-server` may depend on `anyhow` (BRIEF §18): a library that erases its error
/// type cannot drive the §8.8 retry policy.
///
/// Judged on shipped dependencies only. A test helper that wants `anyhow` erases nothing a caller
/// could have branched on, and every library error type is still asserted to be a `thiserror` enum
/// by its own crate's tests.
fn rule_a5(crates: &[CrateInfo]) -> Vec<String> {
    crates
        .iter()
        .filter(|c| c.name != "aulos-server" && c.shipped().contains("anyhow"))
        .map(|c| format!("A5: {} must not depend on anyhow", c.name))
        .collect()
}

/// The subset rule — a crate's declared dependencies must be a subset of its DESIGN §3 row.
///
/// This is what catches a crate quietly acquiring a dependency the design did not budget for, and
/// it is deliberately checked on **shipped** dependencies only: the rows describe the architecture,
/// while `insta`, `wiremock`, `rstest`, `proptest` and friends are a test toolbox the table does
/// not enumerate (and the `aulos-workspace-tests` row's "(dev)" annotation says as much).
fn rule_subset(crates: &[CrateInfo]) -> Vec<String> {
    let table: BTreeMap<&str, Option<&[&str]>> = TABLE.iter().copied().collect();
    let mut out = Vec::new();
    for c in crates {
        let Some(row) = table.get(&*c.name) else {
            out.push(format!(
                "subset: {} is not in the DESIGN §3 table — add a row for it",
                c.name
            ));
            continue;
        };
        let Some(allowed) = row else {
            continue; // aulos-server: "everything".
        };
        for dep in c.shipped() {
            if UBIQUITOUS_FIVE.contains(&&*dep) || allowed.contains(&&*dep) {
                continue;
            }
            out.push(format!(
                "subset: {} declares {dep}, which its DESIGN §3 row does not budget for",
                c.name
            ));
        }
    }
    out
}

/// Every rule, so the positive test and the "no rule is dead" test agree on the list.
#[allow(clippy::type_complexity)] // a table of named checks is exactly this shape
const RULES: [(&str, fn(&[CrateInfo]) -> Vec<String>); 6] = [
    ("A1", rule_a1),
    ("A2", rule_a2),
    ("A3", rule_a3),
    ("A4", rule_a4),
    ("A5", rule_a5),
    ("subset", rule_subset),
];

// ---------------------------------------------------------------------------
// The gate.
// ---------------------------------------------------------------------------

#[test]
fn the_real_workspace_satisfies_every_dependency_rule() {
    let crates = parse_workspace();
    assert_eq!(
        crates.len(),
        TABLE.len(),
        "the workspace has {} members but DESIGN §3 lists {}: {:?}",
        crates.len(),
        TABLE.len(),
        crates.iter().map(|c| &c.name).collect::<Vec<_>>()
    );

    let mut violations = Vec::new();
    for (_, rule) in RULES {
        violations.extend(rule(&crates));
    }
    assert!(
        violations.is_empty(),
        "DESIGN §3 violations:\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn every_crate_in_the_design_table_exists_and_vice_versa() {
    let crates = parse_workspace();
    let actual: BTreeSet<&str> = crates.iter().map(|c| &*c.name).collect();
    let expected: BTreeSet<&str> = TABLE.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        actual, expected,
        "the workspace members and the DESIGN §3 table must be the same set"
    );
}

#[test]
fn the_provider_crates_row_is_proven_not_assumed() {
    // PLAN WP-03 asks specifically that the provider rows be asserted, because they are the ones
    // rule A1 protects and the ones every later wave adds code to.
    let crates = parse_workspace();
    let by_name: BTreeMap<&str, &CrateInfo> = crates.iter().map(|c| (&*c.name, c)).collect();

    let provider = by_name["aulos-provider"];
    for required in [
        "aulos-core",
        "tokio",
        "tokio-util",
        "toml",
        "regex",
        "nix",
        "url",
    ] {
        assert!(
            provider.deps.contains(required),
            "aulos-provider must declare {required} (DESIGN §3)"
        );
    }
    let ytdlp = by_name["aulos-provider-ytdlp"];
    for required in [
        "aulos-core",
        "aulos-provider",
        "tokio",
        "nix",
        "command-fds",
    ] {
        assert!(
            ytdlp.deps.contains(required),
            "aulos-provider-ytdlp must declare {required} (DESIGN §3)"
        );
    }
    for name in PROVIDER_CRATES {
        let c = by_name[name];
        assert!(
            !c.all().contains("aulos-store") && !c.all().contains("aulos-queue"),
            "{name} must not see the store or the queue (A1)"
        );
    }
}

#[test]
fn the_ubiquitous_five_match_the_design_paragraph() {
    // The exemption list is load-bearing: without it the gate would reject WP-06 through WP-09 on
    // their first commit. So it is asserted against the prose that authorises it, rather than
    // trusted to stay in sync.
    let design = std::fs::read_to_string(repo_root().join("docs/DESIGN.md"))
        .expect("docs/DESIGN.md must be readable");
    let paragraph = design
        .split("\n\n")
        .find(|p| p.contains("**The ubiquitous five.**"))
        .expect("DESIGN §3 must still contain the ubiquitous-five paragraph");

    for name in UBIQUITOUS_FIVE {
        assert!(
            paragraph.contains(&format!("`{name}`")),
            "DESIGN §3 must name `{name}` as one of the ubiquitous five"
        );
    }
    // …and exactly five, so a sixth added to the prose does not silently widen the gate.
    let quoted: BTreeSet<&str> = paragraph
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|s| UBIQUITOUS_FIVE.contains(s) || *s == "anyhow" || *s == "aulos-server")
        .collect();
    assert_eq!(
        quoted.len(),
        UBIQUITOUS_FIVE.len() + 2,
        "the paragraph must name the five plus the `anyhow`/`aulos-server` inverse: {quoted:?}"
    );
    assert!(
        paragraph.contains("A5"),
        "the paragraph must still tie `anyhow` to rule A5"
    );
}

// ---------------------------------------------------------------------------
// Negative tests: one deliberately broken tree per rule, so no rule can be silently dead.
// ---------------------------------------------------------------------------

/// The real tree with one extra dependency bolted on.
fn tree_with(crate_name: &str, dep: &str) -> Vec<CrateInfo> {
    let mut crates = parse_workspace();
    let target = crates
        .iter_mut()
        .find(|c| c.name == crate_name)
        .unwrap_or_else(|| panic!("{crate_name} must exist"));
    target.deps.insert(dep.to_owned());
    crates
}

#[test]
fn a1_fires_on_a_store_dependency_in_a_provider_crate() {
    let broken = tree_with("aulos-provider-sc", "aulos-store");
    let found = rule_a1(&broken);
    assert_eq!(
        found,
        ["A1: aulos-provider-sc must not depend on aulos-store"]
    );
    assert!(rule_a1(&parse_workspace()).is_empty());

    // Dev-dependencies count as well.
    let mut broken = parse_workspace();
    broken
        .iter_mut()
        .find(|c| c.name == "aulos-provider")
        .unwrap()
        .dev_deps
        .insert("aulos-queue".to_owned());
    assert_eq!(
        rule_a1(&broken),
        ["A1: aulos-provider must not depend on aulos-queue"]
    );
}

#[test]
fn a2_fires_on_a_rusqlite_dependency_in_aulos_api() {
    let broken = tree_with("aulos-api", "rusqlite");
    assert_eq!(
        rule_a2(&broken),
        ["A2: aulos-api must not depend on rusqlite"]
    );
    assert!(rule_a2(&parse_workspace()).is_empty());
}

#[test]
fn a3_fires_on_an_axum_dependency_in_aulos_queue() {
    let broken = tree_with("aulos-queue", "axum");
    assert_eq!(
        rule_a3(&broken),
        ["A3: aulos-queue must not depend on axum"]
    );
    assert!(rule_a3(&parse_workspace()).is_empty());
}

#[test]
fn a4_fires_on_an_api_dependency_in_aulos_telegram() {
    let broken = tree_with("aulos-telegram", "aulos-api");
    assert_eq!(
        rule_a4(&broken),
        ["A4: aulos-telegram must not depend on aulos-api"]
    );
    assert!(rule_a4(&parse_workspace()).is_empty());
}

#[test]
fn a5_fires_on_an_anyhow_dependency_in_aulos_core() {
    let broken = tree_with("aulos-core", "anyhow");
    assert_eq!(
        rule_a5(&broken),
        ["A5: aulos-core must not depend on anyhow"]
    );
    assert!(rule_a5(&parse_workspace()).is_empty());
}

#[test]
fn the_subset_rule_fires_on_an_unbudgeted_dependency_and_on_an_unlisted_crate() {
    let broken = tree_with("aulos-core", "hyper");
    assert_eq!(
        rule_subset(&broken),
        ["subset: aulos-core declares hyper, which its DESIGN §3 row does not budget for"]
    );

    let mut broken = parse_workspace();
    broken.push(CrateInfo {
        name: "aulos-surprise".to_owned(),
        ..CrateInfo::default()
    });
    assert_eq!(
        rule_subset(&broken),
        ["subset: aulos-surprise is not in the DESIGN §3 table — add a row for it"]
    );

    // The ubiquitous five are exempt everywhere, which is the whole point of the exemption.
    for five in UBIQUITOUS_FIVE {
        let tolerated = tree_with("aulos-core", five);
        assert!(
            rule_subset(&tolerated).is_empty(),
            "{five} must be tolerated in every crate"
        );
    }
    // A crate declaring *fewer* dependencies than its row is fine: it is a subset rule.
    let mut lean = parse_workspace();
    lean.iter_mut()
        .find(|c| c.name == "aulos-hooks")
        .unwrap()
        .deps
        .clear();
    assert!(rule_subset(&lean).is_empty());
}

#[test]
fn a_renamed_dependency_cannot_slip_past_the_gate() {
    // `sneaky = { package = "rusqlite" }` must be read as `rusqlite`, or renaming would be a hole
    // straight through every rule.
    let manifest = r#"
[package]
name = "aulos-api"

[dependencies]
sneaky = { package = "rusqlite", version = "0.37" }
"#;
    let doc: toml::Value = toml::from_str(manifest).unwrap();
    let names = names_from(doc.get("dependencies"));
    assert!(names.contains("rusqlite"), "{names:?}");
    assert!(!names.contains("sneaky"), "{names:?}");
}

#[test]
fn target_specific_and_build_dependencies_are_counted() {
    // A `[target.'cfg(unix)'.dependencies]` entry is still a dependency; so is a build script's.
    let manifest = r#"
[package]
name = "aulos-queue"

[build-dependencies]
axum = "0.8"

[target.'cfg(unix)'.dependencies]
rusqlite = "0.37"
"#;
    let doc: toml::Value = toml::from_str(manifest).unwrap();
    let mut info = CrateInfo {
        name: "aulos-queue".to_owned(),
        deps: names_from(doc.get("dependencies")),
        dev_deps: names_from(doc.get("dev-dependencies")),
        build_deps: names_from(doc.get("build-dependencies")),
    };
    if let Some(toml::Value::Table(targets)) = doc.get("target") {
        for cfg in targets.values() {
            info.deps.extend(names_from(cfg.get("dependencies")));
        }
    }
    let broken = vec![info];
    assert_eq!(
        rule_a2(&broken),
        ["A2: aulos-queue must not depend on rusqlite"]
    );
    assert_eq!(
        rule_a3(&broken),
        ["A3: aulos-queue must not depend on axum"]
    );
}
