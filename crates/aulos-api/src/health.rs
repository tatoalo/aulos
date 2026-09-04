//! `GET <p>healthz` and `GET <p>livez` (DESIGN §16.3).
//!
//! `healthz` answers `200` when everything required works, `200` with `"status":"degraded"` when
//! an optional component is down, and `503` **only** when the store is unusable or its WAL has run
//! away — the one condition that makes the service useless. The container's `HEALTHCHECK` uses it,
//! so anything softer than that must not restart the container.
//!
//! The `components` map is owned by [`aulos_core::HealthRegistry`], which the binary's probes
//! write (DESIGN §16.1, §16.2). Two components are synthesised here when the registry has none,
//! because `aulos-api` is the only thing that can see them and the 503 rule depends on the first:
//! `store` (from the [`aulos_store::Store`] handle) and `queue` (from the published snapshot).
//!
//! `?probe=deep` re-runs every provider's own [`aulos_provider::Provider::probe`] and folds the
//! result back into the registry's `Ready`/`Degraded` state, rate-limited to one run per ten
//! seconds so `doctor` and the runbook cannot be turned into a denial of service.

use aulos_core::ComponentStatus;
use axum::extract::State;

use crate::v2::Q;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::error::Json;
use crate::{ApiState, v2};

/// The WAL ceiling above which `healthz` answers `503` (DESIGN §16.3).
pub const WAL_LIMIT_BYTES: u64 = 256 * 1024 * 1024;

/// `?probe=deep` on `healthz`.
#[derive(Debug, Deserialize)]
pub struct HealthQuery {
    /// `deep` re-runs the tool probes live.
    pub probe: Option<String>,
}

/// `GET <p>livez` — `200 {"ok":true}`, doing no work at all.
pub async fn livez() -> Json<Value> {
    Json(json!({ "ok": true }))
}

/// `GET <p>healthz`.
pub async fn healthz(State(state): State<ApiState>, Q(query): Q<HealthQuery>) -> Response {
    let deep = query.probe.as_deref() == Some("deep");
    let probe_label = if deep {
        if v2::meta::claim_deep_probe(&state) {
            run_deep_probe(&state).await;
            "deep"
        } else {
            // Rate-limited: the answer is still honest, it is just the cached component set.
            "throttled"
        }
    } else {
        "shallow"
    };

    let view = state.health.snapshot();
    let mut components: Map<String, Value> = view
        .components
        .iter()
        .map(|(name, component)| (name.clone(), json!(component)))
        .collect();

    let wal_bytes = state.store.wal_bytes();
    components
        .entry("store".to_owned())
        .or_insert_with(|| store_component(&state, wal_bytes));
    components
        .entry("queue".to_owned())
        .or_insert_with(|| queue_component(&state));

    let published = state.state.load();
    let wal_blown = wal_bytes > WAL_LIMIT_BYTES;
    let status = if view.is_fatal() || wal_blown {
        ComponentStatus::Down
    } else {
        view.status
    };
    let http = if view.is_fatal() || wal_blown {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };

    let body = json!({
        "status": status.as_str(),
        "version": state.info.version,
        "yt_dlp": state.info.yt_dlp,
        "boot_id": state.hub.boot_id(),
        "uptime_s": uptime_s(&state),
        "url_prefix": state.cfg.url_prefix,
        "v1_shim": state.cfg.v1_enabled,
        "seq": state.hub.head().0,
        "probe": probe_label,
        "components": components,
        "providers": providers(&state),
        "ws": {
            "clients": state.live.clients.load(std::sync::atomic::Ordering::Relaxed),
            "frames_total": state.hub.frames_published(),
            "lagged_total": state.live.lagged.load(std::sync::atomic::Ordering::Relaxed),
            "slow_disconnects": state
                .live
                .slow_disconnects
                .load(std::sync::atomic::Ordering::Relaxed),
        },
        "items": published.counts,
    });
    (http, Json(body)).into_response()
}

/// Seconds since the process started.
fn uptime_s(state: &ApiState) -> i64 {
    state
        .now_ms()
        .saturating_sub(state.info.started_at)
        .max(0)
        .div_euclid(1000)
}

/// The `store` component, when nothing else published one.
fn store_component(state: &ApiState, wal_bytes: u64) -> Value {
    let status = if wal_bytes > WAL_LIMIT_BYTES {
        ComponentStatus::Down
    } else {
        ComponentStatus::Ok
    };
    json!({
        "status": status.as_str(),
        "wal_bytes": wal_bytes,
        "db_bytes": state.store.db_bytes(),
        "commits_total": state.store.commit_count(),
    })
}

/// The `queue` component, when nothing else published one.
fn queue_component(state: &ApiState) -> Value {
    let published = state.state.load();
    let counts = published.counts;
    json!({
        "status": ComponentStatus::Ok.as_str(),
        "downloading": counts.downloading,
        "postprocessing": counts.postprocessing,
        "queued": counts.queued,
        "resolving": counts.resolving,
    })
}

/// The `providers` array — the same summaries `capabilities` carries.
fn providers(state: &ApiState) -> Value {
    let body = v2::meta::capabilities_providers(state);
    Value::Array(body)
}

/// Re-probes every provider and folds the answer back into the registry (DESIGN §6.4, §16.3).
async fn run_deep_probe(state: &ApiState) {
    let results = v2::meta::probe_providers(state).await;
    let mut registry = match state.registry.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    for (id, health) in results {
        match health {
            aulos_provider::ProviderHealth::Ok => registry.set_ready(&id),
            aulos_provider::ProviderHealth::Degraded(reason)
            | aulos_provider::ProviderHealth::Down(reason) => {
                registry.set_degraded(&id, reason);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wal_ceiling_is_the_design_number() {
        assert_eq!(WAL_LIMIT_BYTES, 268_435_456);
    }
}
