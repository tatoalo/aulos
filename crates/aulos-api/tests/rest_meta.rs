//! Discovery, options, cookies, subscriptions, health and the small top-level routes
//! (PROTOCOL §4.5–§4.7, §9, §1.5, §1.6), under both prefixes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use aulos_core::{ComponentHealth, ComponentStatus};
use serde_json::{Value, json};
use support::{Rig, for_each_prefix, normalise};

const YT: &str = "https://www.youtube.com/watch?v=dQw4w9WgXcQ";
const SC: &str = "https://streamingcommunity.test/titles/1-a-show";

// ---------------------------------------------------------------------------
// capabilities
// ---------------------------------------------------------------------------

#[tokio::test]
async fn capabilities_carries_the_whole_legacy_format_matrix() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (status, body) = rig.get("api/v2/capabilities").await;
        assert_eq!(status, 200, "{body}");

        // PROTOCOL §4.5: sixteen entries — three video, five audio, seven captions, one
        // thumbnail. Anything narrower silently deletes a format a legacy client could request.
        let formats = body["formats"].as_array().unwrap();
        let ytdlp_formats: Vec<&Value> = formats.iter().collect();
        assert_eq!(ytdlp_formats.len(), 16, "{formats:#?}");
        let by_type = |wanted: &str| -> Vec<&str> {
            formats
                .iter()
                .filter(|f| f["download_type"] == wanted)
                .map(|f| f["id"].as_str().unwrap())
                .collect()
        };
        assert_eq!(by_type("video"), ["any", "mp4", "ios"]);
        assert_eq!(by_type("audio"), ["m4a", "mp3", "opus", "wav", "flac"]);
        assert_eq!(
            by_type("captions"),
            ["srt", "txt", "vtt", "ttml", "sbv", "scc", "dfxp"],
            "all seven legacy caption formats"
        );
        assert_eq!(by_type("thumbnail"), ["jpg"]);

        let quality_ids = |id: &str| -> Vec<String> {
            formats.iter().find(|f| f["id"] == id).unwrap()["qualities"]
                .as_array()
                .unwrap()
                .iter()
                .map(|q| q["id"].as_str().unwrap().to_owned())
                .collect()
        };
        assert_eq!(
            quality_ids("ios"),
            [
                "best", "2160", "1440", "1080", "720", "480", "360", "240", "worst"
            ],
            "`ios` is a real video format with all nine heights (PROTOCOL §8)"
        );
        assert_eq!(quality_ids("any").len(), 9);
        assert!(
            quality_ids("mp4").contains(&"best_remux".to_owned()),
            "only mp4 has best_remux"
        );
        assert_eq!(quality_ids("srt"), ["best"]);

        assert_eq!(
            body["actions"],
            json!(["start", "pause", "cancel", "retry", "delete"])
        );
        assert_eq!(
            body["download_types"],
            json!(["video", "audio", "captions", "thumbnail"])
        );
        assert_eq!(
            body["codecs"],
            json!(["auto", "h264", "h265", "av1", "vp9"])
        );
        assert_eq!(
            body["subtitle_modes"],
            json!(["auto_only", "manual_only", "prefer_manual", "prefer_auto"])
        );
        assert!(
            body["features"]
                .as_array()
                .unwrap()
                .contains(&json!("cancel_resolve"))
        );
        assert_eq!(body["protocol"]["ws_subprotocol"], "aulos.v2");
        assert_eq!(body["protocol"]["socketio"], false);
        assert_eq!(body["url_prefix"], prefix);
        assert_eq!(body["config"]["default_format"], "mp4");
        assert_eq!(body["config"]["default_quality"], "best");
    })
    .await;
}

/// The second half of the PLAN's cross-check: `capabilities.formats` and the per-URL catalog have
/// to describe the same matrix, so neither can drift from PROTOCOL §4.5/§8 on its own.
#[tokio::test]
async fn the_flat_matrix_and_the_url_catalog_agree() {
    let rig = Rig::start("/").await;
    let (_, capabilities) = rig.get("api/v2/capabilities").await;
    let (_, catalog) = rig
        .get(&format!("api/v2/catalog?url={}", urlencoding(YT)))
        .await;

    for dt in catalog["download_types"].as_array().unwrap() {
        for format in dt["formats"].as_array().unwrap() {
            let flat = capabilities["formats"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["id"] == format["id"] && f["download_type"] == dt["id"])
                .unwrap_or_else(|| panic!("{} is not in capabilities.formats", format["id"]));
            let flat_qualities: Vec<&str> = flat["qualities"]
                .as_array()
                .unwrap()
                .iter()
                .map(|q| q["id"].as_str().unwrap())
                .collect();
            let rich_qualities: Vec<&str> = format["qualities"]
                .as_array()
                .unwrap()
                .iter()
                .map(|q| q["id"].as_str().unwrap())
                .collect();
            assert_eq!(
                flat_qualities, rich_qualities,
                "{} disagrees between the two payloads",
                format["id"]
            );
        }
    }
}

