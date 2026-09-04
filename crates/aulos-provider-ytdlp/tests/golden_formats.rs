//! Replays the WP-00 golden corpus for `dl_formats.get_format` against
//! [`aulos_provider_ytdlp::get_format_raw`].
//!
//! The corpus is `tests/golden/formats.json` at the **workspace** root, dumped from the legacy
//! `app/dl_formats.py` by `tools/capture/dump_formats.py` and checked in with provenance. It
//! carries three sections and this file replays all three:
//!
//! - `selectors` — every `(download_type, codec, format, quality)` tuple the legacy add API
//!   admits, mapped to its selector string. Every one must be byte-identical.
//! - `edge_cases` — the inputs the tuple sweep cannot reach: `custom:`, `None`, `""`, whitespace
//!   and case, an unknown codec, and the three documented quirks.
//! - `errors` — the three `ValueError`s, whose message text the v1 shim echoes verbatim.
//!
//! Plus the cross-check PLAN WP-06 asks for: the tuple set the DESIGN §6.6 catalog admits and the
//! key set of `selectors` must be **equal**. A catalog entry with no golden vector is a hole in
//! the proof, and a golden vector with no catalog entry is a selector nothing can request.
//!
//! There is no network and no Python here: the corpus is data.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: a panic IS the failure

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use aulos_core::catalog::FormatCatalog;
use aulos_provider_ytdlp::{CODEC_FILTER_MAP, get_format, get_format_raw, ytdlp_catalog};
use serde::Deserialize;

/// The golden file's top level.
#[derive(Deserialize)]
struct Golden {
    /// `download_type|codec|format|quality` → selector.
    selectors: BTreeMap<String, String>,
    edge_cases: Vec<EdgeCase>,
    errors: Vec<ErrorCase>,
    codec_filter_map: BTreeMap<String, String>,
    key_format: String,
    provenance: Provenance,
}

/// The four free-form arguments, exactly as Python received them.
#[derive(Deserialize)]
struct Args {
    download_type: Option<String>,
    codec: Option<String>,
    format: Option<String>,
    quality: Option<String>,
}

#[derive(Deserialize)]
struct EdgeCase {
    name: String,
    args: Args,
    selector: String,
    why: String,
}

#[derive(Deserialize)]
struct ErrorCase {
    name: String,
    args: Args,
    /// A one-entry map: exception class → message.
    raises: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Provenance {
    legacy_commit: String,
    legacy_module: String,
    tool: String,
}

fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("tests/golden/formats.json")
}

fn load() -> Golden {
    let path = golden_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("cannot parse {}: {e}", path.display()))
}

fn call(args: &Args) -> Result<String, String> {
    get_format_raw(
        args.download_type.as_deref(),
        args.codec.as_deref(),
        args.format.as_deref(),
        args.quality.as_deref(),
    )
    .map_err(|e| e.to_string())
}

/// Every tuple `(download_type, codec, format, quality)` the catalog admits, as a golden key.
///
/// A `FormatSpec` with no codecs means the control does not apply, and legacy sent `auto` for
/// those, so the tuple is keyed with `auto`.
fn catalog_keys(catalog: &FormatCatalog) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for dt in &catalog.download_types {
        for f in &dt.formats {
            let codecs: Vec<&str> = if f.codecs.is_empty() {
                vec!["auto"]
            } else {
                f.codecs.iter().map(|c| &*c.id).collect()
            };
            for codec in codecs {
                for q in &f.qualities {
                    out.insert(format!("{}|{codec}|{}|{}", dt.id, f.id, q.id));
                }
            }
        }
    }
    out
}

#[test]
fn the_corpus_is_the_one_we_think_it_is() {
    let g = load();
    assert_eq!(g.key_format, "download_type|codec|format|quality");
    assert_eq!(g.provenance.legacy_module, "app/dl_formats.py");
    assert_eq!(g.provenance.tool, "tools/capture/dump_formats.py");
    assert_eq!(g.provenance.legacy_commit.len(), 40);
    assert!(!g.selectors.is_empty());
}

