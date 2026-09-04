//! The replay machinery for WP-00's `tests/v1_golden/` corpus (PLAN WP-15).
//!
//! The corpus is 131 directories of `{request,response,meta}.json` captured from the **Python
//! server being replaced**, at `<workspace>/tests/v1_golden/`. After cutover that server is gone,
//! so this is the only evidence of what the v1 wire actually looked like — which is why the
//! harness asserts it covered **every** directory (see `every_corpus_case_is_replayed`): a route
//! the capture recorded can never be silently skipped.
//!
//! # Where the legacy strings live
//!
//! Not in `response.json`. Legacy raised `web.HTTPBadRequest(reason='<message>')` and aiohttp put
//! that in the **status line**, serving a `text/plain` body of `400: <message>`. So every
//! byte-identical string of DESIGN §11.7 is `meta.json:reason`, and the shim's job is to carry it
//! in `error.message` of the §1.5 envelope. [`Mode::Strict`] compares exactly that.
//!
//! # The two comparison modes
//!
//! | Mode | What is asserted |
//! |---|---|
//! | [`Mode::Strict`] | the status, and every key of the captured body — a key the shim adds must be in [`ADDITIVE_KEYS`], and a legacy 400's `error.message` must equal `meta.reason` byte for byte |
//! | [`Mode::Shape`] | the status (after the documented promotions) plus a structural assertion, with a written reason for why the values cannot match |
//!
//! Only twenty of the 131 cases are [`Mode::Shape`], and every one of them is there for one of
//! three reasons, each recorded in [`shape_reason`]: the capture ran against a seeded legacy
//! `STATE_DIR` this process cannot reproduce (its ids are legacy UUIDs and its `last_checked` a
//! fixed future timestamp), legacy answered a **reasonless** 400 so there is no string to
//! preserve, or the shim deliberately promotes a leaked 500 to a real 400.
//!
//! # Ordering
//!
//! The corpus was captured against one long-lived server, so a handful of cases are sequential:
//! `cookie_status_with_cookies` is only true *after* `upload_cookies_ok`, and
//! `subscribe_duplicate_url` is only true after something subscribed to that URL. [`ordered`]
//! reproduces those two sequences explicitly and leaves everything else in sorted order, so a
//! failure names a case rather than a race.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use reqwest::Method;
use serde_json::{Map, Value};

use crate::support::Rig;

/// One case, as it sits on disk.
pub struct Case {
    /// The directory name, which is also the case id in `MANIFEST.json`.
    pub name: String,
    /// `request.json`.
    pub request: Value,
    /// `response.json` — the parsed body, or `{"__text__": …}` when legacy served text.
    pub response: Value,
    /// `meta.json` — where `status`, `reason` and `content_type` live.
    pub meta: Value,
}

impl Case {
    /// The HTTP method.
    pub fn method(&self) -> Method {
        match self.request["method"].as_str().unwrap_or("GET") {
            "POST" => Method::POST,
            "OPTIONS" => Method::OPTIONS,
            _ => Method::GET,
        }
    }

    /// The path, always prefix-relative in the corpus (`URL_PREFIX` was `""`).
    pub fn suffix(&self) -> &str {
        self.request["path"]
            .as_str()
            .unwrap_or("/")
            .trim_start_matches('/')
    }

    /// The captured HTTP status.
    pub fn status(&self) -> u16 {
        u16::try_from(self.meta["status"].as_u64().unwrap_or(0)).unwrap_or(0)
    }

    /// The aiohttp reason phrase — the legacy validation string, when there is one.
    pub fn reason(&self) -> &str {
        self.meta["reason"].as_str().unwrap_or_default()
    }

    /// The `Origin` header the capture sent, if any.
    pub fn origin(&self) -> Option<&str> {
        self.request["headers"]["Origin"].as_str()
    }