#[tokio::test]
async fn capabilities_is_etagged() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let response = rig.get_raw("api/v2/capabilities").await;
        let etag = response
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let response = rig
            .http
            .get(rig.url("api/v2/capabilities"))
            .header("if-none-match", &etag)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 304);
        assert!(response.text().await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn the_capabilities_payload_is_stable() {
    let rig = Rig::start("/").await;
    let (_, mut body) = rig.get("api/v2/capabilities").await;
    normalise(&mut body);
    insta::assert_json_snapshot!("capabilities", body);
}

// ---------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_per_url_catalog_answers_for_the_provider_that_would_run() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;

        let (status, ytdlp) = rig
            .get(&format!("api/v2/catalog?url={}", urlencoding(YT)))
            .await;
        assert_eq!(status, 200, "{ytdlp}");
        assert_eq!(ytdlp["provider"], "ytdlp");
        assert_eq!(ytdlp["naming"], "template");
        assert_eq!(ytdlp["match"]["reason"], "fallback");
        assert_eq!(ytdlp["match"]["score"], 1);
        assert!(
            ytdlp["runner_up"].is_null(),
            "nothing else claims a YouTube URL"
        );
        assert!(ytdlp["etag"].as_str().is_some());

        let (status, sc) = rig
            .get(&format!("api/v2/catalog?url={}", urlencoding(SC)))
            .await;
        assert_eq!(status, 200, "{sc}");
        assert_eq!(sc["provider"], "streamingcommunity");
        assert_eq!(
            sc["naming"], "provider",
            "the provider names the file itself"
        );
        assert_eq!(sc["match"]["reason"], "host_contains");
        assert_eq!(sc["match"]["score"], 200);
        assert_eq!(
            sc["runner_up"],
            json!({ "provider": "ytdlp", "score": 1 }),
            "the runner-up is named"
        );
        let format = &sc["download_types"][0]["formats"][0];
        assert_eq!(format["qualities"].as_array().unwrap().len(), 1);
        assert_eq!(format["qualities"][0]["label"], "Source");
        assert_eq!(format["flags"]["advisory"], true);
        assert!(
            format["notice"]
                .as_str()
                .unwrap()
                .contains("one source rendition")
        );

        // A URL nothing claims is answered with the merged catalog and `match: null`.
        let (status, merged) = rig
            .get(&format!(
                "api/v2/catalog?url={}",
                urlencoding("mailto:someone@example.test")
            ))
            .await;
        assert_eq!(status, 200, "{merged}");
        assert!(merged["match"].is_null());
    })
    .await;
}

#[tokio::test]
async fn every_catalog_option_is_a_renderable_control() {
    let rig = Rig::builder("/")
        .env(
            "YTDL_OPTIONS_PRESETS",
            r#"{"sponsorblock":{},"archive":{}}"#,
        )
        .start()
        .await;
    let (_, catalog) = rig
        .get(&format!("api/v2/catalog?url={}", urlencoding(YT)))
        .await;
    assert!(
        matches!(catalog["naming"].as_str(), Some("template" | "provider")),
        "naming is a closed two-value enum"
    );
    for dt in catalog["download_types"].as_array().unwrap() {
        for option in dt["options"].as_array().unwrap() {
            for key in ["id", "label", "kind", "default", "choices", "help"] {
                assert!(option.get(key).is_some(), "{key} on {option}");
            }
            let kind = option["kind"]["type"].as_str().unwrap();
            match kind {
                "int" => {
                    assert!(option["kind"]["min"].is_number());
                    assert!(option["kind"]["max"].is_number());
                }
                "text" => assert!(option["kind"].get("pattern").is_some()),
                "enum" => assert!(
                    !option["choices"].as_array().unwrap().is_empty(),
                    "enum is the only kind with choices: {option}"
                ),
                "bool" | "path" => {
                    assert!(option["choices"].as_array().unwrap().is_empty());
                }
                other => panic!("unknown option kind {other}"),
            }
        }
    }
    // The operator's presets reach the picker (docs/INTEGRATION-NOTES.md, WP-02).
    let presets = catalog["download_types"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|dt| dt["options"].as_array().unwrap())
        .find(|o| o["id"] == "ytdl_options_presets")
        .unwrap();
    let choices: Vec<&str> = presets["choices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(choices, ["archive", "sponsorblock"]);
}

#[tokio::test]
async fn the_catalog_payloads_are_stable() {
    let rig = Rig::start("/").await;
    let (_, mut ytdlp) = rig
        .get(&format!("api/v2/catalog?url={}", urlencoding(YT)))
        .await;
    normalise(&mut ytdlp);
    insta::assert_json_snapshot!("catalog_ytdlp", ytdlp);

    let (_, mut sc) = rig
        .get(&format!("api/v2/catalog?url={}", urlencoding(SC)))
        .await;
    normalise(&mut sc);
    insta::assert_json_snapshot!("catalog_streamingcommunity", sc);
}

// ---------------------------------------------------------------------------
// providers, presets, plugins, resolve-preview
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_provider_inventory_names_the_fallback() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (status, body) = rig.get("api/v2/providers").await;
        assert_eq!(status, 200, "{body}");
        let providers = body["providers"].as_array().unwrap();
        assert_eq!(providers.len(), 3);
        let ytdlp = providers.iter().find(|p| p["id"] == "ytdlp").unwrap();
        assert_eq!(ytdlp["state"], "ready");
        assert_eq!(ytdlp["fallback"], true);
        for key in ["reason", "version", "capabilities", "limits", "argv"] {
            assert!(ytdlp.get(key).is_some(), "{key}");
        }
        let sc = providers
            .iter()
            .find(|p| p["id"] == "streamingcommunity")
            .unwrap();
        assert_eq!(sc["fallback"], false);
    })
    .await;
}

