//! Replays the WP-00 golden corpus for `dl_formats.get_opts` against
//! [`aulos_provider_ytdlp::get_opts_raw`], as canonical JSON.
//!
//! The corpus is `tests/golden/opts.json` at the **workspace** root, dumped from the legacy
//! `app/dl_formats.py` by `tools/capture/dump_formats.py`. Two sections:
//!
//! - `sweep` — every legal request tuple with an empty caller dict (235 entries; the caption rows
//!   carry `|subtitle_mode|subtitle_language` suffixes).
//! - `branches` — one entry per branch that needs non-default inputs: an existing
//!   `writethumbnail`, an existing `postprocessors` list, a caller `format` that `best_remux` must
//!   pop, a nested dict that proves the deep copy, the caption normalisers, and `None` defaults.
//!
//! Comparison is on `serde_json::Value`, and because `serde_json::Map` is a `BTreeMap` in this
//! workspace (no `preserve_order` feature) both sides are key-sorted by construction — the
//! "canonical JSON" the PLAN asks for, with no normalisation step to get wrong.
//!
//! # The one expected difference
//!
//! Legacy appended `{"key": "Exec", "exec_cmd": "python3 /app/app/audio_sync_fix.py
//! %(filepath)q"}` to `postprocessors` for `{video, mp4, best_remux}`. DESIGN §9.8 (Δ C9)
//! replaces it with the in-process `audio_sync` hook of §13.3, so this port does not emit it. The
//! delta is *proven*, not assumed: [`strip_legacy_exec`] asserts the trailing entry is exactly
//! [`aulos_provider_ytdlp::opts::legacy_audio_sync_exec`] before removing it, and
//! [`the_only_difference_from_legacy_is_the_exec_step`] asserts the corpus still contains that
//! entry — so if a future capture drops it, or changes it, this file fails instead of quietly
//! agreeing.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: a panic IS the failure

use std::collections::BTreeMap;
use std::path::PathBuf;

use aulos_provider_ytdlp::opts::legacy_audio_sync_exec;
use aulos_provider_ytdlp::{get_opts_raw, normalize_caption_mode};
use serde::Deserialize;
use serde_json::{Map, Value};

/// The golden file's top level.
#[derive(Deserialize)]
struct Golden {
    /// `download_type|codec|format|quality[|subtitle_mode|subtitle_language]` → option dict.
    sweep: BTreeMap<String, Map<String, Value>>,
    branches: Vec<Branch>,
    caption_modes: Vec<String>,
    key_format: String,
    provenance: Provenance,
}

#[derive(Deserialize)]
struct Branch {
    name: String,
    args: Args,
    ytdl_opts: Map<String, Value>,
    subtitle_language: String,
    subtitle_mode: String,
    opts: Map<String, Value>,
    caller_opts_unmutated: bool,
    why: String,
}

#[derive(Deserialize)]
struct Args {
    download_type: Option<String>,
    format: Option<String>,
    quality: Option<String>,
}

#[derive(Deserialize)]
struct Provenance {
    legacy_module: String,
    tool: String,
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/golden/opts.json")
}

fn load() -> Golden {
    let path = golden_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("cannot parse {}: {e}", path.display()))
}

/// Removes the legacy `Exec` audio-sync step from an expected dict, asserting it was there and
/// was exactly what we think it is.
///
/// `what` names the corpus entry so a failure points at a row rather than at this helper.
fn strip_legacy_exec(expected: &mut Map<String, Value>, what: &str) -> bool {
    let Some(Value::Array(list)) = expected.get_mut("postprocessors") else {
        panic!("{what}: golden entry has no `postprocessors` array");
    };
    match list.last() {
        Some(last) if last == &legacy_audio_sync_exec() => {
            list.pop();
            true
        }
        _ => false,
    }
}

/// Splits a sweep key into the arguments `get_opts` took.
///
/// Four fields for every type but `captions`, which appends `|subtitle_mode|subtitle_language`.
fn split_key(key: &str) -> (Args, String, String) {
    let parts: Vec<&str> = key.split('|').collect();
    assert!(
        parts.len() == 4 || parts.len() == 6,
        "malformed golden key {key:?}"
    );
    let args = Args {
        download_type: Some(parts[0].to_owned()),
        format: Some(parts[2].to_owned()),
        quality: Some(parts[3].to_owned()),
    };
    let (mode, language) = if parts.len() == 6 {
        (parts[4].to_owned(), parts[5].to_owned())
    } else {
        ("prefer_manual".to_owned(), "en".to_owned())
    };
    (args, mode, language)
}

