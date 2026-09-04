//! The add path (DESIGN §8.3) and dedupe policy (DESIGN §8.5).
//!
//! Everything here happens **synchronously before the ack**: provider selection, catalog
//! validation, the folder resolve and containment check, the preset and overrides gates, the
//! dedupe lookup, the ULID mint and the `ord` allocation — then one batched
//! `InsertItems` with `Durability::Sync`, then the ack, then `Added`. Resolution is background
//! work that starts after the caller already has its ids.
//!
//! Insert status is **always `resolving`**, including when `auto_start = false`: it is the *end* of
//! resolution that produces `queued(auto_start = false)`. There is no "insert directly as queued"
//! path, because a `queued` row whose resolve task is still running would contradict
//! PROTOCOL §3.1.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aulos_core::{
    AddReason, DedupeMode, DownloadRequest, ErrorCode, Item, ItemId, Kind, RelDir, SourceRef,
    Status, WireError, contain,
};
use aulos_store::{Durability, WriteOp};
use tokio::sync::oneshot;

use crate::cmd::{AddError, AddOutcome, Duplicate};
use crate::dedupe::DedupeKey;
use crate::engine::Engine;

/// The legacy string for a folder given with `CUSTOM_DIRS` off (DESIGN §11.7).
pub const FOLDER_CUSTOM_DIRS_OFF: &str =
    "A folder for the download was specified but CUSTOM_DIRS is not true in the configuration.";

/// The legacy string for a folder that resolves outside the base directory (DESIGN §11.7).
#[must_use]
pub fn folder_escapes(folder: &str, base: &Path) -> String {
    format!(
        "Folder \"{folder}\" must resolve inside the base download directory \"{}\"",
        base.display()
    )
}

/// The legacy string for a missing folder with `CREATE_CUSTOM_DIRS` off (DESIGN §11.7).
#[must_use]
pub fn folder_missing(folder: &str, base: &Path) -> String {
    format!(
        "Folder \"{folder}\" for download does not exist inside base directory \"{}\", and CREATE_CUSTOM_DIRS is not true in the configuration.",
        base.display()
    )
}

/// The legacy string for an overrides payload with the gate closed (DESIGN §11.7).
pub const OVERRIDES_DISABLED: &str = "ytdl_options_overrides are disabled";

impl Engine {
    /// [`crate::EngineCmd::Add`] (DESIGN §8.3).
    pub(crate) async fn handle_add(
        &mut self,
        requests: Vec<DownloadRequest>,
        source: SourceRef,
        ack: oneshot::Sender<Result<AddOutcome, AddError>>,
    ) {
        let max = self.cfg.max_batch_urls;
        if max > 0 && requests.len() > max as usize {
            let _ = ack.send(Err(AddError::TooManyUrls {
                max,
                got: requests.len(),
            }));
            return;
        }

        let presets = self.known_presets();
        let mut items: Vec<Item> = Vec::with_capacity(requests.len());
        let mut ids: Vec<ItemId> = Vec::with_capacity(requests.len());
        let mut duplicates: Vec<Duplicate> = Vec::new();
        let mut keys: Vec<DedupeKey> = Vec::new();

        for (index, request) in requests.into_iter().enumerate() {
            let prepared = match self.prepare(index, request, &presets, &source) {
                Ok(Some(p)) => p,
                Ok(None) => continue,
                Err(e) => {
                    let _ = ack.send(Err(e));
                    return;
                }
            };
            match prepared {
                Prepared::Item(minted) => {
                    let (item, key) = *minted;
                    ids.push(item.id);
                    keys.push(key);
                    items.push(item);
                }
                Prepared::Duplicate(d) => duplicates.push(d),
            }
        }

        if !items.is_empty()
            && !self
                .apply(
                    vec![WriteOp::InsertItems {
                        items: items.clone(),
                    }],
                    Durability::Sync,
                )
                .await
        {
            let _ = ack.send(Err(AddError::Unavailable(
                "the queue could not be written".into(),
            )));
            return;
        }

        let generation = self.add_generation;
        let mut views = Vec::with_capacity(items.len());
        for (item, key) in items.into_iter().zip(keys) {
            let id = item.id;
            let arc = self.cache_insert(item);
            self.dedupe.insert(key, id);
            views.push(self.view(&arc));
        }

        // Ack first (DESIGN §8.3), then the frame, then the background work.
        let _ = ack.send(Ok(AddOutcome {
            ids: ids.clone(),
            duplicates,
            generation,
        }));
        self.publish_added(views, AddReason::Created).await;
        for id in ids {
            self.spawn_resolve(id, generation, None).await;
        }
    }

