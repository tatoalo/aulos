//! `PUT`/`DELETE api/v2/devices/{token}` and its `live-activities/{item_id}` pair
//! (PROTOCOL §4.8, DESIGN §25).
//!
//! Four routes, all idempotent, all answering `204 No Content`. They are the whole write surface
//! the iOS app needs for push: one call when the app gets an APNs device token, one when it starts
//! a Live Activity for a download, and the two deletes that undo them.
//!
//! # Why every one of them is `204` and not a body
//!
//! There is nothing to say back. The app already knows the token — it is in the path — and it has
//! no use for a server-side echo of what it just sent. That also decides what happens to an unknown
//! request field: PROTOCOL §4.1's "an unknown field is a `warnings` entry, never a 400" has nowhere
//! to put a warning here, so unknown keys are logged at `debug` and ignored. The rule that matters
//! (a forward-compatible client is never rejected for saying too much) is kept.
//!
//! # Validation
//!
//! A device token and a Live Activity token are both **hex**, 32 to 200 characters. They are
//! normalised to lowercase on the way in, so `DELETE` with the token a client happened to
//! uppercase still finds the row it registered. Anything else — a `platform` that is not `ios`, an
//! `environment` outside the two gateways, a `bundle_id` that is not a reverse-DNS identifier — is
//! a `400 validation_failed` naming the field, because a device registered under a wrong value
//! would fail silently much later, inside APNs, as a `DeviceTokenNotForTopic` nobody is watching.
//! `install_id` joins that list for the same reason with a quieter failure: a device stored under
//! the wrong install id simply stops being alerted, weeks later, with nothing in any log.
//!
//! `bundle_id` is checked against **`APNS_TOPIC`** as well as against its shape. It becomes the
//! `apns-topic` of every push to that device, so leaving it free-form lets any holder of
//! `AULOS_API_TOKEN` pick which app the operator's ES256 provider key signs for. The accepted
//! values are `APNS_TOPIC` itself and its extensions (`<APNS_TOPIC>.something`, which is how an
//! App Clip or a widget extension is named); anything else is a `400` on `bundle_id`.

use aulos_core::{ApnsEnvironment, DeviceRecord, ItemId, LiveActivityRecord, PortError};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};

use crate::ApiState;
use crate::error::ApiError;
use crate::v2::{json_body, parse_bool, parse_str};

/// The shortest APNs token this server accepts. A classic device token is 64 hex characters; the
/// floor is lower because Apple has never promised the length and has changed it before.
const TOKEN_MIN: usize = 32;

/// The longest. Well above anything Apple has issued, and low enough that the column stays sane.
const TOKEN_MAX: usize = 200;

/// The longest `app_version` string kept. It is a log label, not data.
const APP_VERSION_MAX: usize = 64;

/// The longest bundle id accepted.
const BUNDLE_ID_MAX: usize = 200;

/// The keys `PUT api/v2/devices/{token}` knows.
const DEVICE_FIELDS: [&str; 7] = [
    "platform",
    "bundle_id",
    "environment",
    "alerts",
    "live_activity_start_token",
    "install_id",
    "app_version",
];

/// The keys `PUT …/live-activities/{item_id}` knows.
const ACTIVITY_FIELDS: [&str; 1] = ["update_token"];

/// `PUT api/v2/devices/{token}` — register or refresh one device. `204`, idempotent.
pub async fn register(
    State(state): State<ApiState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let token = parse_token("token", &token)?;
    let root = json_body(&headers, &body)?;
    log_unknown(&root, &DEVICE_FIELDS);

    let platform = parse_platform(&root)?;
    let bundle_id = parse_bundle_id(&root, &state.cfg.apns_topic)?;
    let environment = parse_environment(&root)?;
    // Absent means "yes": a device that registered at all wants to hear about its downloads, and
    // an older client that predates the switch must not go silent when the server learns it.
    let alerts = match root.get("alerts") {
        None | Some(Value::Null) => true,
        Some(value) => parse_bool("alerts", value)?,
    };
    let start_token = match root.get("live_activity_start_token") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_token(
            "live_activity_start_token",
            parse_str("live_activity_start_token", value)?,
        )?),
    };
    let install_id = parse_install_id(&root)?;
    let app_version = match root.get("app_version") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let raw = parse_str("app_version", value)?.trim();
            if raw.is_empty() {
                None
            } else if raw.chars().count() > APP_VERSION_MAX {
                return Err(ApiError::invalid(
                    "app_version",
                    format!("app_version must be at most {APP_VERSION_MAX} characters"),
                ));
            } else {
                Some(raw.into())
            }
        }
    };

    let now = state.now_ms();
    state
        .devices
        .upsert_device(DeviceRecord {
            token,
            platform,
            bundle_id,
            environment,
            alerts,
            live_activity_start_token: start_token,
            install_id,
            app_version,
            // On a repeat `PUT` the store keeps the `registered_at` it already has, so both
            // columns being "now" here is correct for the first registration and harmless after.
            registered_at: now,
            last_seen_at: now,
        })
        .await
        .map_err(port_error)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `DELETE api/v2/devices/{token}` — forget a device and its Live Activities. `204`, idempotent.