    /// The request body, in whichever of the four captured forms it took.
    pub fn body(&self) -> Body {
        let body = &self.request["body"];
        if body.is_null() {
            return Body::None;
        }
        if let Some(raw) = body.get("__raw__").and_then(Value::as_str) {
            return Body::Raw(raw.to_owned());
        }
        if let Some(part) = body.get("__multipart__") {
            return Body::Multipart {
                field: part["field_name"].as_str().map(str::to_owned),
                filename: part["filename"]
                    .as_str()
                    .unwrap_or("cookies.txt")
                    .to_owned(),
                bytes: usize::try_from(part["content_bytes"].as_u64().unwrap_or(0)).unwrap_or(0),
                has_parts: part["has_parts"].as_bool().unwrap_or(false),
            };
        }
        Body::Json(body.clone())
    }
}

/// The four shapes a captured request body takes.
pub enum Body {
    /// No body at all (`GET`, `OPTIONS`).
    None,
    /// A JSON object, sent as-is.
    Json(Value),
    /// Bytes that are deliberately not valid JSON, or not an object.
    Raw(String),
    /// A synthesised `multipart/form-data` upload. The capture records the byte count and the
    /// recipe (`b"a" * N`) rather than a megabyte of payload.
    Multipart {
        /// The part name, or `None` when there is no part.
        field: Option<String>,
        /// The part's file name.
        filename: String,
        /// How many `b'a'` bytes the part carries.
        bytes: usize,
        /// Whether the body has a part at all.
        has_parts: bool,
    },
}

/// How closely a case's response is compared.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Status and body, key by key.
    Strict,
    /// Status plus a structural assertion, because the values cannot match. The `&'static str` is
    /// the reason, which [`shape_reason`] supplies.
    Shape(&'static str),
}

/// Keys the shim adds to a legacy body. Every one is documented in DESIGN §11.1 / PROTOCOL §10.1
/// as additive, and the shipped clients ignore unknown keys.
pub const ADDITIVE_KEYS: [&str; 4] = ["ids", "job_id", "url_prefix", "protocol"];

/// The statuses the shim deliberately answers where legacy answered something else
/// (DESIGN §11.1's deviation column).
pub fn promoted_status(name: &str) -> Option<u16> {
    // Three cases, one status. `start_pending(None)` raised a `TypeError`, and `_coerce_bool`
    // raised `ValueError` with `enabled must be a boolean` never leaving the log (Δ C25); all
    // three are a real 400 here.
    const PROMOTED_TO_400: [&str; 3] = [
        "start_ids_null",
        "start_missing_ids",
        "subscriptions_update_enabled_not_a_boolean",
    ];
    PROMOTED_TO_400.contains(&name).then_some(400)
}

/// Why a case cannot be compared strictly, or `None` when it can.
///
/// Three families, and nothing else is allowed to be here — the list is asserted exhaustive by
/// `every_shape_case_has_a_reason`.
pub fn shape_reason(name: &str) -> Option<&'static str> {
    const SEEDED: &str = "the capture ran against a seeded legacy STATE_DIR whose rows carry \
                          legacy UUIDs and a fixed future last_checked; this process cannot mint \
                          those, so the structure is asserted instead of the values";
    const REASONLESS: &str = "legacy raised a reasonless HTTPBadRequest, so aiohttp's own \
                              'Bad Request' is all that was captured and there is no legacy \
                              string to preserve";
    const PROMOTED: &str = "legacy leaked a 500 with nothing on the wire; the shim answers a 400 \
                            carrying the string that never left the log (DESIGN §11.1)";
    const VOLATILE: &str = "the value is this build's own, not the capture's";

    match name {
        "history_seeded"
        | "history_after_mutations"
        | "subscriptions_list"
        | "subscriptions_list_after_mutations"
        | "subscriptions_update_rename"
        | "subscriptions_update_blank_name_is_ignored"
        | "subscriptions_update_check_interval_is_floored_at_1"
        | "subscriptions_update_enabled_string_is_accepted" => Some(SEEDED),

        "delete_bad_where"
        | "delete_empty_ids"
        | "delete_ids_null"
        | "delete_missing_ids"
        | "delete_missing_where" => Some(REASONLESS),

        "start_ids_null" | "start_missing_ids" | "subscriptions_update_enabled_not_a_boolean" => {
            Some(PROMOTED)
        }

        "version" | "version_with_allowed_origin" | "version_with_disallowed_origin" => {
            Some(VOLATILE)
        }

        _ => None,
    }
}