    /// Every preset name the effective options declare, for the validator.
    pub(crate) fn known_presets(&self) -> BTreeSet<Box<str>> {
        self.ytdl
            .load()
            .presets
            .keys()
            .map(|k| Box::<str>::from(k.as_str()))
            .collect()
    }

    /// Validates one request and mints its row, or reports a duplicate (DESIGN §8.3, §8.5).
    fn prepare(
        &self,
        index: usize,
        request: DownloadRequest,
        presets: &BTreeSet<Box<str>>,
        source: &SourceRef,
    ) -> Result<Option<Prepared>, AddError> {
        // 1. Provider selection. "Nothing matched" is a real outcome and is `unsupported_url`.
        let selected = {
            let registry = self
                .registry
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let picked = registry
                .pick(&request.url, request.provider_hint.as_ref())
                .ok_or_else(|| {
                    AddError::field(
                        index,
                        ErrorCode::UnsupportedUrl,
                        "url",
                        format!("Unsupported resource \"{}\"", request.url),
                    )
                })?;
            let catalog = registry
                .by_id(&picked.id)
                .map(|p| p.catalog())
                .ok_or_else(|| {
                    AddError::field(
                        index,
                        ErrorCode::UnsupportedUrl,
                        "url",
                        format!("Unsupported resource \"{}\"", request.url),
                    )
                })?;
            (picked.id, catalog)
        };
        let (provider, catalog) = selected;

        // 2. Catalog + legacy-matrix validation, collecting every failure.
        if let Err(errors) = request.validate(&catalog, presets) {
            return Err(AddError::Invalid { index, errors });
        }

        // 3. The overrides gate.
        if !request.ytdl_options_overrides.is_empty() && !self.cfg.allow_ytdl_options_overrides {
            return Err(AddError::field(
                index,
                ErrorCode::OverridesDisabled,
                "ytdl_options_overrides",
                OVERRIDES_DISABLED,
            ));
        }

        // 4. Folder resolve, containment check and creation.
        self.resolve_folder(request.folder.as_ref(), request.selection.download_type)
            .map_err(|message| {
                AddError::field(index, ErrorCode::FolderInvalid, "folder", message)
            })?;

        // 5. Dedupe over `(canonical, selection)`, non-terminal items only.
        let key = DedupeKey::for_url(&provider, &request.url, request.selection.clone());
        if self.cfg.dedupe_mode != DedupeMode::Off
            && let Some(existing) = self.live_duplicate(&key)
        {
            return match self.cfg.dedupe_mode {
                DedupeMode::Strict => Err(AddError::Duplicate {
                    index,
                    existing_id: existing,
                }),
                DedupeMode::Off | DedupeMode::Active => Ok(Some(Prepared::Duplicate(Duplicate {
                    url: Arc::from(request.url.as_str()),
                    existing_id: existing,
                }))),
            };
        }

        // 6. Mint. `provider` stays `None` on the row until resolution names one (DESIGN §4.5),
        //    but its id is in the dedupe key, because that is what the key is *for*.
        let now = self.clock.now_ms();
        let item = Item {
            id: ItemId::new(),
            kind: Kind::Item,
            group_id: None,
            group_index: None,
            ord: self.store.next_ord(),
            url: request.url.clone(),
            canonical_key: key.canonical.clone(),
            provider: None,
            media_id: None,
            title: Box::from(request.url.as_str()),
            status: Status::Resolving,
            auto_start: request.auto_start,
            msg: None,
            error: None,
            request,
            entry: None,
            filename: None,
            size: None,
            chapter_files: Vec::new(),
            subtitle_files: Vec::new(),
            created_at: now,
            started_at: None,
            finished_at: None,
            attempt: 0,
            source: source.clone(),
            children_total: None,
            clear_after: None,
        };
        Ok(Some(Prepared::Item(Box::new((item, key)))))
    }

