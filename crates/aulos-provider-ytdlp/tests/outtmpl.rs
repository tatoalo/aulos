//! The output-template acceptance tests of PLAN WP-06: the playlist/channel swap, the empty-value
//! rule, sanitisation of string values only, and the `mode = "outtmpl"` job shape of DESIGN §9.2.
//!
//! No Python and no network: the shim's half of the round trip is simulated by feeding
//! [`OutTmplJob::apply`] the strings yt-dlp's `evaluate_outtmpl` would have returned. What is
//! under test is which template is chosen, which field references are extracted, what info they
//! are evaluated against, and how the results are spliced back — everything except the evaluation
//! itself, which is deliberately yt-dlp's job (see the `outtmpl` module docs).

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: a panic IS the failure

use aulos_core::config::{Config, RawEnv, load};
use aulos_core::request::DownloadRequest;
use aulos_core::selection::{Codec, DownloadType, FormatId, QualityId, Selection};
use aulos_provider::entry::EntryHints;
use aulos_provider_ytdlp::{OutTmplError, build_outtmpl};
use serde_json::{Value, json};
use url::Url;

/// A config with the four `OUTPUT_TEMPLATE*` values set and everything else defaulted.
fn config(playlist: &str, channel: &str) -> Config {
    let dir = std::env::temp_dir().join("aulos-wp06-outtmpl");
    std::fs::create_dir_all(&dir).unwrap();
    load(&RawEnv::from_pairs([
        ("DOWNLOAD_DIR", dir.display().to_string()),
        ("STATE_DIR", dir.display().to_string()),
        ("TEMP_DIR", dir.display().to_string()),
        ("OUTPUT_TEMPLATE", "%(title)s.%(ext)s".to_owned()),
        (
            "OUTPUT_TEMPLATE_CHAPTER",
            "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s".to_owned(),
        ),
        ("OUTPUT_TEMPLATE_PLAYLIST", playlist.to_owned()),
        ("OUTPUT_TEMPLATE_CHANNEL", channel.to_owned()),
    ]))
    .unwrap_or_else(|e| panic!("config must load: {e:?}"))
}

/// The legacy defaults for the two container templates.
fn default_config() -> Config {
    config(
        "%(playlist_title)s/%(title)s.%(ext)s",
        "%(channel)s/%(title)s.%(ext)s",
    )
}

fn request() -> DownloadRequest {
    DownloadRequest::new(
        Url::parse("https://www.youtube.com/watch?v=dQw4w9WgXcQ").unwrap(),
        Selection::new(
            DownloadType::Video,
            Codec::Auto,
            FormatId::parse("mp4").unwrap(),
            QualityId::parse("1080").unwrap(),
        ),
    )
}

fn in_playlist(title: &str, index: u32) -> EntryHints {
    EntryHints {
        playlist_index: Some(index),
        playlist_count: Some(12),
        playlist_title: Some(title.into()),
        ..EntryHints::default()
    }
}

fn in_channel(title: &str, index: u32) -> EntryHints {
    EntryHints {
        channel_index: Some(index),
        channel_count: Some(99),
        channel_title: Some(title.into()),
        ..EntryHints::default()
    }
}

#[test]
fn a_single_video_needs_no_shim_call() {
    let job = build_outtmpl(&default_config(), &request(), &EntryHints::default());
    assert!(job.is_ready());
    assert!(job.templates().is_empty());
    assert!(job.info().is_empty());
    assert!(job.prefixes().is_empty());
    let t = job.ready().unwrap();
    assert_eq!(t.default, "%(title)s.%(ext)s");
    assert_eq!(
        t.chapter,
        "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s"
    );
}

#[test]
fn the_playlist_template_replaces_the_default_only_when_playlist_index_is_present() {
    let cfg = default_config();
    let req = request();

    // Absent: the default survives untouched.
    let single = build_outtmpl(&cfg, &req, &EntryHints::default());
    assert_eq!(single.ready().unwrap().default, "%(title)s.%(ext)s");

    // Present: the playlist template takes over and its `playlist*` field is extracted.
    let child = build_outtmpl(&cfg, &req, &in_playlist("Mix - lofi", 3));
    assert!(!child.is_ready());
    assert_eq!(child.templates(), ["%(playlist_title)s"]);
    assert_eq!(child.prefixes(), ["playlist"]);
    let t = child.apply(&["Mix - lofi".to_owned()]).unwrap();
    assert_eq!(t.default, "Mix - lofi/%(title)s.%(ext)s");
}