#[test]
fn every_golden_tuple_produces_the_legacy_selector() {
    let g = load();
    let mut wrong = Vec::new();
    for (key, expected) in &g.selectors {
        let parts: Vec<&str> = key.split('|').collect();
        assert_eq!(parts.len(), 4, "malformed golden key {key:?}");
        let args = Args {
            download_type: Some(parts[0].to_owned()),
            codec: Some(parts[1].to_owned()),
            format: Some(parts[2].to_owned()),
            quality: Some(parts[3].to_owned()),
        };
        match call(&args) {
            Ok(got) if &got == expected => {}
            Ok(got) => wrong.push(format!("{key}\n  want {expected}\n  got  {got}")),
            Err(e) => wrong.push(format!("{key}\n  want {expected}\n  got  ValueError: {e}")),
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {} golden selectors differ:\n{}",
        wrong.len(),
        g.selectors.len(),
        wrong.join("\n")
    );
}

#[test]
fn every_golden_edge_case_reproduces() {
    let g = load();
    assert!(!g.edge_cases.is_empty());
    for case in &g.edge_cases {
        let got = call(&case.args)
            .unwrap_or_else(|e| panic!("{}: unexpected ValueError {e} ({})", case.name, case.why));
        assert_eq!(got, case.selector, "{}: {}", case.name, case.why);
    }
}

#[test]
fn every_golden_error_reproduces_the_valueerror_text() {
    let g = load();
    assert_eq!(g.errors.len(), 3, "the three legacy ValueErrors");
    for case in &g.errors {
        let (class, message) = case
            .raises
            .iter()
            .next()
            .unwrap_or_else(|| panic!("{}: no exception recorded", case.name));
        assert_eq!(class, "ValueError", "{}", case.name);
        match call(&case.args) {
            Err(text) => assert_eq!(&text, message, "{}", case.name),
            Ok(got) => panic!("{}: expected {message:?}, got selector {got:?}", case.name),
        }
    }
}

#[test]
fn the_codec_filter_map_is_the_legacy_table() {
    let g = load();
    let ours: BTreeMap<String, String> = CODEC_FILTER_MAP
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
    assert_eq!(ours, g.codec_filter_map);
}

#[test]
fn the_catalog_tuple_set_and_the_golden_key_set_are_equal() {
    let g = load();
    let catalog = catalog_keys(&ytdlp_catalog());
    let golden: BTreeSet<String> = g.selectors.keys().cloned().collect();

    let missing: Vec<&String> = catalog.difference(&golden).collect();
    let extra: Vec<&String> = golden.difference(&catalog).collect();
    assert!(
        missing.is_empty(),
        "catalog tuples with no golden vector (regenerate the corpus): {missing:?}"
    );
    assert!(
        extra.is_empty(),
        "golden vectors no catalog entry can request (stale corpus or narrowed catalog): {extra:?}"
    );
    // 158: video 3 formats x 5 codecs x (9|10|9) heights, audio 3+4+1+1+1, captions 7, thumbnail 1.
    assert_eq!(catalog.len(), 158);
}

#[test]
fn the_typed_entry_point_agrees_with_the_raw_one_on_every_catalog_tuple() {
    use aulos_core::selection::{Codec, DownloadType};

    let catalog = ytdlp_catalog();
    let mut checked = 0_usize;
    for dt in &catalog.download_types {
        let typed_dt = dt.download_type().expect("a catalog id is a DownloadType");
        for f in &dt.formats {
            let codecs: Vec<Codec> = if f.codecs.is_empty() {
                vec![Codec::Auto]
            } else {
                f.codecs
                    .iter()
                    .map(|c| Codec::from_str_exact(&c.id).expect("a catalog codec id"))
                    .collect()
            };
            for codec in codecs {
                for q in &f.qualities {
                    let typed = get_format(typed_dt, codec, &f.id, &q.id).unwrap();
                    let raw = get_format_raw(
                        Some(dt.id.as_ref()),
                        Some(codec.as_str()),
                        Some(f.id.as_ref()),
                        Some(q.id.as_ref()),
                    )
                    .unwrap();
                    assert_eq!(typed, raw);
                    checked += 1;
                }
            }
        }
    }
    assert_eq!(checked, 158);

    // Every catalog `download_type` id is one of the four `DownloadType` values, which is what
    // makes the `expect` above sound rather than lucky.
    let ids: BTreeSet<String> = catalog
        .download_types
        .iter()
        .map(|d| d.id.to_string())
        .collect();
    let typed: BTreeSet<String> = DownloadType::ALL
        .into_iter()
        .map(|d| d.as_str().to_owned())
        .collect();
    assert_eq!(ids, typed);
}