/// [`Mode`] for a case.
pub fn mode(name: &str) -> Mode {
    shape_reason(name).map_or(Mode::Strict, Mode::Shape)
}

/// `<workspace>/tests/v1_golden`.
///
/// The corpus is WP-00's, checked in at the repository root and read in place: copying 131
/// directories into this crate's `tests/fixtures` would give two sources of truth for the one
/// thing that cannot be re-captured.
pub fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("v1_golden")
}

/// `MANIFEST.json`.
pub fn manifest() -> Value {
    let path = corpus_dir().join("MANIFEST.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("the corpus manifest {} is unreadable: {e}", path.display()));
    serde_json::from_str(&text).expect("MANIFEST.json must be valid JSON")
}

/// Every case on disk, by name.
pub fn load_all() -> BTreeMap<String, Case> {
    let dir = corpus_dir();
    let mut out = BTreeMap::new();
    let entries = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("the corpus {} is unreadable: {e}", dir.display()));
    for entry in entries {
        let entry = entry.expect("a readable directory entry");
        if !entry.file_type().expect("a file type").is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let read = |file: &str| -> Value {
            let path = entry.path().join(file);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} is unreadable: {e}", path.display()));
            serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()))
        };
        out.insert(
            name.clone(),
            Case {
                name,
                request: read("request.json"),
                response: read("response.json"),
                meta: read("meta.json"),
            },
        );
    }
    assert!(!out.is_empty(), "the corpus must not be empty");
    out
}

/// The cookie sequence, in capture order: legacy served these against one long-lived server, so
/// `cookie_status_with_cookies` is only true after an upload landed.
pub const COOKIE_SEQUENCE: [&str; 9] = [
    "cookie_status_without_cookies",
    "upload_cookies_no_parts",
    "upload_cookies_wrong_field_name",
    "upload_cookies_over_the_cap",
    "upload_cookies_ok",
    "cookie_status_with_cookies",
    "upload_cookies_at_the_cap",
    "delete_cookies_ok",
    "cookie_status_after_delete",
];

/// The subscription cases that need a subscription to exist first.
pub const SUBSCRIPTION_SEQUENCE: [&str; 3] = [
    "subscribe_duplicate_url",
    "subscribe_duplicate_url_with_surrounding_space",
    "subscribe_check_interval_null_uses_default",
];

/// The URL the capture's seeded subscription used, and therefore the one the three duplicate cases
/// send.
pub const SEEDED_FEED_URL: &str = "https://www.youtube.com/@BlenderOfficial/videos";

/// Case names in a replay order that reproduces the capture's two sequences.
///
/// Everything not in a sequence is state-independent and runs in sorted order, so a failure is
/// reproducible on its own.
pub fn ordered(cases: &BTreeMap<String, Case>) -> Vec<String> {
    let sequenced: BTreeSet<&str> = COOKIE_SEQUENCE
        .into_iter()
        .chain(SUBSCRIPTION_SEQUENCE)
        .chain(["delete_cookies_nothing_to_delete"])
        .collect();
    let mut out: Vec<String> = cases
        .keys()
        .filter(|name| !sequenced.contains(name.as_str()))
        .cloned()
        .collect();
    for name in COOKIE_SEQUENCE {
        out.push(name.to_owned());
    }
    // Only true once the upload has been deleted again, which `COOKIE_SEQUENCE` just did.
    out.push("delete_cookies_nothing_to_delete".to_owned());
    for name in SUBSCRIPTION_SEQUENCE {
        out.push(name.to_owned());
    }
    out
}