/// A manifest that loads with a clamped limit is visible on both operator surfaces.
///
/// The fatal cases were already visible — a directory that produces nothing is
/// `ReloadReport.failed`, a matcher-only manifest is a `degraded` provider — but clamps and
/// auto-anchors were reported nowhere, which is the WP-14 request in
/// `docs/INTEGRATION-NOTES.md`.
#[tokio::test]
async fn a_clamped_plugin_manifest_warns_on_healthz_and_on_the_provider_inventory() {
    let rig = Rig::builder("/")
        .plugin(
            "loud",
            r#"
            manifest_version = 1
            name = "Loud"
            version = "1.0.0"

            [match]
            hosts = ["loud.test"]

            [limits]
            max_concurrent = 0

            [download]
            command = ["/bin/echo", "{url}"]
            "#,
        )
        .start()
        .await;

    let (status, body) = rig.get("api/v2/providers").await;
    assert_eq!(status, 200, "{body}");
    let warnings = body["warnings"].as_array().expect("an array");
    assert!(
        warnings.iter().any(|w| {
            let w = w.as_str().unwrap_or_default();
            w.contains("loud") && w.contains("limits.max_concurrent")
        }),
        "the clamp is reported: {body:?}"
    );
    assert!(
        body["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == "command:loud"),
        "and the plugin still loaded: {body}"
    );

    let (status, body) = rig.get("healthz").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["plugin_warnings"].as_array(),
        Some(warnings),
        "healthz reports the same list"
    );
}

/// With no plugin directory to scan, both surfaces carry an empty array rather than omitting it.
#[tokio::test]
async fn the_warning_lists_are_present_and_empty_with_no_plugins() {
    let rig = Rig::start("/").await;
    let (_, body) = rig.get("api/v2/providers").await;
    assert_eq!(body["warnings"], json!([]));
    let (_, body) = rig.get("healthz").await;
    assert_eq!(body["plugin_warnings"], json!([]));
}

#[tokio::test]
async fn presets_and_a_plugin_reload_answer_their_documented_shapes() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("YTDL_OPTIONS_PRESETS", r#"{"sponsorblock":{"quiet":true}}"#)
            .start()
            .await;
        let (status, body) = rig.get("api/v2/presets").await;
        assert_eq!(status, 200);
        assert_eq!(body, json!({ "presets": ["sponsorblock"] }));

        let (status, body) = rig.post("api/v2/plugins/reload", &json!({})).await;
        assert_eq!(status, 200, "{body}");
        for key in ["added", "updated", "removed", "failed", "warnings"] {
            assert!(body[key].is_array(), "{key} must be an array: {body}");
        }
    })
    .await;
}

#[tokio::test]
async fn resolve_preview_explains_the_choice() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (status, body) = rig
            .get(&format!("api/v2/resolve-preview?url={}", urlencoding(SC)))
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["provider"], "streamingcommunity");
        assert_eq!(body["score"], 200);
        assert_eq!(body["reason"], "host_contains");
        assert_eq!(body["runner_up"]["provider"], "ytdlp");

        let (status, body) = rig.get("api/v2/resolve-preview").await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["field"], "url");
    })
    .await;
}

// ---------------------------------------------------------------------------
// custom dirs, import report, options
// ---------------------------------------------------------------------------

