//! Replays the WP-00 golden corpus for `_calculate_progress_percent` against
//! [`aulos_core::Normalizer`], plus a `proptest` for the invariants the corpus cannot enumerate.
//!
//! The corpus is `tests/golden/percent.json` at the **workspace** root, captured from the legacy
//! `app/ytdl.py` by `tools/capture/dump_progress_vectors.py`. Every vector and every sequence must
//! reproduce byte-for-byte; the one documented signature difference is that
//! `Normalizer::apply` returns `f64` where legacy returned `float | None`, so a legacy `None`
//! (which meant "keep the previous value", and there was none) is `0.0` here — `ItemView.percent`
//! is contractually never null.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: a panic IS the failure

use std::path::PathBuf;

use aulos_core::progress::{integer, number};
use aulos_core::{Normalizer, RawProgress, Status};
use serde::Deserialize;

/// The golden file's top level.
#[derive(Deserialize)]
struct Golden {
    rules: Vec<String>,
    vectors: Vec<Vector>,
    sequences: Vec<Sequence>,
}

/// One single-frame vector.
#[derive(Deserialize)]
struct Vector {
    name: String,
    status: Frame,
    #[serde(default)]
    previous_percent: Option<f64>,
    #[serde(default)]
    expected: Option<f64>,
    #[serde(default)]
    rules: Vec<String>,
}

/// A yt-dlp progress-hook frame, as the legacy function received it.
///
/// Every numeric field is an untyped JSON value, because that is what legacy's `_number()` was
/// handed and what the corpus exercises: integers, floats, numeric **strings**, `"n/a"` and
/// negatives all appear. The conversion goes through `aulos_core::progress::number` / `integer`,
/// which is exactly what the yt-dlp shim client (WP-07) must do.
#[derive(Deserialize, Default)]
struct Frame {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    downloaded_bytes: serde_json::Value,
    #[serde(default)]
    total_bytes: serde_json::Value,
    #[serde(default)]
    total_bytes_estimate: serde_json::Value,
    #[serde(default)]
    fragment_index: serde_json::Value,
    #[serde(default)]
    fragment_count: serde_json::Value,
    #[serde(default)]
    speed: serde_json::Value,
    #[serde(default)]
    eta: serde_json::Value,
}

impl Frame {
    fn to_raw(&self) -> RawProgress {
        RawProgress {
            downloaded_bytes: number(&self.downloaded_bytes),
            total_bytes: number(&self.total_bytes),
            total_bytes_estimate: number(&self.total_bytes_estimate),
            fragment_index: integer(&self.fragment_index),
            fragment_count: integer(&self.fragment_count),
            speed: number(&self.speed),
            eta: integer(&self.eta),
            phase: None,
            phase_percent: None,
            source_tag: 0,
        }
    }

    /// Only `"finished"` is special-cased by the algorithm, so every other value (including a
    /// missing key) maps to `Downloading`.
    fn status(&self) -> Status {
        match self.status.as_deref() {
            Some("finished") => Status::Finished,
            _ => Status::Downloading,
        }
    }
}

/// One threaded sequence: `previous_percent` flows from each step's result to the next.
#[derive(Deserialize)]
struct Sequence {
    name: String,
    steps: Vec<Step>,
    #[serde(default)]
    final_percent: Option<f64>,
    #[serde(default)]
    rules: Vec<String>,
}

/// One step of a sequence. A step with no `frame` and `reset: true` models a `source_tag` change.
#[derive(Deserialize)]
struct Step {
    #[serde(default)]
    frame: Option<Frame>,
    #[serde(default)]
    percent: Option<f64>,
    #[serde(default)]
    previous_percent: Option<f64>,
    #[serde(default)]
    reset: bool,
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/golden/percent.json")
}

fn load() -> Golden {
    let path = golden_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("cannot parse {}: {e}", path.display()))
}

/// Legacy `None` means "there was no previous value"; the wire contract says `0.0`.
fn expect(value: Option<f64>) -> f64 {
    value.unwrap_or(0.0)
}

/// "Byte-for-byte" means exactly that: the same 64 bits, not an epsilon.
///
/// This is only achievable because `serde_json`'s `float_roundtrip` feature is enabled for this
/// crate; the default parser can land one ULP away from the value CPython produced.
fn bit_equal(a: f64, b: f64) -> bool {
    a.to_bits() == b.to_bits() || (a == 0.0 && b == 0.0)
}

#[test]
fn every_golden_vector_replays_byte_for_byte() {
    let golden = load();
    assert!(!golden.vectors.is_empty(), "the corpus must not be empty");

    for v in &golden.vectors {
        let mut n = Normalizer::with_previous(v.previous_percent);
        let got = n.apply(&v.status.to_raw(), v.status.status());
        let want = expect(v.expected);
        assert!(
            bit_equal(got, want),
            "{}: expected {want:?}, got {got:?} (rules {:?})",
            v.name,
            v.rules
        );
    }
}

