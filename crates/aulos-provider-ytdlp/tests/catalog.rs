//! The `ytdlp` catalogue is asserted three ways: against an `insta` snapshot (so any change to
//! its wire shape is a reviewed diff), against the **DESIGN §6.6 markdown table** itself (so the
//! document and the code cannot drift apart silently), and against the two notices §6.6 requires
//! the honest labelling of `best_remux` and `worst` to carry.
//!
//! Parsing the design table rather than retyping it is deliberate: a hand-copied expectation in a
//! test is one more place the matrix can be wrong. The catalogue *data* lives in `aulos-core`
//! (§6.6 is a wire type), so this file is the provider's proof that what it advertises is what the
//! design says, not a second definition.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: a panic IS the failure

use std::path::PathBuf;

use aulos_provider_ytdlp::ytdlp_catalog;

/// The catalogue as `(download_type, format, [qualities])`, in declaration order.
fn actual_rows() -> Vec<(String, String, Vec<String>)> {
    ytdlp_catalog()
        .download_types
        .iter()
        .flat_map(|dt| {
            dt.formats.iter().map(move |f| {
                (
                    dt.id.to_string(),
                    f.id.to_string(),
                    f.qualities.iter().map(|q| q.id.to_string()).collect(),
                )
            })
        })
        .collect()
}

/// The DESIGN §6.6 table, parsed out of the document.
///
/// The table's format cell packs several ids per row (`opus` / `wav` / `flac`, and the seven
/// caption formats), so one markdown row expands to several catalogue rows — which is exactly how
/// a reader reads it.
fn design_rows() -> Vec<(String, String, Vec<String>)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("docs/DESIGN.md");
    let design = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

    let section = design
        .split("### 6.6 ")
        .nth(1)
        .expect("DESIGN must still have a §6.6")
        .split("\n### ")
        .next()
        .expect("a section always has a first chunk");
    let header = "| download_type | format | qualities |";
    let table = section
        .split(header)
        .nth(1)
        .expect("DESIGN §6.6 must still contain the download_type/format/qualities table");

    let clean = |s: &str| s.replace(['`', '*'], "").trim().to_owned();
    let mut rows = Vec::new();
    for line in table.lines().map(str::trim) {
        if line.is_empty() {
            continue; // the newline that follows the header row
        }
        if !line.starts_with('|') {
            break; // the table ended
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').collect();
        if cells.iter().all(|c| c.trim().starts_with("---")) {
            continue; // the markdown separator row
        }
        assert_eq!(cells.len(), 3, "unexpected §6.6 row {line:?}");
        let dt = clean(cells[0]);
        let qualities: Vec<String> = cells[2].split(',').map(clean).collect();
        for format in cells[1].split(['/', ',']) {
            rows.push((dt.clone(), clean(format), qualities.clone()));
        }
    }
    assert!(!rows.is_empty(), "the §6.6 table parsed to nothing");
    rows
}

#[test]
fn the_catalog_matches_the_design_6_6_table_exactly() {
    assert_eq!(actual_rows(), design_rows());
}

#[test]
fn the_two_honest_notices_are_present() {
    let catalog = ytdlp_catalog();
    let video = catalog
        .download_types
        .iter()
        .find(|d| &*d.id == "video")
        .unwrap();

    // `best_remux` exists only on `mp4` and says that it re-encodes.
    let mp4 = video.format("mp4").unwrap();
    let remux = mp4.quality("best_remux").unwrap();
    let notice = remux.notice.as_deref().unwrap();
    assert!(
        notice.contains("Re-encodes audio") && notice.contains("SponsorBlock"),
        "the best_remux notice must explain the cost: {notice:?}"
    );
    assert!(mp4.flags.slow, "best_remux is the slow path (§6.6)");
    for other in ["any", "ios"] {
        assert!(
            video.format(other).unwrap().quality("best_remux").is_none(),
            "{other} must not offer best_remux"
        );
    }

    // `worst` no longer lies: the legacy quirk is kept, the label is not.
    for format in ["any", "mp4", "ios"] {
        let worst = video.format(format).unwrap().quality("worst").unwrap();
        assert_eq!(
            worst.notice.as_deref(),
            Some("This selector currently resolves to the best available stream"),
            "{format}/worst must carry the honest notice (Δ C21)"
        );
    }
}

#[test]
fn the_catalog_wire_shape_matches_its_snapshot() {
    insta::assert_json_snapshot!(&*ytdlp_catalog());
}