#[tokio::test]
async fn custom_dirs_lists_the_tree_and_is_404_when_disabled() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("CUSTOM_DIRS_EXCLUDE_REGEX", "^private")
            .start()
            .await;
        std::fs::create_dir_all(rig.download_dir().join("Music/Live")).unwrap();
        std::fs::create_dir_all(rig.download_dir().join("private")).unwrap();

        let (status, body) = rig.get("api/v2/custom-dirs").await;
        assert_eq!(status, 200, "{body}");
        let dirs: Vec<&str> = body["download_dir"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d.as_str().unwrap())
            .collect();
        assert!(
            dirs.contains(&""),
            "the base dir is offered as an empty string"
        );
        assert!(dirs.contains(&"Music"));
        assert!(dirs.contains(&"Music/Live"));
        assert!(
            !dirs.contains(&"private"),
            "CUSTOM_DIRS_EXCLUDE_REGEX: {dirs:?}"
        );
        assert!(body["audio_download_dir"].is_array());

        let off = Rig::builder(prefix)
            .env("CUSTOM_DIRS", "false")
            .start()
            .await;
        let (status, body) = off.get("api/v2/custom-dirs").await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "not_found");
    })
    .await;
}

#[tokio::test]
async fn the_import_report_is_404_when_nothing_was_imported() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (status, body) = rig.get("api/v2/import-report").await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "not_found");
    })
    .await;
}

#[tokio::test]
async fn ytdl_options_reports_its_keys_and_reloads_on_demand() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env(
                "YTDL_OPTIONS",
                r#"{"quiet":true,"cookiefile":"/secret/cookies.txt"}"#,
            )
            .env(
                "YTDL_OPTIONS_PRESETS",
                r#"{"archive":{"download_archive":"a.txt"}}"#,
            )
            .start()
            .await;
        let (status, body) = rig.get("api/v2/ytdl-options").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["msg"], "");
        let keys: Vec<&str> = body["keys"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap())
            .collect();
        assert_eq!(keys, ["cookiefile", "quiet"], "keys only, never values");
        assert_eq!(body["presets"], json!(["archive"]));

        let (status, body) = rig.post("api/v2/ytdl-options/reload", &json!({})).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["ok"], true);
        assert!(body.get("update_time").is_some());
    })
    .await;
}

#[tokio::test]
async fn a_broken_options_file_reloads_as_not_ok_and_keeps_the_last_good_set() {
    let rig_dir = tempfile::tempdir().unwrap();
    let path = rig_dir.path().join("ytdl.json");
    std::fs::write(&path, r#"{"quiet":true}"#).unwrap();
    let rig = Rig::builder("/")
        .env("YTDL_OPTIONS_FILE", path.to_str().unwrap())
        .start()
        .await;
    let (_, body) = rig.get("api/v2/ytdl-options").await;
    assert_eq!(body["ok"], true);

    std::fs::write(&path, "not json").unwrap();
    let (status, body) = rig.post("api/v2/ytdl-options/reload", &json!({})).await;
    assert_eq!(status, 200, "a broken file is reported, not a 500: {body}");
    assert_eq!(body["ok"], false);
    assert!(!body["msg"].as_str().unwrap().is_empty());

    let (_, after) = rig.get("api/v2/ytdl-options").await;
    assert_eq!(after["ok"], false, "the snapshot tells a fresh client too");
    assert_eq!(
        after["keys"],
        json!(["quiet"]),
        "the last good options are still in force"
    );

    let (_, state) = rig.get("api/v2/state").await;
    assert_eq!(state["ytdl_options"]["ok"], false);
    assert!(!state["ytdl_options"]["msg"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn debug_options_labels_every_key_with_its_layer() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("YTDL_OPTIONS", r#"{"quiet":true,"proxy":"http://p"}"#)
            .env(
                "YTDL_OPTIONS_PRESETS",
                r#"{"archive":{"download_archive":"a.txt"}}"#,
            )
            .env("ALLOW_YTDL_OPTIONS_OVERRIDES", "true")
            .start()
            .await;
        let (status, body) = rig.get("api/v2/debug/options").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["options"]["quiet"]["source"], "env");
        assert_eq!(body["options"]["quiet"]["value"], true);
        assert_eq!(
            body["options"]["proxy"]["value"], "«redacted»",
            "a secret-looking key is never echoed"
        );

        let (_, added) = rig
            .post(
                "api/v2/downloads",
                &json!({
                    "url": YT,
                    "auto_start": false,
                    "ytdl_options_presets": ["archive"],
                    "ytdl_options_overrides": { "ratelimit": 1024 },
                }),
            )
            .await;
        let id = added["ids"][0].as_str().unwrap().to_owned();
        rig.until_status(&id, "queued").await;

        let (status, body) = rig.get(&format!("api/v2/debug/options?item_id={id}")).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["item_id"], id.as_str());
        assert_eq!(
            body["options"]["download_archive"]["source"],
            "preset:archive"
        );
        assert_eq!(body["options"]["ratelimit"]["source"], "request");
        assert_eq!(body["options"]["ratelimit"]["value"], 1024);

        let (status, body) = rig
            .get("api/v2/debug/options?item_id=01JBQ7Z5T9K3M2R8V4XW6Y0AAA")
            .await;
        assert_eq!(status, 404, "{body}");
    })
    .await;
}