#[test]
fn every_golden_sequence_replays_with_the_monotonic_floor_threaded() {
    let golden = load();
    assert!(!golden.sequences.is_empty(), "the corpus must not be empty");

    for s in &golden.sequences {
        let mut n = Normalizer::new();
        let mut last: Option<f64> = None;

        for (i, step) in s.steps.iter().enumerate() {
            match &step.frame {
                None => {
                    assert!(
                        step.reset,
                        "{}: step {i} has no frame and is not a reset",
                        s.name
                    );
                    n.reset();
                    assert_eq!(
                        step.percent, None,
                        "{}: a reset step reports no percent",
                        s.name
                    );
                    last = None;
                }
                Some(frame) => {
                    if step.reset {
                        n.reset();
                    }
                    assert_eq!(
                        n.previous(),
                        step.previous_percent,
                        "{}: step {i} threading diverged",
                        s.name
                    );
                    let got = n.apply(&frame.to_raw(), frame.status());
                    let want = expect(step.percent);
                    assert!(
                        bit_equal(got, want),
                        "{}: step {i} expected {want:?}, got {got:?}",
                        s.name
                    );
                    last = Some(got);
                }
            }
        }

        if let (Some(expected), Some(got)) = (s.final_percent, last) {
            assert!(
                bit_equal(got, expected),
                "{}: final expected {expected:?}, got {got:?} (rules {:?})",
                s.name,
                s.rules
            );
        }
    }
}

#[test]
fn every_rule_the_corpus_declares_is_exercised_by_at_least_one_case() {
    let golden = load();
    for rule in &golden.rules {
        let covered = golden.vectors.iter().any(|v| v.rules.contains(rule))
            || golden.sequences.iter().any(|s| s.rules.contains(rule));
        assert!(covered, "no case exercises rule {rule:?}");
    }
}

#[test]
fn a_source_tag_change_resets_the_floor_without_an_explicit_reset() {
    // The corpus models the reset as an out-of-band step because legacy had no `source_tag`
    // concept; in the Rust port a changed tag on the frame does it (DESIGN §4.7).
    let mut n = Normalizer::new();
    let high = RawProgress {
        downloaded_bytes: Some(95.0),
        total_bytes: Some(100.0),
        source_tag: 1,
        ..RawProgress::default()
    };
    assert!((n.apply(&high, Status::Downloading) - 95.0).abs() < f64::EPSILON);

    let low_new_leg = RawProgress {
        downloaded_bytes: Some(2.0),
        total_bytes: Some(100.0),
        source_tag: 2,
        ..RawProgress::default()
    };
    assert!(
        (n.apply(&low_new_leg, Status::Downloading) - 2.0).abs() < f64::EPSILON,
        "the video->audio leg of a merge starts from zero again"
    );
}

mod properties {
    use super::{Normalizer, RawProgress, Status};
    use proptest::prelude::*;

    prop_compose! {
        fn frame()(
            downloaded in prop::option::of(0.0f64..1_000_000_000.0),
            total in prop::option::of(0.0f64..1_000_000_000.0),
            estimate in prop::option::of(0.0f64..1_000_000_000.0),
            index in prop::option::of(-10i64..5_000),
            count in prop::option::of(0i64..5_000),
        ) -> RawProgress {
            RawProgress {
                downloaded_bytes: downloaded,
                total_bytes: total,
                total_bytes_estimate: estimate,
                fragment_index: index,
                fragment_count: count,
                ..RawProgress::default()
            }
        }
    }

    fn status() -> impl Strategy<Value = Status> {
        prop_oneof![
            Just(Status::Queued),
            Just(Status::Preparing),
            Just(Status::Downloading),
            Just(Status::Postprocessing),
            Just(Status::Finished),
            Just(Status::Error),
            Just(Status::Canceled),
        ]
    }

    proptest! {
        #[test]
        fn percent_is_monotonic_and_bounded_for_one_source(
            frames in prop::collection::vec((frame(), status()), 1..40)
        ) {
            let mut n = Normalizer::new();
            let mut previous = 0.0_f64;
            let mut saw_finished = false;

            for (f, s) in frames {
                let got = n.apply(&f, s);
                prop_assert!(got.is_finite(), "percent must be finite, got {got}");
                prop_assert!((0.0..=100.0).contains(&got), "percent out of range: {got}");
                if s == Status::Finished {
                    prop_assert!((got - 100.0).abs() < f64::EPSILON);
                    saw_finished = true;
                } else {
                    prop_assert!(
                        got <= 99.9 || saw_finished,
                        "an active frame must stay at or below the 99.9 ceiling, got {got}"
                    );
                    prop_assert!(
                        got + 1e-9 >= previous,
                        "percent decreased: {previous} -> {got}"
                    );
                }
                previous = got;
            }
        }

        #[test]
        fn a_source_tag_change_is_the_only_way_percent_may_fall(
            first in frame(),
            second in frame(),
        ) {
            let mut n = Normalizer::new();
            let mut a = first;
            a.source_tag = 1;
            let high = n.apply(&a, Status::Downloading);

            let mut b = second;
            b.source_tag = 1;
            prop_assert!(n.apply(&b, Status::Downloading) + 1e-9 >= high);

            let mut c = second;
            c.source_tag = 2;
            let after_reset = n.apply(&c, Status::Downloading);
            prop_assert!((0.0..=99.9).contains(&after_reset));
        }
    }
}
