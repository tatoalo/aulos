//! The one-shot legacy JSON importer (DESIGN §7.6).
//!
//! It runs from the boot sequence when the database file did not exist yet, and on demand via
//! `aulos-server import`. It is the single most cutover-critical component in the workspace, and
//! its two hard guarantees are:
//!
//! - **T2: the legacy files are never touched.** No rename, no quarantine, no deletion, no
//!   rewrite — the only file the importer creates anywhere is `<STATE_DIR>/.aulos-imported`. A
//!   downgrade to the Python image therefore resumes from untouched JSON, which is what makes the
//!   cutover reversible in under two minutes (DESIGN §19.4).
//! - **One transaction.** Every row, every subscription, every seen id, every chat and the three
//!   `meta` provenance keys land in a single [`Store::write`] call, so there is no half-imported
//!   database. On a fatal error, or a file error under the default policy, nothing is written and
//!   the caller deletes the database file ([`delete_db_files`]).
//!
//! # The three failure classes (DESIGN §7.6.1)
//!
//! | Class | Example | Report | Effect |
//! |---|---|---|---|
//! | **Record** | one array element is malformed, or has a non-string `url` | `warnings[]`, `record_skipped` | that record is skipped, the rest of the file imports, the import **commits** |
//! | **File** | malformed JSON, `schema_version ∉ {1,2}`, the wrong `kind`, an unreadable file | `errors[]`, `file_invalid` | [`OnError`] decides |
//! | **Fatal** | the database cannot be written, a legacy `shelve` with no JSON counterpart, an unreadable `STATE_DIR` | `errors[]` | always rollback, no policy escape |
//!
//! [`OnError::Skip`] is the documented escape from the restart loop a single corrupt legacy file
//! would otherwise cause: legacy quarantined such a file and **started normally with that
//! collection empty**, and refusing to boot forever where the old server kept serving would be a
//! strictly worse operational property (DESIGN §7.6.1, §19.4).

pub(crate) mod canonical;
mod chats;
mod items;
pub(crate) mod legacy_model;
mod report;
mod sc;
mod subs;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aulos_core::{Item, ItemId, Status};
use serde_json::Value;

use crate::actor::now_ms;
use crate::error::StoreError;
use crate::import::items::{ItemOpts, Staged};
use crate::import::legacy_model::{Collection, LegacyRecord, read_envelope};
use crate::import::subs::SubOpts;
use crate::ops::{Durability, WriteOp};
use crate::{Store, meta};

pub use crate::import::canonical::{canonical_key, normalize_url};
pub use crate::import::report::{
    FileReport, ImportError, ImportErrorCode, ImportReport, REPORTED_STATUSES, Warning, WarningCode,
};

/// The marker file written into `STATE_DIR` on a successful import (DESIGN §7.6.6).
///
/// It holds the whole report, so a second start with a **deleted** database does not silently
/// re-import stale JSON. `--force` overrides it.
pub const MARKER_FILE: &str = ".aulos-imported";

/// `cookies.txt` in `STATE_DIR`, registered as the `cookiefile` runtime override (DESIGN §17.2).
pub const COOKIES_FILE: &str = "cookies.txt";

/// The `kv` key the cookie file is registered under.
pub const COOKIEFILE_KEY: &str = "cookiefile";

/// What a **file** error does (`AULOS_IMPORT_ON_ERROR`, DESIGN §7.6.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum OnError {
    /// Roll back, delete the database, exit non-zero with the report printed. The default: during
    /// a cutover the operator is watching and the legacy files are untouched, so the right answer
    /// is "read the report, fix it, retry".
    #[default]
    Fail,
    /// Import everything else, downgrade the error to a `file_skipped` warning, and leave the
    /// importer health component `degraded` for the life of the process.
    Skip,
}