#[test]
fn an_empty_output_template_playlist_keeps_the_default() {
    let cfg = config("", "");
    let job = build_outtmpl(&cfg, &request(), &in_playlist("Mix", 3));
    // The template is unchanged, and it references nothing playlist-shaped, so there is nothing
    // to pre-resolve and no shim call — but the prefix is still active, as in legacy.
    assert!(job.is_ready());
    assert_eq!(job.ready().unwrap().default, "%(title)s.%(ext)s");
    assert_eq!(job.prefixes(), ["playlist"]);
}

#[test]
fn an_empty_output_template_playlist_still_pre_resolves_a_playlist_field_in_the_default() {
    // `OUTPUT_TEMPLATE` itself may reference the playlist; legacy resolved it in that case too,
    // because the pass runs whether or not the swap happened.
    let cfg = config("", "");
    let mut cfg = cfg;
    cfg.output_template = "%(playlist_index)03d - %(title)s.%(ext)s".into();
    let job = build_outtmpl(&cfg, &request(), &in_playlist("Mix", 7));
    assert_eq!(job.templates(), ["%(playlist_index)03d"]);
    assert_eq!(job.info()["playlist_index"], json!(7));
    let t = job.apply(&["007".to_owned()]).unwrap();
    assert_eq!(t.default, "007 - %(title)s.%(ext)s");
}

#[test]
fn the_channel_template_wins_over_the_playlist_one_and_discards_its_pass() {
    // Both indices present and both templates set: legacy reassigned `output`, throwing the
    // playlist-resolved string away, so only `channel*` is pre-resolved.
    let cfg = default_config();
    let mut hints = in_playlist("Mix", 3);
    hints.channel_index = Some(1);
    hints.channel_title = Some("Chillhop".into());

    let job = build_outtmpl(&cfg, &request(), &hints);
    assert_eq!(job.prefixes(), ["channel"]);
    assert_eq!(job.templates(), ["%(channel)s"]);
    assert_eq!(
        job.apply(&["Chillhop".to_owned()]).unwrap().default,
        "Chillhop/%(title)s.%(ext)s"
    );
}

#[test]
fn an_empty_channel_template_leaves_both_prefixes_active_on_the_playlist_template() {
    let cfg = config("%(playlist_title)s/%(channel)s/%(title)s.%(ext)s", "");
    let mut hints = in_playlist("Mix", 3);
    hints.channel_index = Some(1);
    hints.channel_title = Some("Chillhop".into());

    let job = build_outtmpl(&cfg, &request(), &hints);
    assert_eq!(job.prefixes(), ["playlist", "channel"]);
    assert_eq!(job.templates(), ["%(playlist_title)s", "%(channel)s"]);
    let t = job
        .apply(&["Mix".to_owned(), "Chillhop".to_owned()])
        .unwrap();
    assert_eq!(t.default, "Mix/Chillhop/%(title)s.%(ext)s");
}

#[test]
fn windows_invalid_characters_are_replaced_in_strings_only_not_in_numbers() {
    let cfg = config(
        r#"%(playlist_title)s/%(playlist_index)02d/%(title)s.%(ext)s"#,
        "",
    );
    let job = build_outtmpl(
        &cfg,
        &request(),
        &in_playlist(r#"AC\DC: Live? "Best"|Mix"#, 4),
    );

    // Every one of `\ : * ? " < > |` becomes `_`; the forward slash is deliberately untouched,
    // as in legacy.
    assert_eq!(
        job.info()["playlist_title"],
        json!("AC_DC_ Live_ _Best__Mix")
    );
    assert_eq!(job.info()["playlist"], job.info()["playlist_title"]);
    // Numbers pass through, which is what keeps `%(playlist_index)02d` working.
    assert_eq!(job.info()["playlist_index"], json!(4));
    assert_eq!(job.info()["playlist_count"], json!(12));
    assert!(job.info()["playlist_index"].is_number());
}

#[test]
fn merge_info_sanitises_and_overwrites() {
    let cfg = default_config();
    let mut job = build_outtmpl(&cfg, &request(), &in_playlist("Mix", 3));
    let mut extra = serde_json::Map::new();
    extra.insert("playlist_id".to_owned(), json!("PL9:tY"));
    extra.insert("playlist_count".to_owned(), json!(500));
    job.merge_info(&extra);
    assert_eq!(job.info()["playlist_id"], json!("PL9_tY"));
    assert_eq!(job.info()["playlist_count"], json!(500));
    // The templates found are unaffected by extra info.
    assert_eq!(job.templates(), ["%(playlist_title)s"]);
}

#[test]
fn the_job_has_the_design_9_2_shape() {
    let cfg = default_config();
    let job = build_outtmpl(&cfg, &request(), &in_playlist("Mix", 3));
    let v = job.to_job("01JBQ7Z5T9K3M2R8V4XW6Y0AAA");
    assert_eq!(v["v"], json!(1));
    assert_eq!(v["protocol"], json!(1));
    assert_eq!(v["job_id"], json!("01JBQ7Z5T9K3M2R8V4XW6Y0AAA"));
    assert_eq!(v["mode"], json!("outtmpl"));
    assert_eq!(v["templates"], json!(["%(playlist_title)s"]));
    assert_eq!(v["prefixes"], json!(["playlist"]));
    assert_eq!(v["info"]["playlist_title"], json!("Mix"));
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        [
            "info",
            "job_id",
            "mode",
            "prefixes",
            "protocol",
            "templates",
            "v"
        ]
    );
}

