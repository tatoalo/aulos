//! Byte-for-byte parity with the legacy generator for a **yt-dlp** download (DESIGN §13.2).
//!
//! The built-in NFO hook used to be gated to `provider == "streamingcommunity"`, so on the
//! production cutover a finished YouTube download produced no `.nfo` at all while the legacy image
//! had produced one for every download through an `Exec` postprocessor. The hook now applies to
//! every provider, which only helps if it writes what legacy wrote — so the expectation here is
//! not hand-written. Each `tests/fixtures/<name>.nfo` is the output of running
//! `/Users/apogliaghi/Development/metube_pot/app/jellyfin_nfo_generator.py`'s `create_nfo_xml`
//! over the fixture of the same name, with only `<dateadded>` (a `datetime.now()` in legacy, a
//! clock parameter here) normalised to `FIXED` on both sides.
//!
//! Regenerate with:
//!
//! ```text
//! python3 - <<'PY'
//! import importlib.util, json, re, pathlib
//! spec = importlib.util.spec_from_file_location(
//!     "legacy_nfo", "/Users/apogliaghi/Development/metube_pot/app/jellyfin_nfo_generator.py")
//! m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
//! base = pathlib.Path("crates/aulos-hooks/tests/fixtures")
//! for src, dst in [("youtube_info.json", "youtube_info.nfo"),
//!                  ("youtube_series_info.json", "youtube_series_info.nfo"),
//!                  ("youtube_sparse_info.json", "youtube_sparse_info.nfo"),
//!                  ("youtube_minimal_info.json", "youtube_minimal_info.nfo")]:
//!     xml = m.create_nfo_xml(json.loads((base / src).read_text()))
//!     xml = re.sub(r"<dateadded>[^<]*</dateadded>", "<dateadded>FIXED</dateadded>", xml)
//!     (base / dst).write_text(xml)
//! PY
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use aulos_core::item::EntryBlob;
use aulos_hooks::nfo::{self, Source::Sidecar};
use common::ItemBuilder;

const NOW: i64 = 1_788_480_000_000; // 2026-09-04T00:00:00Z

fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn info_json(name: &str) -> EntryBlob {
    let text = std::fs::read_to_string(fixture_path(name)).expect("the info.json fixture");
    EntryBlob::new(serde_json::from_str(&text).expect("the fixture must be JSON"))
}

fn legacy_output(name: &str) -> String {
    std::fs::read_to_string(fixture_path(name)).expect("the captured legacy output")
}

/// `<dateadded>` is the one element that cannot agree: legacy stamped it from `datetime.now()`.
fn normalise(xml: &str) -> String {
    xml.lines()
        .map(|line| {
            if line.trim_start().starts_with("<dateadded>") {
                let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
                format!("{indent}<dateadded>FIXED</dateadded>")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The exact shape of the download in the production bug report: a YouTube video, no series keys,
/// an ampersand and angle brackets in the description, an empty tag in the middle of the list and
/// a fractional duration.
#[test]
fn a_youtube_info_json_renders_exactly_what_the_legacy_script_rendered() {
    let view = ItemBuilder::finished("Le incredibili elezioni del 2000")
        .provider("ytdlp")
        .view();
    let ours = nfo::render(&view, &info_json("youtube_info.json"), Sidecar, NOW).expect("render");
    assert_eq!(normalise(&ours), legacy_output("youtube_info.nfo"));
}

/// The episode branch: `series`/`season_number`/`episode_number` make it `episodedetails`, and
/// `original_url` wins over `webpage_url` for `<website>`.
#[test]
fn a_series_info_json_renders_as_episodedetails_exactly_as_legacy_did() {
    let view = ItemBuilder::finished("Ci vuole una scienza - S02E07")
        .provider("ytdlp")
        .view();
    let ours =
        nfo::render(&view, &info_json("youtube_series_info.json"), Sidecar, NOW).expect("render");
    let theirs = legacy_output("youtube_series_info.nfo");
    assert!(theirs.contains("<episodedetails>"), "{theirs}");
    assert_eq!(normalise(&ours), theirs);
}

/// The normalisation may not be doing the work: the two documents differ in nothing else, so
/// swapping one element's text has to break the comparison.
#[test]
fn the_comparison_would_notice_a_difference() {
    let view = ItemBuilder::finished("Le incredibili elezioni del 2000")
        .provider("ytdlp")
        .view();
    let ours = nfo::render(&view, &info_json("youtube_info.json"), Sidecar, NOW).expect("render");
    let tampered = legacy_output("youtube_info.nfo").replace("<runtime>24", "<runtime>25");
    assert_ne!(normalise(&ours), tampered);
}

/// The three places the renderer used to be more helpful than legacy — and therefore wrong about
/// the promise that the same `.info.json` yields the same bytes. This one sidecar has all three:
/// an empty `title` (legacy's `"Unknown Title"` default fires on an **absent** key only), an
/// `uploader` that is present and `null` (legacy's `info.get("uploader", info.get("channel", ""))`
/// selects the `None`, so `channel` is *not* consulted and no `<studio>`/`<director>` is written),
/// and no url of either spelling (legacy writes no `<website>` rather than reaching for the row).
#[test]
fn a_sparse_sidecar_gets_legacys_own_fallbacks_not_better_ones() {
    let view = ItemBuilder::finished("Titolo dalla riga")
        .provider("ytdlp")
        .view();
    let ours =
        nfo::render(&view, &info_json("youtube_sparse_info.json"), Sidecar, NOW).expect("render");
    assert_eq!(normalise(&ours), legacy_output("youtube_sparse_info.nfo"));
    assert!(
        !ours.contains("<studio>"),
        "a null uploader writes none: {ours}"
    );
    assert!(
        !ours.contains("<website>"),
        "and no url writes none: {ours}"
    );
    assert!(
        !ours.contains("Titolo dalla riga"),
        "the row may not leak into a sidecar rendering: {ours}"
    );
}

/// The other end of the same rule: an empty object is still a sidecar, and legacy rendered one.
/// `Unknown Title` is not a good title, but it is the one the script wrote, and the file it wrote
/// is the file a cutover must keep producing.
#[test]
fn an_empty_sidecar_renders_the_stub_legacy_rendered() {
    let view = ItemBuilder::finished("Titolo dalla riga")
        .provider("ytdlp")
        .view();
    let ours =
        nfo::render(&view, &info_json("youtube_minimal_info.json"), Sidecar, NOW).expect("render");
    assert_eq!(normalise(&ours), legacy_output("youtube_minimal_info.nfo"));
}