impl From<aulos_core::config::ImportOnError> for OnError {
    fn from(v: aulos_core::config::ImportOnError) -> Self {
        match v {
            aulos_core::config::ImportOnError::Fail => Self::Fail,
            aulos_core::config::ImportOnError::Skip => Self::Skip,
        }
    }
}

/// How to run the import.
#[derive(Clone, Copy, Debug)]
pub struct ImportOpts {
    /// Run everything, report it, and write no marker file. The caller points the [`Store`] at a
    /// throwaway database (DESIGN §7.6.6, §19.2).
    pub dry_run: bool,
    /// Import even though the destination already holds imported state.
    pub force: bool,
    /// What a file error does.
    pub on_error: OnError,
    /// `CLEAR_COMPLETED_AFTER`, in seconds, applied to terminal rows using `finished_at`.
    pub clear_completed_after_s: u64,
    /// `SUBSCRIPTION_MAX_SEEN_IDS`.
    pub max_seen_ids: u32,
}

impl Default for ImportOpts {
    /// The production defaults: a real run, no force, `fail`, no clear timer, legacy's seen cap.
    fn default() -> Self {
        Self {
            dry_run: false,
            force: false,
            on_error: OnError::Fail,
            clear_completed_after_s: 0,
            max_seen_ids: 50_000,
        }
    }
}

/// The import was rolled back and nothing was written.
///
/// It always carries the report, because the report *is* the error message an operator acts on
/// (DESIGN §7.6.6).
#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub struct ImportFatal {
    /// Which fatal class this was.
    pub code: ImportErrorCode,
    /// One line, suitable for a log or a CLI stderr line.
    pub reason: Box<str>,
    /// The report as it stood when the import gave up.
    pub report: Box<ImportReport>,
}

impl ImportFatal {
    /// Whether the caller should delete the destination database file.
    ///
    /// True for every fatal class **except** [`ImportErrorCode::AlreadyImported`]: refusing to
    /// re-import must never destroy the database that made us refuse.
    #[must_use]
    pub const fn should_delete_db(&self) -> bool {
        !matches!(self.code, ImportErrorCode::AlreadyImported)
    }

    fn new(code: ImportErrorCode, reason: impl Into<Box<str>>, mut report: ImportReport) -> Self {
        let reason = reason.into();
        report.errors.push(ImportError::new(code, reason.clone()));
        Self {
            code,
            reason,
            report: Box::new(report),
        }
    }
}

