//! Compare the yt-dlp catalog with its wire snapshot and documented format table.

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

/// Read the format table from docs/PROTOCOL.md.
///
/// The table's format cell packs several ids per row (`opus` / `wav` / `flac`, and the seven
/// caption formats), so one markdown row expands to several catalogue rows, which is how
/// a reader reads it.
fn documented_rows() -> Vec<(String, String, Vec<String>)> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("docs/PROTOCOL.md");
    let protocol = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));

    let section = protocol
        .split("## 8. Format and quality selection")
        .nth(1)
        .expect("the protocol must document format selection")
        .split("\n## ")
        .next()
        .expect("a section always has a first chunk");
    let header = "| Download type | Format | Quality |";
    let table = section
        .split(header)
        .nth(1)
        .expect("the protocol must contain a download type, format and quality table");

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
        assert_eq!(cells.len(), 3, "unexpected format table row {line:?}");
        let dt = clean(cells[0]);
        let qualities: Vec<String> = cells[2].split(',').map(clean).collect();
        for format in cells[1].split(['/', ',']) {
            rows.push((dt.clone(), clean(format), qualities.clone()));
        }
    }
    assert!(!rows.is_empty(), "the format table parsed to nothing");
    rows
}

#[test]
fn the_catalog_matches_the_documented_format_table() {
    assert_eq!(actual_rows(), documented_rows());
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
