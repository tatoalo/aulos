//! `stress_consistency` — the mandatory one (PLAN WP-14).
//!
//! Under a fifty-item load, client state is reconstructed **purely from the frame stream** with the
//! PROTOCOL §7 apply algorithm and compared, field for field, against the authoritative snapshot.
//! Any mismatch fails.
//!
//! # What makes the comparison fair
//!
//! The aggregator emits a flush's frames and *then* republishes the snapshot (DESIGN §15.1), so
//! `Published.seq` is always the last frame of a completed flush. The comparison therefore reads
//! the snapshot's `seq` — call it `S` — and applies exactly the frames with `seq <= S`, keeping
//! the rest for the next round. That is the only alignment under which "the client is equal to the
//! server" is a statement about consistency rather than about timing.
//!
//! The rig runs with a 50 ms batch window instead of the stock 250 ms, so the five-second
//! comparison cadence of the acceptance list is compressed to one second — the same number of
//! flushes per round, five times less wall clock.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aulos_provider::fake::FakeProvider;
use serde_json::{Value, json};
use support::{Rig, apply, connect, next_frame, try_next_frame, ytdlp_like};

/// How many items the load is.
const ITEMS: usize = 50;

/// How many comparison rounds run while the load is in flight.
const ROUNDS: usize = 5;

/// One round of the acceptance list's five-second cadence, compressed by the rig's 5× faster
/// batch window.
const ROUND: Duration = Duration::from_secs(1);

/// A provider that produces real progress traffic: several stages, several percentages, and a
/// produced file, so the frame stream carries `added`, `delta` and `completed` alike.
fn scripted() -> FakeProvider {
    FakeProvider::from_toml(
        r#"
        id = "fake"
        score = 200
        strong = true
        hosts = ["fake.test"]

        [[timeline]]
        write_files = false
        download = [
            { kind = "stage", stage = "preparing" },
            { kind = "stage", stage = "downloading" },
            { kind = "progress", percent = 20.0, speed = 1048576.0, eta = 8 },
            { kind = "wait", ms = 30 },
            { kind = "progress", percent = 55.0, speed = 2097152.0, eta = 4 },
            { kind = "wait", ms = 30 },
            { kind = "stage", stage = "postprocessing" },
            { kind = "progress", percent = 100.0 },
            { kind = "finish", filename = "clip.mp4", size = 4096 },
        ]
    "#,
    )
    .unwrap()
}

#[tokio::test]
async fn stress_consistency() {
    for prefix in support::PREFIXES {
        let rig = Rig::builder(prefix)
            .without_default_providers()
            .provider(Arc::new(ytdlp_like()))
            .provider(Arc::new(scripted()))
            .env("MAX_CONCURRENT_DOWNLOADS", "4")
            .start()
            .await;

        let mut socket = connect(&rig, "ws").await;
        let snapshot = next_frame(&mut socket).await;
        assert_eq!(snapshot["t"], "snapshot");
        let mut client: HashMap<String, Value> = HashMap::new();
        apply(&mut client, &snapshot);
        let mut buffered: Vec<Value> = Vec::new();

        // Fifty items, five batches — the share-sheet shape.
        for batch in 0..5 {
            let items: Vec<Value> = (0..ITEMS / 5)
                .map(|n| json!({ "url": format!("https://fake.test/item-{batch}-{n}") }))
                .collect();
            let (status, body) = rig
                .post("api/v2/downloads", &json!({ "items": items }))
                .await;
            assert_eq!(status, 202, "{body}");
        }

        for round in 0..ROUNDS {
            tokio::time::sleep(ROUND).await;
            compare(&rig, &mut socket, &mut client, &mut buffered, round).await;
        }

        // And once more when everything has come to rest, which is where a lost `completed` or a
        // stale `percent` would show up most plainly.
        rig.settle().await;
        tokio::time::sleep(ROUND).await;
        compare(&rig, &mut socket, &mut client, &mut buffered, ROUNDS).await;

        // Fifty items, all terminal, every one of them known to the client.
        assert_eq!(client.len(), ITEMS, "every item is in the client's state");
        assert!(
            client.values().all(|i| i["status"] == "finished"),
            "the load finished: {:?}",
            client
                .values()
                .map(|i| i["status"].clone())
                .collect::<Vec<_>>()
        );
    }
}

/// One comparison round: align the client to the snapshot's `seq`, then assert equality.
async fn compare(
    rig: &Rig,
    socket: &mut support::Socket,
    client: &mut HashMap<String, Value>,
    buffered: &mut Vec<Value>,
    round: usize,
) {
    let (status, snapshot) = rig.get("api/v2/state").await;
    assert_eq!(status, 200, "{snapshot}");
    let target = snapshot["seq"].as_u64().unwrap();

    // Read until the stream has passed `target`, or until it goes quiet — either way every frame
    // at or below the target has then been received.
    let mut passed = buffered.iter().any(|f| f["seq"].as_u64() > Some(target));
    while !passed {
        match try_next_frame(socket, Duration::from_millis(500)).await {
            None => break,
            Some(frame) => {
                passed = frame["seq"].as_u64() > Some(target);
                buffered.push(frame);
            }
        }
    }

    let mut keep = Vec::new();
    for frame in buffered.drain(..) {
        if frame["seq"].as_u64().unwrap_or(0) <= target {
            apply(client, &frame);
        } else {
            keep.push(frame);
        }
    }
    *buffered = keep;

    let mut expected: HashMap<String, Value> = HashMap::new();
    for list in ["items", "done"] {
        for item in snapshot[list].as_array().into_iter().flatten() {
            expected.insert(item["id"].as_str().unwrap().to_owned(), item.clone());
        }
    }

    let mut client_ids: Vec<&String> = client.keys().collect();
    let mut expected_ids: Vec<&String> = expected.keys().collect();
    client_ids.sort();
    expected_ids.sort();
    assert_eq!(
        client_ids, expected_ids,
        "round {round}: the id sets differ at seq {target}"
    );

    for (id, want) in &expected {
        let got = &client[id];
        if got != want {
            let differing: Vec<String> = want
                .as_object()
                .unwrap()
                .iter()
                .filter(|(key, value)| got.get(*key) != Some(*value))
                .map(|(key, value)| format!("{key}: server={value} client={}", got[key.as_str()]))
                .collect();
            panic!(
                "round {round}: item {id} differs at seq {target}:\n  {}",
                differing.join("\n  ")
            );
        }
    }
}