/// Removes a SQLite database and its WAL sidecars (DESIGN §7.6.6: "rollback, **delete the DB
/// file**").
///
/// Best-effort per file — a missing file is success — because the caller is already on an error
/// path and a leftover `-shm` must not mask the real failure.
///
/// # Errors
/// The first `io::Error` that is not `NotFound`.
pub fn delete_db_files(path: &Path) -> std::io::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut p = path.to_path_buf().into_os_string();
        p.push(suffix);
        match std::fs::remove_file(PathBuf::from(p)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Imports `STATE_DIR` into `store` in one transaction (DESIGN §7.6).
///
/// # Errors
/// [`ImportFatal`] for the fatal classes of DESIGN §7.6.1, and for a *file* error under
/// [`OnError::Fail`]. Nothing has been written when this returns `Err`; the caller deletes the
/// database when [`ImportFatal::should_delete_db`] says so.
#[allow(clippy::too_many_lines)] // one block per legacy file; splitting it would hide the order
pub async fn import(
    state_dir: &Path,
    store: &Store,
    opts: ImportOpts,
) -> Result<ImportReport, ImportFatal> {
    let now = now_ms();
    let mut report = ImportReport::new(state_dir.to_path_buf(), now);

    // --- 0. the state dir itself --------------------------------------------------------------
    match std::fs::metadata(state_dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            return Err(ImportFatal::new(
                ImportErrorCode::StateDirUnreadable,
                format!("{} is not a directory", state_dir.display()),
                report,
            ));
        }
        Err(e) => {
            return Err(ImportFatal::new(
                ImportErrorCode::StateDirUnreadable,
                format!("{} cannot be read: {e}", state_dir.display()),
                report,
            ));
        }
    }

    // --- 1. idempotence -----------------------------------------------------------------------
    if !opts.force
        && let Some(reason) = already_imported(state_dir, store).await?
    {
        return Err(ImportFatal::new(
            ImportErrorCode::AlreadyImported,
            format!("{reason}; pass --force to import anyway"),
            report,
        ));
    }

    // --- 2. pickle shelves --------------------------------------------------------------------
    // Legacy did **not** delete the shelf after migrating it to JSON, so a real STATE_DIR usually
    // holds both. A shelf next to a readable JSON file is therefore just noise (a warning); a
    // shelf with no JSON counterpart is state we cannot read at all, and that is fatal.
    for (shelf, json) in shelf_candidates() {
        let Some(found) = find_shelf(state_dir, shelf) else {
            continue;
        };
        if state_dir.join(json).is_file() {
            report.warnings.push(Warning::new(
                WarningCode::ShelfIgnored,
                format!(
                    "legacy shelf found at {}; {json} is present and is used instead",
                    found.display()
                ),
            ));
        } else {
            return Err(ImportFatal::new(
                ImportErrorCode::ShelfPresent,
                format!(
                    "legacy shelf found at {}; start the Python image once so it migrates to \
                     JSON, then re-run the import.",
                    found.display()
                ),
                report,
            ));
        }
    }

    // --- 3. the three item collections --------------------------------------------------------
    let mut staged: Vec<Staged> = Vec::new();
    let mut counts: HashMap<Collection, FileCounts> = HashMap::new();

    for collection in Collection::ALL {
        let mut file = FileReport {
            file: collection.file().into(),
            schema_version: None,
            records: 0,
            imported: 0,
            skipped: 0,
        };
        match read_file(state_dir, collection.file()) {
            FileRead::Absent => {}
            FileRead::Invalid(reason) => {
                record_file_error(&mut report, collection.file(), &reason, opts.on_error);
            }
            FileRead::Text(text) => match read_envelope(&text, collection.kind()) {
                Err(bad) => {
                    record_file_error(&mut report, collection.file(), &bad.0, opts.on_error);
                }
                Ok(env) => {
                    file.schema_version = Some(env.schema_version);
                    file.records = env.items.len() as u64;
                    for (index, element) in env.items.iter().enumerate() {
                        match stage(element, collection, index) {
                            Ok(s) => staged.push(s),
                            Err(why) => {
                                file.skipped += 1;
                                report.warnings.push(Warning::new(
                                    WarningCode::RecordSkipped,
                                    format!("{}[{index}] {why}", collection.file()),
                                ));
                            }
                        }
                    }
                }
            },
        }
        counts.insert(
            collection,
            FileCounts {
                index: report.files.len(),
            },
        );
        report.files.push(file);
    }

    // --- 4. global ordering and duplicate-url resolution --------------------------------------
    staged.sort_by_key(|s| s.sort_key);
    let kept = resolve_duplicates(staged, &mut report, &counts);

    // --- 5. rows ------------------------------------------------------------------------------
    let item_opts = ItemOpts {
        clear_completed_after_s: opts.clear_completed_after_s,
        fallback_ms: now,
    };
    let mut rows: Vec<Item> = Vec::with_capacity(kept.len());
    for s in kept {
        let ord = store.next_ord();
        match items::build(&s, ord, item_opts) {
            Ok(built) => {
                report.warnings.extend(built.warnings);
                report.count_item(built.item.status);
                bump(&mut report, &counts, s.collection, true);
                rows.push(built.item);
            }
            Err(why) => {
                bump(&mut report, &counts, s.collection, false);
                report.warnings.push(Warning::new(
                    WarningCode::RecordSkipped,
                    format!("{}[{}] {why}", s.collection.file(), s.index),
                ));
            }
        }
    }

    let mut ops: Vec<WriteOp> = Vec::new();
    if !rows.is_empty() {
        ops.push(WriteOp::InsertItems { items: rows });
    }

    // --- 6. subscriptions ---------------------------------------------------------------------
    let mut file = FileReport {
        file: subs::FILE.into(),
        schema_version: None,
        records: 0,
        imported: 0,
        skipped: 0,
    };
    let sub_opts = SubOpts {
        max_seen_ids: opts.max_seen_ids,
        now_ms: now,
    };
    match read_file(state_dir, subs::FILE) {
        FileRead::Absent => {}
        FileRead::Invalid(reason) => {
            record_file_error(&mut report, subs::FILE, &reason, opts.on_error);
        }
        FileRead::Text(text) => match read_envelope(&text, subs::KIND) {
            Err(bad) => record_file_error(&mut report, subs::FILE, &bad.0, opts.on_error),
            Ok(env) => {
                file.schema_version = Some(env.schema_version);
                file.records = env.items.len() as u64;
                let mut urls: Vec<Box<str>> = Vec::new();
                for (index, element) in env.items.iter().enumerate() {
                    match subs::build(element, index, sub_opts) {
                        Ok(built) => {
                            let url: Box<str> = built.record.url.as_str().into();
                            if urls.contains(&url) {
                                file.skipped += 1;
                                report.warnings.push(Warning::new(
                                    WarningCode::DuplicateUrl,
                                    format!(
                                        "{url} appears twice in {}; kept the first",
                                        subs::FILE
                                    ),
                                ));
                                continue;
                            }
                            urls.push(url);
                            file.imported += 1;
                            report.warnings.extend(built.warnings);
                            report.seen_ids_imported += built.seen.len() as u64;
                            let sub_id = built.record.id.clone();
                            ops.push(WriteOp::UpsertSubscription(Box::new(built.record)));
                            if !built.seen.is_empty() {
                                ops.push(WriteOp::MarkSeen {
                                    sub: sub_id,
                                    ids: built.seen,
                                    at: built.seen_at,
                                });
                            }
                        }
                        Err(why) => {
                            file.skipped += 1;
                            report.warnings.push(Warning::new(
                                WarningCode::RecordSkipped,
                                format!("{}[{index}] {why}", subs::FILE),
                            ));
                        }
                    }
                }
            }
        },
    }
    report.files.push(file);

    // --- 7. telegram chats --------------------------------------------------------------------
    let mut file = FileReport {
        file: chats::FILE.into(),
        // The file is a bare object: there is no envelope and therefore no version.
        schema_version: None,
        records: 0,
        imported: 0,
        skipped: 0,
    };
    match read_file(state_dir, chats::FILE) {
        FileRead::Absent => {}
        FileRead::Invalid(reason) => {
            record_file_error(&mut report, chats::FILE, &reason, opts.on_error);
        }
        FileRead::Text(text) => match chats::read_object(&text) {
            Err(reason) => record_file_error(&mut report, chats::FILE, &reason, opts.on_error),
            Ok(obj) => {
                file.records = obj.len() as u64;
                for (key, value) in &obj {
                    match chats::build(key, value) {
                        Ok(built) => {
                            file.imported += 1;
                            ops.push(WriteOp::UpsertTelegramChat {
                                chat_id: built.chat_id,
                                config: built.config,
                            });
                        }
                        Err(why) => {
                            file.skipped += 1;
                            report.warnings.push(Warning::new(
                                WarningCode::RecordSkipped,
                                format!("{}[{key}] {why}", chats::FILE),
                            ));
                        }
                    }
                }
            }
        },
    }
    report.files.push(file);

    // --- 8. cookies.txt -----------------------------------------------------------------------
    let cookies = state_dir.join(COOKIES_FILE);
    if cookies.is_file() {
        ops.push(WriteOp::SetKv {
            key: COOKIEFILE_KEY.into(),
            value: Some(Value::String(cookies.display().to_string())),
        });
    }

    // --- 9. the policy gate -------------------------------------------------------------------
    // Every error still in the report at this point is one the policy chose not to downgrade, so
    // nothing is written and the caller deletes the database.
    if !report.errors.is_empty() {
        let reason = report
            .errors
            .iter()
            .map(|e| format!("{}: {}", e.code, e.detail))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ImportFatal {
            code: ImportErrorCode::FileInvalid,
            reason: reason.into_boxed_str(),
            report: Box::new(report),
        });
    }

    // --- 10. provenance, then one transaction -------------------------------------------------
    let json = report
        .to_json()
        .unwrap_or_else(|e| format!("{{\"error\":\"report could not be serialised: {e}\"}}"));
    ops.push(meta::set(
        meta::IMPORTED_FROM,
        state_dir.display().to_string(),
    ));
    ops.push(meta::set(meta::IMPORTED_AT, now.to_string()));
    ops.push(meta::set(meta::IMPORT_REPORT, json.clone()));

    if let Err(e) = store.write(ops, Durability::Sync).await {
        return Err(ImportFatal::new(
            ImportErrorCode::DbWriteFailed,
            format!("the destination database rejected the import: {e}"),
            report,
        ));
    }

    // --- 11. the marker -----------------------------------------------------------------------
    // The only file the importer ever writes, and never in a dry run (T2).
    if !opts.dry_run {
        let marker = state_dir.join(MARKER_FILE);
        if let Err(e) = std::fs::write(&marker, &json) {
            // Not fatal: the database is committed and correct. It only means a later boot with a
            // deleted database could re-import, which `meta.imported_at` also guards.
            tracing::warn!(path = %marker.display(), error = %e,
                           "could not write the import marker file");
        }
    }

    tracing::info!(
        items = report.items_total(),
        warnings = report.warnings.len(),
        seen_ids = report.seen_ids_imported,
        dry_run = opts.dry_run,
        "legacy import complete\n{}",
        report.render_table()
    );
    Ok(report)
}

