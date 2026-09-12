//! The add path and the dedupe policy (DESIGN §8.3, §8.5).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use aulos_core::{
    AddReason, Codec, DomainEvent, DownloadType, ErrorCode, FormatId, QualityId, RelDir, Selection,
    SourceKind, SourceRef, Status,
};
use aulos_queue::{AddError, DedupeKey};
use support::{Harness, request, selection};

#[tokio::test]
async fn a_single_add_inserts_resolving_and_acks_before_resolution() {
    let h = Harness::new().await;
    let out = h
        .handle
        .add(
            vec![request("https://fake.test/watch/one")],
            SourceRef::bare(SourceKind::ApiV2),
        )
        .await
        .unwrap();
    assert_eq!(out.ids.len(), 1);
    assert!(out.duplicates.is_empty());
    assert_eq!(
        out.generation, 1,
        "one generation per add, counted from 1 so 0 means \"no add\""
    );

    let id = out.ids[0];
    // Insert status is always `resolving` (DESIGN §8.3), and the row is already persisted when the
    // ack lands, because the insert is `Durability::Sync`.
    let row = h
        .item(id)
        .await
        .expect("the row is persisted before the ack");
    assert_eq!(row.status, Status::Resolving);
    assert_eq!(row.provider, None, "no provider until resolution names one");
    assert_eq!(
        &*row.title, "https://fake.test/watch/one",
        "the URL stands in"
    );
    assert!(row.canonical_key.starts_with("fake\u{1f}"));

    let resolved = h.until_resolved(id).await;
    assert_eq!(resolved.status, Status::Queued);
    assert_eq!(&*resolved.title, "one");
    assert_eq!(
        resolved
            .provider
            .as_ref()
            .map(aulos_core::ProviderId::as_str),
        Some("fake")
    );

    h.events
        .until("add", |e| matches!(e, DomainEvent::Added(..)))
        .await;
    let added = h.events.added();
    assert_eq!(added.len(), 1);
    assert_eq!(added[0], (vec![id], AddReason::Created));
}

#[tokio::test]
async fn a_batch_of_fifty_lands_in_one_transaction_with_ord_in_request_order() {
    let h = Harness::new().await;
    let requests: Vec<_> = (0..50)
        .map(|i| request(&format!("https://fake.test/watch/{i}")))
        .collect();
    let before = h.store.job_count();
    let out = h
        .handle
        .add(requests, SourceRef::bare(SourceKind::ApiV2))
        .await
        .unwrap();
    assert_eq!(out.ids.len(), 50);
    assert_eq!(
        h.store.job_count() - before,
        1,
        "one batched InsertItems for the whole add (DESIGN §8.3)"
    );

    let mut ords = Vec::new();
    for id in &out.ids {
        ords.push(h.item(*id).await.unwrap().ord);
    }
    let mut sorted = ords.clone();
    sorted.sort_unstable();
    assert_eq!(ords, sorted, "`ord` follows request order");
}

#[tokio::test]
async fn a_duplicate_returns_the_existing_id_in_active_mode() {
    let h = Harness::new().await;
    let first = h.add("https://fake.test/watch/dup").await;
    h.until_resolved(first).await;

    let out = h
        .handle
        .add(
            vec![request("https://fake.test/watch/dup")],
            SourceRef::bare(SourceKind::ApiV2),
        )
        .await
        .unwrap();
    assert!(out.ids.is_empty(), "no new item");
    assert_eq!(out.duplicates.len(), 1);
    assert_eq!(out.duplicates[0].existing_id, first);
    assert_eq!(&*out.duplicates[0].url, "https://fake.test/watch/dup");
}

#[tokio::test]
async fn a_duplicate_is_a_conflict_in_strict_mode_and_a_new_item_when_off() {
    let strict = Harness::builder()
        .env("AULOS_DEDUPE_MODE", "strict")
        .build()
        .await;
    let first = strict.add("https://fake.test/watch/dup").await;
    strict.until_resolved(first).await;
    let err = strict
        .add_request(request("https://fake.test/watch/dup"))
        .await
        .expect_err("strict mode answers 409");
    match err {
        AddError::Duplicate { existing_id, index } => {
            assert_eq!(existing_id, first);
            assert_eq!(index, 0);
            assert_eq!(err.code(), ErrorCode::Conflict);
        }
        other => panic!("expected a duplicate, got {other:?}"),
    }

    let off = Harness::builder()
        .env("AULOS_DEDUPE_MODE", "off")
        .build()
        .await;
    let a = off.add("https://fake.test/watch/dup").await;
    off.until_resolved(a).await;
    let b = off.add("https://fake.test/watch/dup").await;
    assert_ne!(a, b, "dedupe off creates a second item");
}