// ---------------------------------------------------------------------------
// cookies
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_cookie_routes_keep_the_legacy_cap_and_messages() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (status, body) = rig.get("api/v2/cookies").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["has_cookies"], false);
        assert!(body["bytes"].is_null());

        // The wrong field name is the legacy message.
        let form = reqwest::multipart::Form::new().text("nope", "x");
        let response = rig
            .http
            .post(rig.url("api/v2/cookies"))
            .multipart(form)
            .send()
            .await
            .unwrap();
        let (status, body) = support::status_and_body(response).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["message"], "No cookies file provided");

        // Over the cap: 1 000 000 bytes, decimal (DESIGN §16.6).
        let part = reqwest::multipart::Part::bytes(vec![b'#'; 1_020_000]).file_name("cookies.txt");
        let form = reqwest::multipart::Form::new().part("cookies", part);
        let response = rig
            .http
            .post(rig.url("api/v2/cookies"))
            .multipart(form)
            .send()
            .await
            .unwrap();
        let (status, body) = support::status_and_body(response).await;
        assert_eq!(status, 413, "{body}");
        assert_eq!(body["error"]["code"], "payload_too_large");
        assert_eq!(body["error"]["message"], "Cookie file too large (max 1MB)");

        // A real upload installs the `cookiefile` runtime override.
        let part = reqwest::multipart::Part::bytes(b"# Netscape HTTP Cookie File\n".to_vec())
            .file_name("cookies.txt");
        let form = reqwest::multipart::Form::new().part("cookies", part);
        let response = rig
            .http
            .post(rig.url("api/v2/cookies"))
            .multipart(form)
            .send()
            .await
            .unwrap();
        let (status, body) = support::status_and_body(response).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["has_cookies"], true);
        assert_eq!(body["bytes"], 28);

        let (_, status_body) = rig.get("api/v2/cookies").await;
        assert_eq!(status_body["has_cookies"], true);
        assert!(status_body["updated_at"].as_i64().is_some());
        let (_, options) = rig.get("api/v2/debug/options").await;
        assert_eq!(options["options"]["cookiefile"]["source"], "aulos");

        let response = rig
            .http
            .delete(rig.url("api/v2/cookies"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 204);
        let (status, body) = rig.delete("api/v2/cookies").await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["message"], "No uploaded cookies to delete");
    })
    .await;
}

// ---------------------------------------------------------------------------
// subscriptions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_subscription_routes_are_the_documented_lifecycle() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        let (status, body) = rig.get("api/v2/subscriptions").await;
        assert_eq!(status, 200);
        assert_eq!(body, json!({ "subscriptions": [] }));

        let (status, created) = rig
            .post(
                "api/v2/subscriptions",
                &json!({
                    "url": "https://www.youtube.com/@veritasium",
                    "format": "any",
                    "check_interval_minutes": 30,
                }),
            )
            .await;
        assert_eq!(status, 201, "{created}");
        for key in [
            "id",
            "name",
            "url",
            "enabled",
            "check_interval_minutes",
            "download_type",
            "codec",
            "format",
            "quality",
            "folder",
            "last_checked",
            "seen_count",
            "error",
            "next_due",
            "consecutive_failures",
            "checking",
        ] {
            assert!(created.get(key).is_some(), "{key} on {created}");
        }
        assert_eq!(
            created["check_interval_minutes"], 30,
            "the body's interval wins"
        );
        assert_eq!(created["format"], "any");
        let id = created["id"].as_str().unwrap().to_owned();

        // A single-video URL and a duplicate keep the legacy strings.
        let (status, body) = rig
            .post("api/v2/subscriptions", &json!({ "url": YT }))
            .await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(
            body["error"]["message"],
            "This URL points to a single video, not a channel or playlist. Use Download instead."
        );
        let (status, body) = rig
            .post(
                "api/v2/subscriptions",
                &json!({ "url": "https://www.youtube.com/@veritasium" }),
            )
            .await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["code"], "conflict");

        // PATCH, and a bad `enabled` is a 400 rather than a leaked 500.
        let (status, patched) = rig
            .patch(
                &format!("api/v2/subscriptions/{id}"),
                &json!({ "enabled": false, "name": "V" }),
            )
            .await;
        assert_eq!(status, 200, "{patched}");
        assert_eq!(patched["enabled"], false);
        assert_eq!(patched["name"], "V");
        let (status, body) = rig
            .patch(
                &format!("api/v2/subscriptions/{id}"),
                &json!({ "enabled": "maybe" }),
            )
            .await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(body["error"]["message"], "enabled must be a boolean");

        // Checks answer immediately with a job handle.
        let (status, body) = rig
            .post(&format!("api/v2/subscriptions/{id}/check"), &json!({}))
            .await;
        assert_eq!(status, 202, "{body}");
        assert_eq!(body["count"], 1);
        assert!(body["job_id"].as_str().is_some());
        let (status, body) = rig.post("api/v2/subscriptions/check", &json!({})).await;
        assert_eq!(status, 202, "{body}");
        assert_eq!(body["count"], 1, "an empty body checks every subscription");

        // DELETE is 204, then 404.
        let response = rig
            .http
            .delete(rig.url(&format!("api/v2/subscriptions/{id}")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 204);
        let (status, body) = rig.delete(&format!("api/v2/subscriptions/{id}")).await;
        assert_eq!(status, 404, "{body}");
    })
    .await;
}