/// Where a file report lives in [`ImportReport::files`], so the per-record counters can be bumped
/// after the global ordering pass has mixed the collections together.
struct FileCounts {
    index: usize,
}

/// Bumps one collection's `imported`/`skipped` counter.
fn bump(
    report: &mut ImportReport,
    counts: &HashMap<Collection, FileCounts>,
    collection: Collection,
    imported: bool,
) {
    if let Some(c) = counts.get(&collection)
        && let Some(f) = report.files.get_mut(c.index)
    {
        if imported {
            f.imported += 1;
        } else {
            f.skipped += 1;
        }
    }
}

/// Whether the destination already holds imported state, and why we think so.
async fn already_imported(state_dir: &Path, store: &Store) -> Result<Option<String>, ImportFatal> {
    let marker = state_dir.join(MARKER_FILE);
    if marker.is_file() {
        return Ok(Some(format!("{} exists", marker.display())));
    }
    let meta = store.meta().await.map_err(|e| {
        ImportFatal::new(
            ImportErrorCode::DbWriteFailed,
            format!("the destination database could not be read: {e}"),
            ImportReport::new(state_dir.to_path_buf(), now_ms()),
        )
    })?;
    if let Some(at) = meta.get(meta::IMPORTED_AT) {
        return Ok(Some(format!("the database was already imported at {at}")));
    }
    let rows = store
        .read(|c| {
            c.query_row("SELECT COUNT(*) FROM items", [], |r| r.get::<_, i64>(0))
                .map_err(StoreError::from)
        })
        .await
        .map_err(|e| {
            ImportFatal::new(
                ImportErrorCode::DbWriteFailed,
                format!("the destination database could not be read: {e}"),
                ImportReport::new(state_dir.to_path_buf(), now_ms()),
            )
        })?;
    if rows > 0 {
        return Ok(Some(format!("the database already holds {rows} items")));
    }
    Ok(None)
}