fn call(args: &Args, user: Map<String, Value>, language: &str, mode: &str) -> Map<String, Value> {
    get_opts_raw(
        args.download_type.as_deref(),
        args.format.as_deref(),
        args.quality.as_deref().unwrap_or(""),
        user,
        language,
        mode,
    )
}

#[test]
fn the_corpus_is_the_one_we_think_it_is() {
    let g = load();
    assert_eq!(
        g.key_format,
        "download_type|codec|format|quality, with |subtitle_mode|subtitle_language appended for \
         download_type=captions"
    );
    assert_eq!(g.provenance.legacy_module, "app/dl_formats.py");
    assert_eq!(g.provenance.tool, "tools/capture/dump_formats.py");
    assert!(!g.sweep.is_empty() && !g.branches.is_empty());
}

#[test]
fn the_caption_modes_are_the_legacy_tuple() {
    let g = load();
    for mode in &g.caption_modes {
        assert_eq!(
            normalize_caption_mode(mode),
            mode,
            "{mode} must be a recognised CAPTION_MODES value"
        );
    }
    assert_eq!(
        g.caption_modes,
        ["auto_only", "manual_only", "prefer_manual", "prefer_auto"]
    );
}

#[test]
fn every_golden_sweep_entry_reproduces_as_canonical_json() {
    let g = load();
    let mut wrong = Vec::new();
    for (key, expected) in &g.sweep {
        let (args, mode, language) = split_key(key);
        let mut expected = expected.clone();
        strip_legacy_exec(&mut expected, key);
        let got = call(&args, Map::new(), &language, &mode);
        if got != expected {
            wrong.push(format!(
                "{key}\n  want {}\n  got  {}",
                Value::Object(expected),
                Value::Object(got)
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} golden option dicts differ:\n{}",
        wrong.len(),
        g.sweep.len(),
        wrong.join("\n")
    );
}

#[test]
fn every_golden_branch_reproduces_and_leaves_the_caller_dict_alone() {
    let g = load();
    for branch in &g.branches {
        let mut expected = branch.opts.clone();
        strip_legacy_exec(&mut expected, &branch.name);

        // The caller's dict is handed over as a clone, which is what legacy's
        // `copy.deepcopy(ytdl_opts)` bought. The original must be untouched afterwards.
        let before = branch.ytdl_opts.clone();
        let got = call(
            &branch.args,
            branch.ytdl_opts.clone(),
            &branch.subtitle_language,
            &branch.subtitle_mode,
        );
        assert_eq!(
            Value::Object(got),
            Value::Object(expected),
            "{}: {}",
            branch.name,
            branch.why
        );
        if branch.caller_opts_unmutated {
            assert_eq!(
                branch.ytdl_opts, before,
                "{}: caller dict mutated",
                branch.name
            );
        }
    }
}

#[test]
fn the_only_difference_from_legacy_is_the_exec_step() {
    let g = load();
    // The corpus must still record the legacy `Exec` step for `best_remux`; if a re-capture ever
    // drops it, this file's `strip_legacy_exec` calls would silently become no-ops and the
    // documented delta would stop being tested.
    let mut remux = 0_usize;
    for (key, expected) in &g.sweep {
        let mut expected = expected.clone();
        if strip_legacy_exec(&mut expected, key) {
            remux += 1;
            assert!(
                key.starts_with("video|") && key.ends_with("|mp4|best_remux"),
                "only {{video, mp4, best_remux}} carried the Exec step, not {key}"
            );
        }
    }
    assert_eq!(remux, 5, "one per codec: auto, av1, h264, h265, vp9");

    // And nothing else in the corpus mentions the legacy script path.
    let text = std::fs::read_to_string(golden_path()).unwrap();
    assert_eq!(
        text.matches("audio_sync_fix.py").count(),
        5 + 2, // the five sweep rows plus the two `branches` entries that exercise ordering
        "the corpus's Exec occurrences moved; re-read the delta before trusting this file"
    );
}