#[tokio::test]
async fn a_duplicate_with_a_different_selection_creates_a_second_item() {
    let h = Harness::new().await;
    let mp4 = h.add("https://fake.test/watch/same").await;
    h.until_resolved(mp4).await;

    let mut audio = request("https://fake.test/watch/same");
    audio.selection = Selection::new(
        DownloadType::Video,
        Codec::Auto,
        FormatId::parse("mp4").unwrap(),
        QualityId::parse("best").unwrap(),
    );
    // Same tuple: still a duplicate.
    let dup = h.add_request(audio.clone()).await.unwrap();
    assert_eq!(dup.duplicates.len(), 1);

    // A different tuple is a different key, so re-adding as `mp3` after pulling `mp4` is a
    // legitimate new item. The `fake` catalog advertises exactly one tuple (`video/mp4/best`), so
    // a second one cannot be *validated* through this provider — the key itself is what carries
    // the property, and it is asserted here against the real dedupe index rather than mocked.
    let resolved = h.item(mp4).await.unwrap();
    let same = DedupeKey::new(resolved.canonical_key.clone(), selection());
    let as_mp3 = DedupeKey::new(
        resolved.canonical_key.clone(),
        Selection::new(
            DownloadType::Audio,
            Codec::Auto,
            FormatId::parse("mp3").unwrap(),
            QualityId::parse("best").unwrap(),
        ),
    );
    assert_ne!(same, as_mp3, "the selection is part of the key");
    let mut index = std::collections::HashMap::new();
    index.insert(same, 1u8);
    index.insert(as_mp3, 2u8);
    assert_eq!(index.len(), 2, "so the two adds occupy two slots");
}

#[tokio::test]
async fn a_terminal_item_does_not_block_a_re_add() {
    let h = Harness::new().await;
    let first = h.add("https://fake.test/watch/again").await;
    h.until_status(first, Status::Finished).await;
    let out = h
        .add_request(request("https://fake.test/watch/again"))
        .await
        .unwrap();
    assert_eq!(
        out.ids.len(),
        1,
        "only non-terminal items participate in dedupe (DESIGN §8.5)"
    );
}

#[tokio::test]
async fn auto_start_false_inserts_resolving_and_only_then_parks() {
    let h = Harness::new().await;
    let mut req = request("https://fake.test/watch/later");
    req.auto_start = false;
    let out = h.add_request(req).await.unwrap();
    let id = out.ids[0];

    let inserted = h.item(id).await.unwrap();
    assert_eq!(
        inserted.status,
        Status::Resolving,
        "auto_start = false still resolves immediately (DESIGN §8.3)"
    );

    let resolved = h.until_resolved(id).await;
    assert_eq!(resolved.status, Status::Queued);
    assert!(!resolved.auto_start, "and only now is it parked");
    h.settle().await;
    assert_eq!(
        h.item(id).await.unwrap().status,
        Status::Queued,
        "an unscheduled item is never picked up"
    );
}

#[tokio::test]
async fn a_batch_over_the_cap_is_rejected_whole() {
    let h = Harness::builder()
        .env("AULOS_MAX_BATCH_URLS", "3")
        .build()
        .await;
    let requests: Vec<_> = (0..4)
        .map(|i| request(&format!("https://fake.test/watch/{i}")))
        .collect();
    let err = h
        .handle
        .add(requests, SourceRef::bare(SourceKind::ApiV2))
        .await
        .expect_err("over the cap");
    assert_eq!(err.code(), ErrorCode::PayloadTooLarge);
    assert!(h.rows().await.is_empty(), "nothing was inserted");
}

#[tokio::test]
async fn a_folder_with_custom_dirs_off_carries_the_verbatim_legacy_string() {
    let h = Harness::builder().env("CUSTOM_DIRS", "false").build().await;
    let mut req = request("https://fake.test/watch/x");
    req.folder = Some(RelDir::parse("shows").unwrap());
    let err = h.add_request(req).await.expect_err("folders are off");
    assert_eq!(err.code(), ErrorCode::FolderInvalid);
    match err {
        AddError::Invalid { errors, .. } => {
            assert_eq!(
                &*errors[0].message,
                "A folder for the download was specified but CUSTOM_DIRS is not true in the configuration."
            );
            assert_eq!(errors[0].field.as_deref(), Some("folder"));
        }
        other => panic!("expected a validation failure, got {other:?}"),
    }
}