/// The `(shelf, json)` pairs the pickle check looks at.
fn shelf_candidates() -> Vec<(&'static str, &'static str)> {
    let mut v: Vec<(&'static str, &'static str)> = Collection::ALL
        .iter()
        .map(|c| (c.shelf(), c.file()))
        .collect();
    v.push((subs::SHELF, subs::FILE));
    v
}

/// Finds a Python `shelve` database named `stem`.
///
/// `dbm.gnu` writes the bare name, `dbm.ndbm` adds `.db` and `dbm.dumb` writes `.dir`/`.dat`/
/// `.bak`, and which one a Python build picked is not knowable from here — so all five spellings
/// count.
fn find_shelf(dir: &Path, stem: &str) -> Option<PathBuf> {
    for suffix in ["", ".db", ".dat", ".dir", ".bak"] {
        let p = dir.join(format!("{stem}{suffix}"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// The three outcomes of looking for a legacy file.
enum FileRead {
    /// Not there — normal, and not an error.
    Absent,
    /// There, and readable.
    Text(String),
    /// There, and not readable as UTF-8 text. A *file* error.
    Invalid(Box<str>),
}

/// Reads one legacy file without ever writing to it.
fn read_file(dir: &Path, name: &str) -> FileRead {
    let path = dir.join(name);
    match std::fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileRead::Absent,
        Err(e) => FileRead::Invalid(format!("cannot be read: {e}").into_boxed_str()),
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => FileRead::Text(text),
            Err(e) => FileRead::Invalid(format!("is not valid UTF-8: {e}").into_boxed_str()),
        },
    }
}

/// Applies the file-error policy of DESIGN §7.6.1.
fn record_file_error(report: &mut ImportReport, file: &str, reason: &str, on_error: OnError) {
    match on_error {
        OnError::Fail => {
            tracing::error!(
                file,
                reason,
                "legacy file is invalid; the import will roll back"
            );
            report.errors.push(ImportError::new(
                ImportErrorCode::FileInvalid,
                format!("{file}: {reason}"),
            ));
        }
        OnError::Skip => {
            tracing::warn!(
                file,
                reason,
                "legacy file is invalid; skipping it (AULOS_IMPORT_ON_ERROR=skip)"
            );
            report.warnings.push(Warning::new(
                WarningCode::FileSkipped,
                format!("{file}: {reason}"),
            ));
        }
    }
}

/// Stages one `{"key": …, "info": {…}}` element.
fn stage(element: &Value, collection: Collection, index: usize) -> Result<Staged, Box<str>> {
    let info = element
        .as_object()
        .and_then(|o| o.get("info"))
        .ok_or_else(|| Box::<str>::from("the element has no info object"))?;
    let record = LegacyRecord::from_json(info)?;
    // DESIGN §7.6.2 step 1: ordered by the legacy timestamp, globally across
    // completed → pending → queue, with file order as the tie-break (and as the whole key for a
    // record that never had a timestamp).
    let sort_key = (record.timestamp_ms.unwrap_or(0), collection as u8, index);
    Ok(Staged {
        record,
        collection,
        index,
        sort_key,
    })
}

/// Keeps the most advanced record per legacy `url` (DESIGN §7.6.2 step 6).
///
/// Legacy deduped only within `queue`, so the same URL really can appear in two files; the terminal
/// record wins, then an active one, then a pending one, and the discarded record is reported.
fn resolve_duplicates(
    staged: Vec<Staged>,
    report: &mut ImportReport,
    counts: &HashMap<Collection, FileCounts>,
) -> Vec<Staged> {
    let mut kept: Vec<Staged> = Vec::with_capacity(staged.len());
    // url → its position in `kept`. A scan per candidate made a `completed.json` of a few thousand
    // rows cost tens of millions of string comparisons while the importer holds the boot
    // (DESIGN §19.3); the index is order-preserving and picks the same winner.
    let mut by_url: HashMap<Box<str>, usize> = HashMap::with_capacity(staged.len());
    for candidate in staged {
        let existing = by_url.get(&candidate.record.url).copied();
        match existing {
            None => {
                by_url.insert(candidate.record.url.clone(), kept.len());
                kept.push(candidate);
            }
            Some(i) => {
                let (winner, loser) = if candidate.advancement() > kept[i].advancement() {
                    let loser = std::mem::replace(&mut kept[i], candidate);
                    (&kept[i], loser)
                } else {
                    (&kept[i], candidate)
                };
                report.warnings.push(Warning::new(
                    WarningCode::DuplicateUrl,
                    format!(
                        "{} in {} and {}; kept {}",
                        winner.record.url,
                        loser.collection.file(),
                        winner.collection.file(),
                        winner.imported_status().as_str()
                    ),
                ));
                bump(report, counts, loser.collection, false);
            }
        }
    }
    kept
}

/// The report a boot needs to hand `healthz`, read back from `meta` (DESIGN §16.3).
///
/// `None` when this database has never been imported into.
///
/// # Errors
/// [`StoreError`] when `meta` cannot be read.
pub async fn stored_report(store: &Store) -> Result<Option<ImportReport>, StoreError> {
    let meta = store.meta().await?;
    let Some(raw) = meta.get(meta::IMPORT_REPORT) else {
        return Ok(None);
    };
    Ok(serde_json::from_str(raw).ok())
}

/// The ids the last import wrote, for a test or a `doctor` run that wants to see them.
///
/// # Errors
/// [`StoreError`] when the read pool is unreachable.
pub async fn imported_ids(store: &Store) -> Result<Vec<ItemId>, StoreError> {
    store
        .read(|c| {
            let mut stmt = c.prepare("SELECT id FROM items ORDER BY ord ASC")?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let raw: String = row.get(0)?;
                out.push(
                    raw.parse::<ItemId>()
                        .map_err(|e| StoreError::decode("items.id", e))?,
                );
            }
            Ok(out)
        })
        .await
}

/// How many rows of each status the database holds, for the acceptance tests.
///
/// # Errors
/// [`StoreError`] when the read pool is unreachable.
pub async fn status_counts(store: &Store) -> Result<HashMap<Status, u64>, StoreError> {
    store
        .read(|c| {
            let mut stmt = c.prepare("SELECT status, COUNT(*) FROM items GROUP BY status")?;
            let mut rows = stmt.query([])?;
            let mut out = HashMap::new();
            while let Some(row) = rows.next()? {
                let raw: String = row.get(0)?;
                let n: i64 = row.get(1)?;
                out.insert(crate::json::status_from_str(&raw)?, n.unsigned_abs());
            }
            Ok(out)
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_policy_is_fail() {
        assert_eq!(OnError::default(), OnError::Fail);
        assert_eq!(
            OnError::from(aulos_core::config::ImportOnError::Skip),
            OnError::Skip
        );
        assert_eq!(
            OnError::from(aulos_core::config::ImportOnError::Fail),
            OnError::Fail
        );
        let d = ImportOpts::default();
        assert!(!d.dry_run && !d.force);
        assert_eq!(d.on_error, OnError::Fail);
        assert_eq!(d.max_seen_ids, 50_000);
    }

    #[test]
    fn refusing_to_reimport_never_deletes_the_database() {
        let report = ImportReport::new(PathBuf::from("/x"), 0);
        let fatal = ImportFatal::new(ImportErrorCode::AlreadyImported, "already", report);
        assert!(!fatal.should_delete_db());
        assert_eq!(fatal.report.errors.len(), 1);

        let report = ImportReport::new(PathBuf::from("/x"), 0);
        for code in [
            ImportErrorCode::FileInvalid,
            ImportErrorCode::ShelfPresent,
            ImportErrorCode::StateDirUnreadable,
            ImportErrorCode::DbWriteFailed,
        ] {
            let fatal = ImportFatal::new(code, "nope", report.clone());
            assert!(fatal.should_delete_db(), "{code}");
        }
    }

    #[test]
    fn deleting_a_missing_database_is_success() {
        let dir = std::env::temp_dir().join(format!("aulos-import-{}", ulid::Ulid::new()));
        assert!(delete_db_files(&dir.join("nope.db")).is_ok());
    }

    #[test]
    fn the_shelf_candidates_cover_all_four_collections() {
        let names: Vec<&str> = shelf_candidates().iter().map(|(s, _)| *s).collect();
        assert_eq!(names, ["completed", "pending", "queue", "subscriptions"]);
    }

    #[test]
    fn a_staged_element_needs_an_info_object() {
        let ok = serde_json::json!({"key": "u://x", "info": {"url": "https://x.test/a"}});
        assert!(stage(&ok, Collection::Queue, 0).is_ok());
        for bad in [
            serde_json::json!({"key": "u://x"}),
            serde_json::json!(["nope"]),
            serde_json::json!({"info": {"title": "no url"}}),
        ] {
            assert!(stage(&bad, Collection::Queue, 0).is_err(), "{bad}");
        }
    }

    /// Duplicate resolution must be linear in the number of staged rows.
    ///
    /// A scan of `kept` per candidate cost tens of millions of URL comparisons on a real
    /// `completed.json`, and the importer holds the boot while it runs (DESIGN §19.3). The bound is
    /// loose on purpose: indexed it is milliseconds, quadratic it is many seconds in a debug build.
    #[test]
    fn duplicate_resolution_scales_linearly_and_still_keeps_the_winner() {
        let mut staged: Vec<Staged> = (0..50_000)
            .map(|n| {
                let element = serde_json::json!({
                    "key": format!("u{n}"),
                    "info": {"url": format!("https://x.test/{n}"), "status": "pending"},
                });
                stage(&element, Collection::Queue, n).expect("fixture")
            })
            .collect();
        // One url that really is duplicated, from the file whose record is more advanced.
        let dup = serde_json::json!({
            "key": "u0",
            "info": {"url": "https://x.test/0", "status": "finished"},
        });
        staged.push(stage(&dup, Collection::Completed, 0).expect("fixture"));

        let mut report = ImportReport::new(PathBuf::from("/x"), 0);
        let started = std::time::Instant::now();
        let kept = resolve_duplicates(staged, &mut report, &HashMap::new());
        let elapsed = started.elapsed();

        assert_eq!(kept.len(), 50_000, "only the one duplicate is collapsed");
        assert_eq!(kept[0].collection, Collection::Completed, "terminal wins");
        assert_eq!(report.warnings.len(), 1);
        assert_eq!(report.warnings[0].code, WarningCode::DuplicateUrl);
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "resolving 50 001 staged rows took {elapsed:?}; the scan is quadratic again"
        );
    }

    #[test]
    fn the_file_error_policy_routes_to_errors_or_warnings() {
        let mut r = ImportReport::new(PathBuf::from("/x"), 0);
        record_file_error(&mut r, "queue.json", "not valid JSON", OnError::Fail);
        assert_eq!(r.errors.len(), 1);
        assert_eq!(r.errors[0].code, ImportErrorCode::FileInvalid);
        assert!(r.warnings.is_empty());

        let mut r = ImportReport::new(PathBuf::from("/x"), 0);
        record_file_error(&mut r, "queue.json", "not valid JSON", OnError::Skip);
        assert!(r.errors.is_empty());
        assert_eq!(r.warnings.len(), 1);
        assert_eq!(r.warnings[0].code, WarningCode::FileSkipped);
        assert!(r.is_degraded());
        assert_eq!(r.skipped_files(), vec!["queue.json: not valid JSON"]);
    }
}