    /// The live item this key maps to, if any. Terminal rows do not participate (DESIGN §8.5).
    fn live_duplicate(&self, key: &DedupeKey) -> Option<ItemId> {
        let id = *self.dedupe.get(key)?;
        let item = self.items.get(&id)?;
        (!item.status.is_terminal()).then_some(id)
    }

    /// The legacy `__calc_download_path`, with containment tightened (DESIGN §8.3, §11.7).
    ///
    /// Returns the absolute output directory. The three error strings are byte-identical to
    /// legacy's.
    pub(crate) fn resolve_folder(
        &self,
        folder: Option<&RelDir>,
        download_type: aulos_core::DownloadType,
    ) -> Result<PathBuf, String> {
        let base = self.cfg.paths.root_for(download_type);
        let Some(folder) = folder.filter(|f| !f.as_str().is_empty()) else {
            return Ok(base.to_path_buf());
        };
        if !self.cfg.custom_dirs {
            return Err(FOLDER_CUSTOM_DIRS_OFF.to_owned());
        }
        let real_base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
        let dir = contain(base, folder.as_path())
            .map_err(|_| folder_escapes(folder.as_str(), &real_base))?;
        if !dir.is_dir() {
            if !self.cfg.create_custom_dirs {
                return Err(folder_missing(folder.as_str(), &real_base));
            }
            std::fs::create_dir_all(&dir).map_err(|e| {
                format!(
                    "Folder \"{}\" could not be created inside base directory \"{}\": {e}",
                    folder.as_str(),
                    real_base.display()
                )
            })?;
        }
        Ok(dir)
    }

    /// The absolute output directory for an item, already created.
    pub(crate) fn out_dir_for(&self, item: &Item) -> PathBuf {
        self.resolve_folder(
            item.request.folder.as_ref(),
            item.request.selection.download_type,
        )
        .unwrap_or_else(|e| {
            tracing::warn!(item = %item.id, "{e}");
            self.cfg
                .paths
                .root_for(item.request.selection.download_type)
                .to_path_buf()
        })
    }

    /// The per-job scratch directory: `<TEMP_DIR>/<item id>` (DESIGN §8.7, §10.5).
    ///
    /// Per job rather than the one shared `TEMP_DIR` legacy passed, because
    /// `aulos-provider-sc` uses `DownloadCtx::tmp_dir` **as** its segment directory: two
    /// concurrent StreamingCommunity jobs sharing it would interleave their segments. It also
    /// makes the DESIGN §8.7 partial cleanup one `remove_dir_all` instead of a glob over shared
    /// state, and keeps a paused yt-dlp job's `.part` exactly where the resume will look for it.
    pub(crate) fn tmp_dir_for(&self, id: ItemId) -> PathBuf {
        self.cfg.paths.temp.join(id.to_string())
    }
}

/// What [`Engine::prepare`] produced.
enum Prepared {
    /// A row to insert, with the dedupe key it claims. Boxed because an [`Item`] carries a whole
    /// [`DownloadRequest`] and every variant of an enum pays for the biggest one.
    Item(Box<(Item, DedupeKey)>),
    /// The request matched a live item instead.
    Duplicate(Duplicate),
}

/// The `WireError` a validation failure carries, so a caller can report the field.
#[must_use]
pub fn validation_error(field: &str, message: impl Into<Arc<str>>) -> WireError {
    WireError::field(ErrorCode::ValidationFailed, field, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_strings_are_byte_identical_to_legacy() {
        assert_eq!(
            FOLDER_CUSTOM_DIRS_OFF,
            "A folder for the download was specified but CUSTOM_DIRS is not true in the configuration."
        );
        assert_eq!(
            folder_escapes("../evil", Path::new("/downloads")),
            "Folder \"../evil\" must resolve inside the base download directory \"/downloads\""
        );
        assert_eq!(
            folder_missing("new", Path::new("/downloads")),
            "Folder \"new\" for download does not exist inside base directory \"/downloads\", and CREATE_CUSTOM_DIRS is not true in the configuration."
        );
        assert_eq!(OVERRIDES_DISABLED, "ytdl_options_overrides are disabled");
    }
}