///
/// A token that was never registered is still a `204`: the caller asked for a state ("this token
/// is not registered") that holds when the call returns, and the app deleting a token the notifier
/// already pruned on a `410 Unregistered` is the normal case, not an error.
pub async fn unregister(
    State(state): State<ApiState>,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    let token = parse_token("token", &token)?;
    state
        .devices
        .remove_device(&token)
        .await
        .map_err(port_error)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `PUT api/v2/devices/{token}/live-activities/{item_id}` — forward an update token. `204`.
///
/// `404` when the device is unknown, because a Live Activity has no meaning without one: the push
/// needs the device's environment to pick a gateway, and an activity row under no device would
/// never be delivered. The **item** need not exist — the app starts an activity the moment the user
/// taps download, which can beat the server's own item row.
pub async fn register_activity(
    State(state): State<ApiState>,
    Path((token, item)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, ApiError> {
    let token = parse_token("token", &token)?;
    let item = parse_ulid(&item)?;
    let root = json_body(&headers, &body)?;
    log_unknown(&root, &ACTIVITY_FIELDS);

    let update_token = root
        .get("update_token")
        .ok_or_else(|| ApiError::invalid("update_token", "update_token is required"))?;
    let update_token = parse_token("update_token", parse_str("update_token", update_token)?)?;

    let device = device_by_token(&state, &token).await?;

    state
        .devices
        .upsert_live_activity(LiveActivityRecord {
            device_token: device.token,
            item_id: item,
            update_token,
            // Copied from the device, so a push needs no join: the activity's tokens are minted by
            // the same build against the same gateway.
            environment: device.environment,
            registered_at: state.now_ms(),
        })
        .await
        .map_err(port_error)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `DELETE api/v2/devices/{token}/live-activities/{item_id}` — `204`, idempotent.
///
/// Unlike the `PUT`, an unknown device is **not** a `404`: the caller wants the registration gone,
/// and it is gone. Answering `404` would make the app's cleanup path — which runs exactly when the
/// device may already have been pruned — look like a failure.
pub async fn unregister_activity(
    State(state): State<ApiState>,
    Path((token, item)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let token = parse_token("token", &token)?;
    let item = parse_ulid(&item)?;
    state
        .devices
        .remove_live_activity(&token, item)
        .await
        .map_err(port_error)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// parsing
// ---------------------------------------------------------------------------

/// A hex token of a plausible length, lowercased.
///
/// # Errors
/// `400 validation_failed` naming `field`.
fn parse_token(field: &str, raw: &str) -> Result<Box<str>, ApiError> {
    let trimmed = raw.trim();
    if trimmed.len() < TOKEN_MIN || trimmed.len() > TOKEN_MAX {
        return Err(ApiError::invalid(
            field,
            format!(
                "{field} must be {TOKEN_MIN}-{TOKEN_MAX} hexadecimal characters (got {})",
                trimmed.len()
            ),
        ));
    }
    if !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::invalid(
            field,
            format!("{field} must be hexadecimal"),
        ));
    }
    Ok(trimmed.to_ascii_lowercase().into_boxed_str())
}

/// The install this device belongs to: the same value the app sends as `X-Aulos-Install`
/// (PROTOCOL §1.3, §4.8). Absent or `null` is `None`.
///
/// The shape is exactly [`crate::v2::downloads::install_id`]'s — one function, so the id a device
/// registers under can never disagree with the id its adds carry. What differs is the verdict on a
/// malformed value: the *header* is silently ignored (a proxy must not be able to break an add),
/// while this **field** is a `400 validation_failed` like every other field on this route. The
/// asymmetry is deliberate and is the module docs' rule — a device stored under a wrong install id
/// would not fail here, it would go quiet weeks later with nobody watching.
///
/// # Errors
/// `400 validation_failed` naming `install_id`.
fn parse_install_id(root: &Map<String, Value>) -> Result<Option<Box<str>>, ApiError> {
    match root.get("install_id") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let raw = parse_str("install_id", value)?;
            crate::v2::downloads::install_id(Some(raw))
                .map(|id| Some(id.into()))
                .ok_or_else(|| {
                    ApiError::invalid(
                        "install_id",
                        "install_id must be 8-64 characters of [A-Za-z0-9._-]",
                    )
                })
        }
    }
}

/// The item id, as a ULID. It need not name an item that exists.
///
/// # Errors
/// `400 validation_failed` naming `item_id`.
fn parse_ulid(raw: &str) -> Result<ItemId, ApiError> {
    raw.parse()
        .map_err(|_| ApiError::invalid("item_id", format!("item_id must be a ULID: {raw:?}")))
}

/// `"ios"`, the only platform this server can push to.
fn parse_platform(root: &Map<String, Value>) -> Result<Box<str>, ApiError> {
    let raw = root
        .get("platform")
        .ok_or_else(|| ApiError::invalid("platform", "platform is required"))?;
    let raw = parse_str("platform", raw)?.trim().to_ascii_lowercase();
    if raw == "ios" {
        Ok(raw.into_boxed_str())
    } else {
        Err(ApiError::invalid(
            "platform",
            format!("platform must be \"ios\" (got {raw:?})"),
        ))
    }
}

/// The app's bundle id, which becomes the `apns-topic`.
///
/// It must be shaped like a reverse-DNS identifier **and** be `topic` (`APNS_TOPIC`) or an
/// extension of it. See the module docs for why the second half is not optional.
fn parse_bundle_id(root: &Map<String, Value>, topic: &str) -> Result<Box<str>, ApiError> {
    let raw = root
        .get("bundle_id")
        .ok_or_else(|| ApiError::invalid("bundle_id", "bundle_id is required"))?;
    let raw = parse_str("bundle_id", raw)?.trim();
    let shaped = !raw.is_empty()
        && raw.len() <= BUNDLE_ID_MAX
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        && !raw.starts_with('.')
        && !raw.ends_with('.');
    if !shaped {
        return Err(ApiError::invalid(
            "bundle_id",
            "bundle_id must be a reverse-DNS application identifier",
        ));
    }
    if !topic_allows(topic, raw) {
        return Err(ApiError::invalid(
            "bundle_id",
            format!("bundle_id must be {topic:?} or an extension of it (got {raw:?})"),
        ));
    }
    Ok(raw.into())
}

/// Whether `APNS_TOPIC` covers this bundle id: the topic itself, or something under it.
///
/// A blank `APNS_TOPIC` means the operator has expressed no opinion, and every shaped bundle id
/// is accepted — the pre-existing behaviour. Rejecting everything there would turn one blanked
/// setting into "no device can register", reported as a `400` naming an empty string.
fn topic_allows(topic: &str, bundle_id: &str) -> bool {
    let topic = topic.trim();
    if topic.is_empty() {
        return true;
    }
    bundle_id == topic
        || (bundle_id.starts_with(topic) && bundle_id[topic.len()..].starts_with('.'))
}

/// Which APNs gateway this device's tokens belong to.
fn parse_environment(root: &Map<String, Value>) -> Result<ApnsEnvironment, ApiError> {
    let raw = root
        .get("environment")
        .ok_or_else(|| ApiError::invalid("environment", "environment is required"))?;
    match parse_str("environment", raw)?.trim() {
        "sandbox" => Ok(ApnsEnvironment::Sandbox),
        "production" => Ok(ApnsEnvironment::Production),
        other => Err(ApiError::invalid(
            "environment",
            format!("environment must be \"sandbox\" or \"production\" (got {other:?})"),
        )),
    }
}

/// The registered device with this token, or `404`.
async fn device_by_token(state: &ApiState, token: &str) -> Result<DeviceRecord, ApiError> {
    // A whole-table read, deliberately: this is a household's phones and tablets, single digits,
    // and the port has no by-token read to add one to.
    let devices = state.devices.devices().await.map_err(port_error)?;
    devices
        .into_iter()
        .find(|d| &*d.token == token)
        .ok_or_else(|| {
            ApiError::not_found("no such device; register it with PUT api/v2/devices/{token} first")
        })
}

/// Logs the keys a route does not know. See the module docs for why this is not a `warnings` list.
fn log_unknown(root: &Map<String, Value>, known: &[&str]) {
    for key in root.keys() {
        if !known.contains(&key.as_str()) {
            tracing::debug!(field = %key, "ignoring an unknown field on a devices route");
        }
    }
}

/// A [`PortError`] in the error envelope.
fn port_error(e: PortError) -> ApiError {
    match e {
        PortError::NotFound(id) => ApiError::not_found(format!("no such item: {id}")),
        other => ApiError::unavailable(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aulos_core::ErrorCode;
    use serde_json::json;

    fn object(raw: &str) -> Map<String, Value> {
        match serde_json::from_str(raw).expect("valid JSON") {
            Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    #[test]
    fn a_token_is_hex_of_a_plausible_length_and_comes_back_lowercased() {
        let good = "A1B2".repeat(16);
        assert_eq!(
            &*parse_token("token", &good).expect("64 hex characters"),
            good.to_ascii_lowercase()
        );
        for bad in [
            "".to_owned(),
            "abcd".to_owned(),
            "z".repeat(64),
            "a".repeat(TOKEN_MAX + 1),
            format!("{}-", "a".repeat(63)),
        ] {
            let err = parse_token("token", &bad).expect_err(&bad);
            assert_eq!(err.code, ErrorCode::ValidationFailed, "{bad}");
            assert_eq!(err.field.as_deref(), Some("token"), "{bad}");
        }
    }

    #[test]
    fn a_token_at_each_end_of_the_range_is_accepted() {
        assert!(parse_token("token", &"a".repeat(TOKEN_MIN)).is_ok());
        assert!(parse_token("token", &"a".repeat(TOKEN_MAX)).is_ok());
        assert!(parse_token("token", &"a".repeat(TOKEN_MIN - 1)).is_err());
    }

    #[test]
    fn the_platform_is_ios_and_nothing_else() {
        assert_eq!(
            &*parse_platform(&object(r#"{"platform":"iOS"}"#)).expect("case insensitive"),
            "ios"
        );
        for bad in [r#"{}"#, r#"{"platform":"android"}"#, r#"{"platform":3}"#] {
            let err = parse_platform(&object(bad)).expect_err(bad);
            assert_eq!(err.field.as_deref(), Some("platform"), "{bad}");
        }
    }

    #[test]
    fn the_environment_is_one_of_the_two_gateways() {
        assert_eq!(
            parse_environment(&object(r#"{"environment":"sandbox"}"#)).expect("sandbox"),
            ApnsEnvironment::Sandbox
        );
        assert_eq!(
            parse_environment(&object(r#"{"environment":"production"}"#)).expect("production"),
            ApnsEnvironment::Production
        );
        for bad in [
            r#"{}"#,
            r#"{"environment":"staging"}"#,
            r#"{"environment":1}"#,
        ] {
            let err = parse_environment(&object(bad)).expect_err(bad);
            assert_eq!(err.field.as_deref(), Some("environment"), "{bad}");
        }
    }

    /// The `APNS_TOPIC` default, which is what the tests validate against.
    const TOPIC: &str = "com.tatoalo.aulos";

    #[test]
    fn a_bundle_id_is_a_reverse_dns_identifier() {
        assert_eq!(
            &*parse_bundle_id(&object(r#"{"bundle_id":"com.tatoalo.aulos"}"#), TOPIC)
                .expect("shaped"),
            "com.tatoalo.aulos"
        );
        for bad in [
            json!({}),
            json!({ "bundle_id": "" }),
            json!({ "bundle_id": "com.tatoalo aulos" }),
            json!({ "bundle_id": ".leading" }),
            json!({ "bundle_id": "trailing." }),
            json!({ "bundle_id": "a".repeat(BUNDLE_ID_MAX + 1) }),
        ] {
            let map = match bad {
                Value::Object(map) => map,
                other => panic!("not an object: {other}"),
            };
            let err = parse_bundle_id(&map, TOPIC).expect_err("must be rejected");
            assert_eq!(err.field.as_deref(), Some("bundle_id"));
        }
    }

    #[test]
    fn a_bundle_id_outside_apns_topic_is_rejected() {
        // A caller with the API token must not choose which app the operator's provider key
        // signs pushes for. Only APNS_TOPIC and its extensions are accepted.
        assert!(topic_allows(TOPIC, TOPIC));
        assert!(topic_allows(TOPIC, "com.tatoalo.aulos.clip"));
        assert!(topic_allows(
            TOPIC,
            "com.tatoalo.aulos.watchkitapp.complication"
        ));
        assert!(!topic_allows(TOPIC, "com.someone.else"));
        assert!(!topic_allows(TOPIC, "com.tatoalo.aulos2"));
        assert!(!topic_allows(TOPIC, "com.tatoalo"));
        // A blanked APNS_TOPIC is "no opinion", not "reject everything".
        assert!(topic_allows("", "com.someone.else"));
        assert!(topic_allows("   ", "com.someone.else"));

        let err = parse_bundle_id(&object(r#"{"bundle_id":"com.someone.else"}"#), TOPIC)
            .expect_err("a foreign topic must be rejected");
        assert_eq!(err.code, ErrorCode::ValidationFailed);
        assert_eq!(err.field.as_deref(), Some("bundle_id"));
        assert!(err.message.contains(TOPIC), "{}", err.message);

        // The extension is kept verbatim, so a widget's own topic still reaches Apple.
        assert_eq!(
            &*parse_bundle_id(&object(r#"{"bundle_id":"com.tatoalo.aulos.clip"}"#), TOPIC)
                .expect("an extension of the topic"),
            "com.tatoalo.aulos.clip"
        );
    }

    /// `install_id` shares its shape with the `X-Aulos-Install` header (PROTOCOL §1.3) and its
    /// *verdict* with every other field on this route: absent is `None`, malformed is a `400`.
    #[test]
    fn an_install_id_is_optional_but_never_silently_wrong() {
        assert_eq!(parse_install_id(&object("{}")).expect("absent"), None);
        assert_eq!(
            parse_install_id(&object(r#"{"install_id":null}"#)).expect("null"),
            None
        );
        assert_eq!(
            parse_install_id(&object(r#"{"install_id":"  iphone.0001  "}"#))
                .expect("trimmed")
                .as_deref(),
            Some("iphone.0001")
        );
        assert_eq!(
            parse_install_id(&object(
                r#"{"install_id":"3F2504E0-4F89-11D3-9A0C-0305E82C3301"}"#
            ))
            .expect("a UUID")
            .as_deref(),
            Some("3F2504E0-4F89-11D3-9A0C-0305E82C3301")
        );

        for bad in [
            json!({ "install_id": "" }),
            json!({ "install_id": "short7" }),
            json!({ "install_id": "a".repeat(65) }),
            json!({ "install_id": "has spaces here" }),
            json!({ "install_id": 42 }),
        ] {
            let map = match bad {
                Value::Object(map) => map,
                other => panic!("not an object: {other}"),
            };
            let err = parse_install_id(&map).expect_err("must be rejected");
            assert_eq!(err.code, ErrorCode::ValidationFailed);
            assert_eq!(err.field.as_deref(), Some("install_id"));
        }
    }

    #[test]
    fn an_item_id_must_be_a_ulid_but_need_not_exist() {
        let id = ItemId::new();
        assert_eq!(parse_ulid(&id.to_string()).expect("a ULID"), id);
        let err = parse_ulid("not-a-ulid").expect_err("nonsense");
        assert_eq!(err.field.as_deref(), Some("item_id"));
        assert_eq!(err.code, ErrorCode::ValidationFailed);
    }

    #[test]
    fn a_store_failure_is_503_and_never_500() {
        let err = port_error(PortError::Store("busy".into()));
        assert_eq!(err.code, ErrorCode::StateUnavailable);
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