#[tokio::test]
async fn subscriptions_are_in_the_snapshot() {
    let rig = Rig::start("/").await;
    rig.post(
        "api/v2/subscriptions",
        &json!({ "url": "https://www.youtube.com/@veritasium" }),
    )
    .await;
    let (_, state) = rig.get("api/v2/state").await;
    let subs = state["subscriptions"].as_array().unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0]["url"], "https://www.youtube.com/@veritasium");
}

// ---------------------------------------------------------------------------
// the error taxonomy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_http_error_code_answers_with_the_same_envelope() {
    let rig = Rig::builder("/")
        .env("AULOS_MAX_BATCH_URLS", "1")
        .start()
        .await;

    // One case per `ErrorCode` that has an HTTP status and is reachable from a request.
    // `internal` and `state_unavailable` are deliberately absent: they are a bug and a busy
    // database, neither of which a request can provoke on a healthy server.
    let cases: [(&str, u16, &str); 8] = [
        ("bad_request", 400, "PATCH api/v2/subscriptions/x"),
        ("validation_failed", 400, "POST api/v2/downloads {}"),
        ("unsupported_url", 400, "POST api/v2/downloads magnet"),
        ("overrides_disabled", 400, "POST api/v2/downloads overrides"),
        ("unknown_preset", 400, "POST api/v2/downloads preset"),
        ("folder_invalid", 400, "POST api/v2/downloads folder"),
        ("not_found", 404, "GET api/v2/items/unknown"),
        ("payload_too_large", 413, "POST api/v2/downloads batch"),
    ];
    for (code, status, what) in cases {
        let body = match code {
            "bad_request" => {
                rig.patch(
                    "api/v2/subscriptions/01JBQ7Z5T9K3M2R8V4XW6Y0AAA",
                    &json!({}),
                )
                .await
            }
            "validation_failed" => rig.post("api/v2/downloads", &json!({})).await,
            "unsupported_url" => {
                rig.post("api/v2/downloads", &json!({ "url": "magnet:?xt=x" }))
                    .await
            }
            "overrides_disabled" => {
                rig.post(
                    "api/v2/downloads",
                    &json!({ "url": YT, "ytdl_options_overrides": { "quiet": true } }),
                )
                .await
            }
            "unknown_preset" => {
                rig.post(
                    "api/v2/downloads",
                    &json!({ "url": YT, "ytdl_options_presets": ["nope"] }),
                )
                .await
            }
            "folder_invalid" => {
                rig.post("api/v2/downloads", &json!({ "url": YT, "folder": "../x" }))
                    .await
            }
            "not_found" => rig.get("api/v2/items/01JBQ7Z5T9K3M2R8V4XW6Y0AAA").await,
            "payload_too_large" => {
                rig.post(
                    "api/v2/downloads",
                    &json!({ "items": [{ "url": YT }, { "url": "https://fake.test/b" }] }),
                )
                .await
            }
            other => panic!("unhandled code {other}"),
        };
        let (got, answer) = body;
        assert_eq!(got, status, "{what} answered {answer}");
        assert_eq!(answer["error"]["code"], code, "{what}");
        let error = answer["error"].as_object().unwrap();
        assert_eq!(error.len(), 6, "exactly six keys: {error:?}");
        for key in [
            "code",
            "message",
            "field",
            "provider",
            "provider_code",
            "request_id",
        ] {
            assert!(error.contains_key(key), "{key} missing for {code}");
        }
    }

    // A malformed query string and a body that is not multipart are envelopes too — those are
    // the two extractor rejections that would otherwise answer in plain text.
    let (status, body) = rig.get("api/v2/state?since=soon").await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "bad_request");
    let response = rig
        .http
        .post(rig.url("api/v2/cookies"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    let (status, body) = support::status_and_body(response).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["code"], "bad_request");

    // `socketio_removed` is the only 501 the server emits.
    let (status, body) = rig.get("socket.io/?EIO=4&transport=polling").await;
    assert_eq!(status, 501, "{body}");
    assert_eq!(body["error"]["code"], "socketio_removed");
    assert!(body["error"]["message"].as_str().unwrap().contains("ws"));
}

