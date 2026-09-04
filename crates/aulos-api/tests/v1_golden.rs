//! The golden replay: WP-00's 131-case v1 corpus, against the shim (PLAN WP-15).
//!
//! `harness.rs` holds the machinery and the two allow-lists; this file holds the assertions. The
//! important one is [`every_corpus_case_is_replayed`]: it fails if a directory on disk was never
//! visited, so the corpus can never quietly shrink into a subset that passes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;
mod v1_golden {
    pub mod harness;
}

use std::collections::BTreeSet;

use serde_json::json;
use v1_golden::harness::{
    self, ADDITIVE_KEYS, COOKIE_SEQUENCE, Mode, SEEDED_FEED_URL, SUBSCRIPTION_SEQUENCE,
};

/// The whole corpus, in one process, in the capture's order.
#[tokio::test(flavor = "multi_thread")]
async fn the_v1_corpus_replays_against_the_shim() {
    let cases = harness::load_all();
    let rig = harness::rig().await;
    let mut replayed: BTreeSet<String> = BTreeSet::new();

    for name in harness::ordered(&cases) {
        let case = cases
            .get(&name)
            .unwrap_or_else(|| panic!("{name} is in the replay order but not on disk"));

        // The capture's `STATE_DIR` held a subscription for this feed, which is what makes the
        // three duplicate cases answer `This URL is already subscribed`. Creating one here is the
        // smallest reproduction of that precondition, and it is the *only* state the replay
        // manufactures.
        if name == SUBSCRIPTION_SEQUENCE[0] {
            let (status, _) = rig
                .post(
                    "subscribe",
                    &json!({
                        "url": SEEDED_FEED_URL,
                        "download_type": "video",
                        "format": "any",
                        "quality": "best",
                        "check_interval_minutes": 60
                    }),
                )
                .await;
            assert_eq!(status, 200, "the replay's subscription precondition");
        }

        let reply = harness::replay(&rig, case).await;
        harness::compare(case, &reply);
        replayed.insert(name);
    }

    // Coverage: every directory, no exceptions. This is the assertion that makes the corpus a
    // contract rather than a suggestion.
    let on_disk: BTreeSet<String> = cases.keys().cloned().collect();
    let missed: Vec<&String> = on_disk.difference(&replayed).collect();
    assert!(
        missed.is_empty(),
        "these corpus cases were never replayed: {missed:?}"
    );
    assert_eq!(
        replayed.len(),
        cases.len(),
        "every one of the {} captured cases must be replayed",
        cases.len()
    );
}

/// The manifest's index and the directories on disk must agree, or the corpus is half-recaptured.
#[test]
fn the_manifest_and_the_directories_agree() {
    let manifest = harness::manifest();
    let cases = harness::load_all();
    let indexed: BTreeSet<String> = manifest["cases"]
        .as_object()
        .expect("cases is an object")
        .keys()
        .cloned()
        .collect();
    let on_disk: BTreeSet<String> = cases.keys().cloned().collect();
    assert_eq!(indexed, on_disk, "MANIFEST.json:cases vs the directories");
    assert_eq!(
        manifest["case_count"].as_u64().unwrap_or(0) as usize,
        cases.len()
    );
    // Provenance: after cutover the Python server is gone, so the commit is the only anchor.
    assert!(
        manifest["legacy_commit"]
            .as_str()
            .is_some_and(|c| c.len() >= 7),
        "the manifest must record the legacy commit"
    );
}

/// Every deviation from strict comparison must be one of the three documented families.
#[test]
fn every_shape_case_has_a_reason() {
    let cases = harness::load_all();
    let mut shape = 0;
    for name in cases.keys() {
        if let Mode::Shape(reason) = harness::mode(name) {
            shape += 1;
            assert!(
                reason.len() > 40,
                "{name}: a shape reason must actually explain itself"
            );
        }
    }
    assert!(
        shape < cases.len() / 5,
        "{shape} of {} cases are shape-only; the corpus is meant to be compared strictly",
        cases.len()
    );
    // And nothing outside the corpus is listed.
    for name in [
        "history_seeded",
        "delete_bad_where",
        "start_ids_null",
        "version",
    ] {
        assert!(cases.contains_key(name), "{name} must exist on disk");
        assert!(matches!(harness::mode(name), Mode::Shape(_)));
    }
}

/// The scope trim WP-00 applied: every captured `POST add` is a validation 400, because anything
/// else would have needed yt-dlp to reach the network. If that ever stops being true, the replay's
/// `AULOS_V1_ADD_RESOLVE_WAIT_MS=0` would be hiding a real behaviour.
#[test]
fn every_captured_add_is_a_validation_400() {
    let cases = harness::load_all();
    let mut adds = 0;
    for case in cases.values() {
        if case.request["route"] == "POST add" {
            adds += 1;
            assert_eq!(case.status(), 400, "{}", case.name);
        }
    }
    assert!(adds > 40, "the add matrix should be broad, got {adds}");
}

/// The two sequences the replay reproduces must all be real cases.
#[test]
fn the_sequenced_cases_exist() {
    let cases = harness::load_all();
    for name in COOKIE_SEQUENCE.into_iter().chain(SUBSCRIPTION_SEQUENCE) {
        assert!(cases.contains_key(name), "{name} must exist on disk");
    }
    assert_eq!(ADDITIVE_KEYS.len(), 4);
}