/// A rig configured like the capture's server (`MANIFEST.json:server_env`).
///
/// The four settings that change an answer are `ALLOW_YTDL_OPTIONS_OVERRIDES=false` (the
/// `ytdl_options_overrides are disabled` 400), `YTDL_OPTIONS_PRESETS` (the `presets` body and the
/// unknown-preset 400), `CORS_ALLOWED_ORIGINS` (the reflected `Origin`) and
/// `DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT=0`.
pub async fn rig() -> Rig {
    Rig::builder("/")
        .env("METUBE_VERSION", "wp00-capture")
        .env("ALLOW_YTDL_OPTIONS_OVERRIDES", "false")
        .env("CORS_ALLOWED_ORIGINS", "https://ui.example.com")
        .env("CUSTOM_DIRS", "true")
        .env("CREATE_CUSTOM_DIRS", "true")
        .env("DEFAULT_OPTION_PLAYLIST_ITEM_LIMIT", "0")
        .env("DELETE_FILE_ON_TRASHCAN", "false")
        .env("SUBSCRIPTION_DEFAULT_CHECK_INTERVAL", "60")
        .env(
            "YTDL_OPTIONS_PRESETS",
            r#"{"archive": {"writesubtitles": true}, "fast": {"concurrent_fragment_downloads": 4}}"#,
        )
        // Every captured `POST add` is a validation 400 (WP-00's verifier asserts it), so the
        // pre-resolve window is never entered. Zero keeps the replay free of any timing.
        .env("AULOS_V1_ADD_RESOLVE_WAIT_MS", "0")
        .start()
        .await
}

/// What the shim actually answered.
pub struct Reply {
    /// The HTTP status.
    pub status: u16,
    /// The body as text.
    pub text: String,
    /// The body parsed, when it is JSON.
    pub json: Option<Value>,
    /// `Access-Control-Allow-Origin`, when present.
    pub allow_origin: Option<String>,
    /// `Access-Control-Allow-Headers`, when present.
    pub allow_headers: Option<String>,
    /// `Content-Type`.
    pub content_type: Option<String>,
}

/// Replays one case against `rig`.
pub async fn replay(rig: &Rig, case: &Case) -> Reply {
    let url = rig.url(case.suffix());
    let mut req = rig.http.request(case.method(), url);
    if let Some(origin) = case.origin() {
        req = req.header("Origin", origin);
    }
    req = match case.body() {
        Body::None => req,
        Body::Json(value) => req.header("Content-Type", "application/json").json(&value),
        Body::Raw(raw) => req.header("Content-Type", "application/json").body(raw),
        Body::Multipart {
            field,
            filename,
            bytes,
            has_parts,
        } => {
            let mut form = reqwest::multipart::Form::new();
            if has_parts {
                let part = reqwest::multipart::Part::bytes(vec![b'a'; bytes])
                    .file_name(filename)
                    .mime_str("text/plain")
                    .expect("a literal mime type");
                form = form.part(field.unwrap_or_else(|| "cookies".to_owned()), part);
            }
            req.multipart(form)
        }
    };

    let response = req.send().await.expect("the shim must answer");
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let allow_origin = header("access-control-allow-origin");
    let allow_headers = header("access-control-allow-headers");
    let content_type = header("content-type");
    let text = response.text().await.unwrap_or_default();
    let json = serde_json::from_str::<Value>(&text).ok();
    Reply {
        status,
        text,
        json,
        allow_origin,
        allow_headers,
        content_type,
    }
}

