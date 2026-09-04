//! Path containment (DESIGN §4.5, §16.6).
//!
//! The legacy server compared a resolved path against its base with `str.startswith`, which let
//! `/downloads-evil` pass as inside `/downloads`. These tests pin the component-wise replacement,
//! including the symlink-escape case a purely lexical check cannot catch.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code: a panic IS the failure

use std::fs;
use std::path::Path;

use aulos_core::paths::{PathError, Paths, RelDir};
use aulos_core::{DownloadType, contain, sanitize_path_component};

/// A base directory plus a sibling whose name has the base as a string prefix.
struct Fixture {
    _root: tempfile::TempDir,
    base: std::path::PathBuf,
    evil_sibling: std::path::PathBuf,
    outside: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let base = root.path().join("downloads");
    let evil_sibling = root.path().join("downloads-evil");
    let outside = root.path().join("elsewhere");
    for d in [&base, &evil_sibling, &outside] {
        fs::create_dir_all(d).expect("mkdir");
    }
    Fixture {
        _root: root,
        base,
        evil_sibling,
        outside,
    }
}

#[test]
fn a_plain_subdirectory_is_contained() {
    let f = fixture();
    fs::create_dir(f.base.join("Music")).expect("mkdir");
    let got = contain(&f.base, Path::new("Music")).expect("Music is inside the base");
    assert!(got.ends_with("downloads/Music"), "{got:?}");
    assert!(got.is_absolute());
}

#[test]
fn a_subdirectory_that_does_not_exist_yet_is_contained() {
    // `CREATE_CUSTOM_DIRS=true` checks containment *before* creating the directory, so the
    // candidate's tail need not exist.
    let f = fixture();
    let got = contain(&f.base, Path::new("Shows/Season 1")).expect("not yet created but inside");
    assert!(got.ends_with("downloads/Shows/Season 1"), "{got:?}");
}

#[test]
fn parent_traversal_is_rejected() {
    let f = fixture();
    for candidate in [
        "..",
        "../elsewhere",
        "Music/../../elsewhere",
        "a/b/../../..",
    ] {
        let err = contain(&f.base, Path::new(candidate)).expect_err(candidate);
        assert!(
            matches!(err, PathError::ParentTraversal { .. }),
            "{candidate}: {err:?}"
        );
    }
}

#[test]
fn a_sibling_whose_name_has_the_base_as_a_string_prefix_is_rejected() {
    // This is the legacy `startswith` bug: "/downloads-evil".startswith("/downloads") is true.
    let f = fixture();
    let err =
        contain(&f.base, &f.evil_sibling).expect_err("downloads-evil is not inside downloads");
    assert!(matches!(err, PathError::Escapes { .. }), "{err:?}");

    // And the containment test really is component-wise, not string-prefix.
    let base_str = f.base.to_string_lossy().into_owned();
    let evil_str = f.evil_sibling.to_string_lossy().into_owned();
    assert!(
        evil_str.starts_with(&base_str),
        "the fixture must reproduce the legacy trap"
    );
}