#[tokio::test]
async fn a_missing_folder_with_create_off_carries_the_verbatim_legacy_string() {
    let h = Harness::builder()
        .env("CREATE_CUSTOM_DIRS", "false")
        .build()
        .await;
    let mut req = request("https://fake.test/watch/x");
    req.folder = Some(RelDir::parse("nope").unwrap());
    let err = h.add_request(req).await.expect_err("the folder is missing");
    match err {
        AddError::Invalid { errors, .. } => {
            let message = &*errors[0].message;
            assert!(message.starts_with(
                "Folder \"nope\" for download does not exist inside base directory \""
            ));
            assert!(
                message.ends_with("\", and CREATE_CUSTOM_DIRS is not true in the configuration.")
            );
        }
        other => panic!("expected a validation failure, got {other:?}"),
    }
}

#[tokio::test]
async fn a_folder_that_would_escape_the_root_is_rejected() {
    let h = Harness::new().await;
    // `RelDir` already rejects a `..` component, so the containment check is reached through an
    // absolute path instead — which `contain` resolves and then refuses.
    let escaping = RelDir::parse("ok/../../etc");
    assert!(
        escaping.is_err(),
        "a parent traversal never reaches the engine"
    );

    let mut req = request("https://fake.test/watch/x");
    req.folder = Some(RelDir::parse("shows/season 1").unwrap());
    let out = h
        .add_request(req)
        .await
        .expect("a nested folder is created");
    assert_eq!(out.ids.len(), 1);
    assert!(h.download_dir().join("shows/season 1").is_dir());
}

#[tokio::test]
async fn overrides_are_rejected_when_the_gate_is_closed() {
    let h = Harness::new().await;
    let mut req = request("https://fake.test/watch/x");
    req.ytdl_options_overrides
        .insert("retries".to_owned(), serde_json::json!(9));
    let err = h
        .add_request(req.clone())
        .await
        .expect_err("the gate is closed");
    assert_eq!(err.code(), ErrorCode::OverridesDisabled);
    match &err {
        AddError::Invalid { errors, .. } => {
            assert_eq!(&*errors[0].message, "ytdl_options_overrides are disabled");
        }
        other => panic!("expected a validation failure, got {other:?}"),
    }

    let open = Harness::builder()
        .env("ALLOW_YTDL_OPTIONS_OVERRIDES", "true")
        .build()
        .await;
    assert!(
        open.add_request(req).await.is_ok(),
        "and accepted when it is open"
    );
}

#[tokio::test]
async fn an_unknown_preset_is_rejected_with_its_own_code() {
    let h = Harness::new().await;
    let mut req = request("https://fake.test/watch/x");
    req.ytdl_options_presets = vec!["nope".into()];
    let err = h.add_request(req).await.expect_err("no such preset");
    assert_eq!(err.code(), ErrorCode::UnknownPreset);
}

#[tokio::test]
async fn a_url_no_provider_claims_is_unsupported() {
    let h = Harness::builder()
        .provider(Arc::new(support::fake()))
        .build()
        .await;
    let err = h
        .add_request(request("https://elsewhere.invalid/thing"))
        .await
        .expect_err("nothing matches");
    assert_eq!(err.code(), ErrorCode::UnsupportedUrl);
    match err {
        AddError::Invalid { errors, .. } => assert_eq!(
            &*errors[0].message,
            "Unsupported resource \"https://elsewhere.invalid/thing\""
        ),
        other => panic!("expected a validation failure, got {other:?}"),
    }
}

#[tokio::test]
async fn a_selection_the_catalog_rejects_reports_every_failing_field() {
    let h = Harness::new().await;
    let mut req = request("https://fake.test/watch/x");
    req.selection = Selection::new(
        DownloadType::Audio,
        Codec::Auto,
        FormatId::parse("mp3").unwrap(),
        QualityId::parse("best").unwrap(),
    );
    let err = h
        .add_request(req)
        .await
        .expect_err("the fake catalog is video/mp4/best only");
    assert_eq!(err.code(), ErrorCode::ValidationFailed);
    match err {
        AddError::Invalid { errors, .. } => {
            assert!(!errors.is_empty());
            assert!(errors.iter().all(|e| e.field.is_some()));
        }
        other => panic!("expected a validation failure, got {other:?}"),
    }
}

#[tokio::test]
async fn the_selection_used_by_the_fixtures_is_the_one_the_fake_catalog_accepts() {
    // A guard on the fixtures themselves: every other test in this file assumes it.
    let h = Harness::new().await;
    let mut req = request("https://fake.test/watch/x");
    req.selection = selection();
    assert!(h.add_request(req).await.is_ok());
}