#[tokio::test]
async fn the_error_envelope_is_stable() {
    let rig = Rig::start("/").await;
    let (_, mut body) = rig
        .post("api/v2/downloads", &json!({ "url": YT, "quality": "1081" }))
        .await;
    normalise(&mut body);
    insta::assert_json_snapshot!("error_envelope", body);
}

#[tokio::test]
async fn every_response_carries_the_two_documented_headers() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        for path in [
            "",
            "version",
            "healthz",
            "livez",
            "api/v2/state",
            "api/v2/capabilities",
        ] {
            let response = rig.get_raw(path).await;
            let headers = response.headers();
            assert!(
                headers.contains_key("x-request-id"),
                "{path} has no X-Request-Id"
            );
            assert!(
                headers.contains_key("x-aulos-seq"),
                "{path} has no X-Aulos-Seq"
            );
            assert_eq!(
                headers.get("content-type").unwrap(),
                "application/json; charset=utf-8",
                "{path}"
            );
        }

        // A client-supplied request id is echoed, and a nonsense one is replaced.
        let response = rig
            .http
            .get(rig.url("livez"))
            .header("x-request-id", "01JBQ7Z5T9K3M2R8V4XW6Y0AAA")
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.headers().get("x-request-id").unwrap(),
            "01JBQ7Z5T9K3M2R8V4XW6Y0AAA"
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// health and the small routes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn healthz_and_livez_answer_without_auth() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::builder(prefix)
            .env("AULOS_API_TOKEN", "s3cret")
            .start()
            .await;
        let (status, body) = rig.get("livez").await;
        assert_eq!(status, 200, "the container healthcheck holds no token");
        assert_eq!(body, json!({ "ok": true }));

        let (status, body) = rig.get("healthz").await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["status"], "ok");
        assert_eq!(body["url_prefix"], prefix);
        assert_eq!(body["probe"], "shallow");
        for key in [
            "version",
            "yt_dlp",
            "boot_id",
            "uptime_s",
            "v1_shim",
            "seq",
            "components",
            "providers",
            "ws",
        ] {
            assert!(body.get(key).is_some(), "{key} on healthz");
        }
        assert_eq!(body["uptime_s"], 43_201, "from the rig's fixed start time");
        assert!(body["components"]["store"]["wal_bytes"].as_u64().is_some());
        assert_eq!(body["ws"]["clients"], 0);

        let (status, deep) = rig.get("healthz?probe=deep").await;
        assert_eq!(status, 200, "{deep}");
        assert_eq!(deep["probe"], "deep");
        let (_, throttled) = rig.get("healthz?probe=deep").await;
        assert_eq!(throttled["probe"], "throttled", "one live probe per 10 s");
    })
    .await;
}

#[tokio::test]
async fn a_degraded_component_is_visible_to_a_fresh_client() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;
        rig.state.health.set(
            "pot",
            ComponentHealth::new(ComponentStatus::Down)
                .with("detail", "3 consecutive probe failures")
                .with("restarts", 3),
        );

        // No `health` frame has been sent — the frame is transition-only — and yet a client that
        // connects now learns it from the snapshot (PROTOCOL §5.3).
        let (_, state) = rig.get("api/v2/state").await;
        assert_eq!(state["health"]["status"], "down");
        assert_eq!(state["health"]["components"]["pot"], "down");

        let (status, health) = rig.get("healthz").await;
        assert_eq!(status, 200, "only an unusable store is a 503: {health}");
        assert_eq!(health["status"], "down");
        assert_eq!(health["components"]["pot"]["restarts"], 3);
    })
    .await;
}