#[test]
fn an_unrelated_absolute_path_is_rejected() {
    let f = fixture();
    let err = contain(&f.base, &f.outside).expect_err("elsewhere is outside");
    assert!(matches!(err, PathError::Escapes { .. }), "{err:?}");
    assert!(
        err.to_string().contains("is outside"),
        "the message should name both paths: {err}"
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_that_escapes_the_base_is_rejected() {
    let f = fixture();
    fs::write(f.outside.join("secret.txt"), b"x").expect("write");
    std::os::unix::fs::symlink(&f.outside, f.base.join("escape")).expect("symlink");

    // The link itself resolves outside the base.
    let err = contain(&f.base, Path::new("escape")).expect_err("the link escapes");
    assert!(matches!(err, PathError::Escapes { .. }), "{err:?}");

    // So does anything under it, including a file that does not exist yet.
    let err = contain(&f.base, Path::new("escape/new.txt")).expect_err("through the link");
    assert!(matches!(err, PathError::Escapes { .. }), "{err:?}");
}

#[cfg(unix)]
#[test]
fn a_symlink_that_stays_inside_the_base_is_accepted() {
    let f = fixture();
    let real = f.base.join("real");
    fs::create_dir(&real).expect("mkdir");
    std::os::unix::fs::symlink(&real, f.base.join("link")).expect("symlink");
    let got = contain(&f.base, Path::new("link/file.mkv")).expect("still inside");
    assert!(got.ends_with("downloads/real/file.mkv"), "{got:?}");
}

#[test]
fn a_missing_base_is_reported_as_the_operators_problem_not_the_requests() {
    let f = fixture();
    let err = contain(&f.base.join("nope"), Path::new("x")).expect_err("no such base");
    assert!(matches!(err, PathError::Base { .. }), "{err:?}");
    assert_eq!(err.code(), aulos_core::ErrorCode::Internal);
    // Every request-side failure is the request's fault.
    let request_side = contain(&f.base, Path::new("..")).expect_err("traversal");
    assert_eq!(request_side.code(), aulos_core::ErrorCode::FolderInvalid);
}

#[test]
fn the_base_itself_is_contained() {
    let f = fixture();
    let got = contain(&f.base, Path::new("")).expect("the base is inside itself");
    assert_eq!(got, f.base.canonicalize().expect("canonicalize"));
}

#[test]
fn paths_out_dir_routes_audio_to_the_audio_root_and_checks_containment() {
    let f = fixture();
    let audio = f.base.join("audio");
    fs::create_dir(&audio).expect("mkdir");
    let paths = Paths {
        download: f.base.clone(),
        audio_download: audio.clone(),
        temp: f.base.clone(),
        state: f.base.clone(),
    };

    assert_eq!(paths.root_for(DownloadType::Audio), audio.as_path());
    assert_eq!(paths.root_for(DownloadType::Video), f.base.as_path());
    assert_eq!(paths.root_for(DownloadType::Captions), f.base.as_path());

    let folder = RelDir::parse("Albums/Live").expect("valid folder");
    let got = paths
        .out_dir(DownloadType::Audio, Some(&folder))
        .expect("inside the audio root");
    assert!(got.ends_with("audio/Albums/Live"), "{got:?}");

    let bare = paths
        .out_dir(DownloadType::Video, None)
        .expect("no folder means the root itself");
    assert_eq!(bare, f.base.canonicalize().expect("canonicalize"));
}

#[cfg(unix)]
#[test]
fn out_dir_rejects_a_folder_that_escapes_through_a_symlink() {
    let f = fixture();
    std::os::unix::fs::symlink(&f.outside, f.base.join("escape")).expect("symlink");
    let paths = Paths {
        download: f.base.clone(),
        audio_download: f.base.clone(),
        temp: f.base.clone(),
        state: f.base.clone(),
    };
    let folder = RelDir::parse("escape/loot").expect("lexically valid");
    let err = paths
        .out_dir(DownloadType::Video, Some(&folder))
        .expect_err("the symlink escapes");
    assert!(matches!(err, PathError::Escapes { .. }), "{err:?}");
}

#[test]
fn sanitize_path_component_is_the_legacy_character_class() {
    // `[\\:*?"<>|]` → `_`, everything else untouched. Ported from `app/ytdl.py`.
    assert_eq!(
        sanitize_path_component(r#"Rick: "Never" <Gonna> Give|You*Up?\Ever"#),
        "Rick_ _Never_ _Gonna_ Give_You_Up__Ever"
    );
    assert_eq!(
        sanitize_path_component("Season 1/Episode 2"),
        "Season 1/Episode 2"
    );
    assert_eq!(sanitize_path_component(""), "");
    // Unicode is preserved.
    assert_eq!(sanitize_path_component("Läuft — 日本語"), "Läuft — 日本語");
}