#[test]
fn a_shim_result_of_the_wrong_length_is_a_contract_error() {
    let cfg = default_config();
    let job = build_outtmpl(&cfg, &request(), &in_playlist("Mix", 3));
    assert_eq!(
        job.apply(&["a".to_owned(), "b".to_owned()]).unwrap_err(),
        OutTmplError::Arity {
            expected: 1,
            got: 2
        }
    );
}

#[test]
fn the_custom_name_prefix_applies_to_the_default_template_only() {
    let cfg = default_config();
    let mut req = request();
    req.custom_name_prefix = "S01E02".into();

    let single = build_outtmpl(&cfg, &req, &EntryHints::default());
    assert_eq!(single.ready().unwrap().default, "S01E02.%(title)s.%(ext)s");

    // A playlist template replaces the whole string, prefix included — legacy behaviour.
    let child = build_outtmpl(&cfg, &req, &in_playlist("Mix", 3));
    assert_eq!(
        child.apply(&["Mix".to_owned()]).unwrap().default,
        "Mix/%(title)s.%(ext)s"
    );
}

#[test]
fn the_chapter_template_comes_from_the_request_only_when_split_by_chapters_is_set() {
    let cfg = default_config();
    let mut req = request();
    req.chapter_template = "%(section_title)s.%(ext)s".into();

    // Not splitting: legacy ignored the request's template entirely.
    let ignored = build_outtmpl(&cfg, &req, &EntryHints::default());
    assert_eq!(
        ignored.ready().unwrap().chapter,
        "%(title)s - %(section_number)02d - %(section_title)s.%(ext)s"
    );

    req.split_by_chapters = true;
    let used = build_outtmpl(&cfg, &req, &EntryHints::default());
    assert_eq!(used.ready().unwrap().chapter, "%(section_title)s.%(ext)s");

    // And the chapter template is never pre-resolved, even when it names a playlist field.
    req.chapter_template = "%(playlist_title)s - %(section_title)s.%(ext)s".into();
    let job = build_outtmpl(&cfg, &req, &in_playlist("Mix", 3));
    assert_eq!(job.templates(), ["%(playlist_title)s"]);
    assert_eq!(
        job.apply(&["Mix".to_owned()]).unwrap().chapter,
        "%(playlist_title)s - %(section_title)s.%(ext)s"
    );
}

#[test]
fn the_info_dict_carries_the_aliases_yt_dlp_children_have() {
    let cfg = default_config();
    let job = build_outtmpl(&cfg, &request(), &in_channel("Chill:hop", 5));
    let info = job.info();
    assert_eq!(info["channel"], json!("Chill_hop"));
    assert_eq!(info["channel_title"], json!("Chill_hop"));
    assert_eq!(info["channel_index"], json!(5));
    assert_eq!(info["channel_count"], json!(99));
    // Nothing playlist-shaped is invented for a channel entry.
    assert!(!info.contains_key("playlist_title"));

    let job = build_outtmpl(&cfg, &request(), &in_playlist("Mix", 3));
    let info = job.info();
    assert_eq!(info["playlist"], json!("Mix"));
    assert_eq!(info["playlist_autonumber"], json!(3));
    assert_eq!(info["n_entries"], json!(12));
    // A key legacy had from the full info dict and `EntryHints` does not carry is simply absent,
    // so yt-dlp resolves it to `NA` unless the engine supplies it via `merge_info`.
    assert!(!info.contains_key("playlist_id"));
    assert_eq!(Value::Object(info.clone()).as_object().unwrap().len(), 6);
}