/// The DESIGN §16.3 stock component set, snapshot-tested so the payload and the document cannot
/// drift apart.
#[tokio::test]
async fn the_healthz_payload_is_stable_for_the_stock_component_set() {
    let rig = Rig::start("/").await;
    let stock: [(&str, ComponentHealth); 15] = [
        (
            "store",
            ComponentHealth::new(ComponentStatus::Ok)
                .with("latency_ms", 0.42)
                .with("wal_bytes", 1_048_576)
                .with("db_bytes", 41_943_040),
        ),
        (
            "queue",
            ComponentHealth::new(ComponentStatus::Ok)
                .with("downloading", 2)
                .with("postprocessing", 0)
                .with("queued", 5)
                .with("resolving", 1)
                .with("progress_dropped_total", 0),
        ),
        (
            "pot",
            ComponentHealth::new(ComponentStatus::Down)
                .with("restarts", 3)
                .with("endpoint", "http://127.0.0.1:4416")
                .with("detail", "3 consecutive probe failures"),
        ),
        (
            "ytdlp_runner",
            ComponentHealth::new(ComponentStatus::Ok)
                .with("python", "3.13.5")
                .with("yt_dlp", "2026.8.30.232658.dev0"),
        ),
        (
            "ffmpeg",
            ComponentHealth::new(ComponentStatus::Ok).with("version", "6.1.1"),
        ),
        (
            "nm3u8dl",
            ComponentHealth::new(ComponentStatus::Ok).with("version", "v0.5.1-beta"),
        ),
        (
            "deno",
            ComponentHealth::new(ComponentStatus::Ok).with("version", "2.x"),
        ),
        (
            "ytdl_options",
            ComponentHealth::new(ComponentStatus::Ok).with("presets", 2),
        ),
        (
            "telegram",
            ComponentHealth::new(ComponentStatus::Ok)
                .with("chats", 2)
                .with("edits_throttled_total", 11),
        ),
        (
            "jellyfin",
            ComponentHealth::new(ComponentStatus::Ok)
                .with("pending", false)
                .with("runs_total", 18)
                .with("failures_total", 0),
        ),
        (
            "nfo",
            ComponentHealth::new(ComponentStatus::Ok)
                .with("runs_total", 7)
                .with("failures_total", 0),
        ),
        (
            "audio_sync",
            ComponentHealth::new(ComponentStatus::Ok)
                .with("runs_total", 2)
                .with("phase", "pre_terminal"),
        ),
        (
            "events",
            ComponentHealth::new(ComponentStatus::Ok)
                .with("dropped", json!({ "hooks": 0, "telegram": 0 })),
        ),
        (
            "subscriptions",
            ComponentHealth::new(ComponentStatus::Ok)
                .with("total", 7)
                .with("failing", 1)
                .with("next_due_in_s", 412),
        ),
        (
            "importer",
            ComponentHealth::new(ComponentStatus::Ok).with("warnings", 2),
        ),
    ];
    for (name, component) in stock {
        rig.state.health.set(name, component);
    }
    let (status, mut body) = rig.get("healthz").await;
    assert_eq!(status, 200);
    normalise(&mut body);
    insta::assert_json_snapshot!("healthz_stock", body);
}

#[tokio::test]
async fn the_small_top_level_routes_answer() {
    for_each_prefix(|prefix| async move {
        let rig = Rig::start(prefix).await;

        // `GET <p>` is a tiny JSON identity document: no HTML, no `metube_theme` cookie
        // (DESIGN §11.1).
        let response = rig.get_raw("").await;
        assert_eq!(response.status().as_u16(), 200);
        assert!(
            !response.headers().contains_key("set-cookie"),
            "no `metube_theme` cookie, and no cookie at all"
        );
        let (_, body) = support::status_and_body(response).await;
        assert_eq!(body["name"], "aulos-server");
        assert_eq!(body["url_prefix"], prefix);
        assert_eq!(body["protocol"], "v2");

        let (status, body) = rig.get("version").await;
        assert_eq!(status, 200);
        assert_eq!(body["version"], "2026.09.04");
        assert_eq!(body["yt-dlp"], "2026.8.30.232658.dev0");
        assert_eq!(body["protocol"], "v2");
        assert_eq!(body["url_prefix"], prefix);

        let response = rig.get_raw("robots.txt").await;
        assert_eq!(response.status().as_u16(), 200);
        assert!(
            response
                .headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/plain")
        );
        assert!(response.text().await.unwrap().contains("Disallow: /"));

        // v1.0: the Prometheus endpoint is CUT, and says so with the envelope.
        let (status, body) = rig.get("metrics").await;
        assert_eq!(status, 404, "{body}");
        assert_eq!(body["error"]["code"], "not_found");
    })
    .await;
}

/// Percent-encodes a URL for a query parameter.
fn urlencoding(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            other => {
                let mut buf = [0u8; 4];
                other
                    .encode_utf8(&mut buf)
                    .bytes()
                    .map(|b| format!("%{b:02X}"))
                    .collect()
            }
        })
        .collect()
}