/// Asserts one case, in its [`Mode`].
pub fn compare(case: &Case, reply: &Reply) {
    let expected_status = promoted_status(&case.name).unwrap_or_else(|| case.status());
    assert_eq!(
        reply.status,
        expected_status,
        "{}: status (captured {}, body {})",
        case.name,
        case.status(),
        reply.text
    );

    match mode(&case.name) {
        Mode::Shape(reason) => {
            assert!(!reason.is_empty());
            compare_shape(case, reply);
        }
        Mode::Strict => compare_strict(case, reply),
    }

    compare_cors(case, reply);
}

/// The strict comparison: the legacy reason for a text 400, or every captured body key.
fn compare_strict(case: &Case, reply: &Reply) {
    if let Some(text) = case.response.get("__text__").and_then(Value::as_str) {
        if case.status() >= 400 {
            // Legacy's message lived in the status line; the shim carries it in the envelope.
            let body = reply.json.as_ref().unwrap_or_else(|| {
                panic!(
                    "{}: expected an error envelope, got {}",
                    case.name, reply.text
                )
            });
            assert_eq!(
                body["error"]["message"].as_str().unwrap_or_default(),
                case.reason(),
                "{}: the legacy reason string must survive byte for byte",
                case.name
            );
        } else {
            assert_eq!(
                reply.text, text,
                "{}: a text body must be byte-identical",
                case.name
            );
        }
        return;
    }

    let expected = case.response.as_object().unwrap_or_else(|| {
        panic!(
            "{}: the captured body is neither text nor an object",
            case.name
        )
    });
    let actual = reply
        .json
        .as_ref()
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("{}: expected a JSON object, got {}", case.name, reply.text));
    compare_object(&case.name, expected, actual);
}

/// Every expected key present and equal; every extra key on the additive allow-list.
fn compare_object(name: &str, expected: &Map<String, Value>, actual: &Map<String, Value>) {
    for (key, want) in expected {
        let got = actual
            .get(key)
            .unwrap_or_else(|| panic!("{name}: the response is missing the legacy key {key:?}"));
        assert_eq!(got, want, "{name}: key {key:?}");
    }
    for key in actual.keys() {
        assert!(
            expected.contains_key(key) || ADDITIVE_KEYS.contains(&key.as_str()),
            "{name}: the response adds an undocumented key {key:?}; \
             add it to ADDITIVE_KEYS and to DESIGN §11.1 or remove it"
        );
    }
}

/// The structural assertion for a [`Mode::Shape`] case: whatever can be checked without the
/// capture's state.
fn compare_shape(case: &Case, reply: &Reply) {
    if case.status() >= 400 || promoted_status(&case.name).is_some() {
        let body = reply.json.as_ref().unwrap_or_else(|| {
            panic!(
                "{}: expected an error envelope, got {}",
                case.name, reply.text
            )
        });
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            !message.is_empty(),
            "{}: every error envelope must carry a message",
            case.name
        );
        // The one promoted case whose string DESIGN §11.7 pins.
        if case.name == "subscriptions_update_enabled_not_a_boolean" {
            assert_eq!(message, "enabled must be a boolean", "{}", case.name);
        }
        return;
    }

    let actual = reply
        .json
        .as_ref()
        .unwrap_or_else(|| panic!("{}: expected JSON, got {}", case.name, reply.text));
    match case.name.as_str() {
        "history_seeded" | "history_after_mutations" => assert_history_shape(&case.name, actual),
        "subscriptions_list" | "subscriptions_list_after_mutations" => {
            let rows = actual
                .as_array()
                .unwrap_or_else(|| panic!("{}: subscriptions is an array", case.name));
            for row in rows {
                assert_subscription_shape(&case.name, row);
            }
        }
        "subscriptions_update_rename"
        | "subscriptions_update_blank_name_is_ignored"
        | "subscriptions_update_check_interval_is_floored_at_1"
        | "subscriptions_update_enabled_string_is_accepted" => {
            // Either the seeded id is unknown here (a `status: "error"` body carrying the legacy
            // string) or the manager answered — both are legal, and both are legacy shapes.
            assert!(
                actual["status"] == "ok" || actual["status"] == "error",
                "{}: a legacy status body",
                case.name
            );
            if actual["status"] == "error" {
                assert_eq!(actual["msg"], "Subscription not found", "{}", case.name);
            } else {
                assert_subscription_shape(&case.name, &actual["subscription"]);
            }
        }
        "version" | "version_with_allowed_origin" | "version_with_disallowed_origin" => {
            assert_eq!(actual["version"], "wp00-capture", "{}", case.name);
            assert!(
                actual.get("yt-dlp").is_some(),
                "{}: the legacy key must be present, whatever its value",
                case.name
            );
            assert_eq!(actual["protocol"], "v2", "{}: additive", case.name);
            assert_eq!(actual["url_prefix"], "/", "{}: additive", case.name);
        }
        other => panic!("{other}: a shape case with no structural assertion"),
    }
}

/// The `GET history` schema the shipped Swift models require (PLAN WP-15's schema check).
pub fn assert_history_shape(name: &str, body: &Value) {
    for key in ["queue", "pending", "done"] {
        assert!(
            body[key].is_array(),
            "{name}: {key} must always be present and an array"
        );
    }
    for key in ["queue", "pending", "done"] {
        for item in body[key].as_array().into_iter().flatten() {
            assert_item_shape(name, item);
        }
    }
}

/// The five legacy status strings, and nothing else.
pub const V1_STATUSES: [&str; 5] = ["pending", "preparing", "downloading", "finished", "error"];

/// One `history` item: the key set, the closed status vocabulary, and the numeric types.
pub fn assert_item_shape(name: &str, item: &Value) {
    let object = item
        .as_object()
        .unwrap_or_else(|| panic!("{name}: every history entry is an object"));
    for key in [
        "id",
        "title",
        "url",
        "status",
        "percent",
        "filename",
        "size",
        "msg",
        "error",
        "timestamp",
    ] {
        assert!(object.contains_key(key), "{name}: item is missing {key}");
    }
    assert!(
        !object.contains_key("entry"),
        "{name}: entry must be omitted (C22)"
    );
    let status = object["status"].as_str().unwrap_or_default();
    assert!(
        V1_STATUSES.contains(&status),
        "{name}: {status:?} is not one of the five legacy statuses"
    );
    assert!(
        object["percent"].is_number(),
        "{name}: percent must decode as a number"
    );
    assert!(
        object["timestamp"].is_number(),
        "{name}: timestamp must be a number"
    );
}

/// One subscription: exactly the legacy 13 keys, with `last_checked` as float seconds or null.
pub fn assert_subscription_shape(name: &str, row: &Value) {
    let object = row
        .as_object()
        .unwrap_or_else(|| panic!("{name}: a subscription is an object"));
    assert_eq!(
        object.len(),
        13,
        "{name}: the legacy projection has 13 keys"
    );
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
    ] {
        assert!(object.contains_key(key), "{name}: missing {key}");
    }
    assert!(
        object["last_checked"].is_null() || object["last_checked"].is_f64(),
        "{name}: last_checked is float seconds or null in v1"
    );
    assert!(object["enabled"].is_boolean(), "{name}: enabled is a bool");
}

/// Legacy's `on_prepare`: two headers when the `Origin` is allowed, none otherwise (DESIGN §11.6).
fn compare_cors(case: &Case, reply: &Reply) {
    let captured = case.meta["response_headers"]["Access-Control-Allow-Origin"].as_str();
    match captured {
        Some(origin) => {
            assert_eq!(
                reply.allow_origin.as_deref(),
                Some(origin),
                "{}: the allowed origin must be reflected",
                case.name
            );
        }
        None => assert!(
            reply.allow_origin.is_none(),
            "{}: no origin was reflected in the capture, so none may be reflected now (got {:?})",
            case.name,
            reply.allow_origin
        ),
    }
}
