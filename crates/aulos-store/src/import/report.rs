//! The import report: what it holds, how it serialises, and how it renders (DESIGN §7.6.6).
//!
//! The report is the only artefact of an import an operator actually reads, and it is consumed by
//! three surfaces — the `INFO` table the CLI prints, the `.aulos-imported` marker file, and
//! `GET <p>api/v2/import-report` — so it is a plain serialisable struct with no behaviour beyond
//! counting and rendering.
//!
//! [`ImportReport::render_table`] deliberately omits `imported_at`: `--dry-run` must print a table
//! **identical** to the real run's (DESIGN §7.6.6, WP-05 acceptance), and a wall-clock line would
//! make that untestable. The timestamp is in the JSON, where a diff can ignore it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use aulos_core::{Status, UnixMs};
use serde::{Deserialize, Serialize};

/// The four statuses an import can produce, always present in `items` so a reader never has to
/// distinguish "zero" from "absent" (DESIGN §7.6.6).
pub const REPORTED_STATUSES: [Status; 4] = [
    Status::Queued,
    Status::Finished,
    Status::Error,
    Status::Canceled,
];

/// One legacy file's contribution.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct FileReport {
    /// The file name, without a directory (`"queue.json"`).
    pub file: Box<str>,
    /// The envelope's `schema_version`, or `null` for `telegram_bot_config.json`, which has no
    /// envelope.
    pub schema_version: Option<u32>,
    /// How many records the file held.
    pub records: u64,
    /// How many became rows.
    pub imported: u64,
    /// How many were skipped — record errors, and duplicates that lost to a more advanced record.
    pub skipped: u64,
}

/// Why a warning was recorded. A warning **never** fails an import (DESIGN §7.6.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningCode {
    /// One array element in a valid envelope could not be used.
    RecordSkipped,
    /// A whole file was dropped under `AULOS_IMPORT_ON_ERROR=skip`. The importer component stays
    /// `degraded` for the life of the process (DESIGN §7.6.1).
    FileSkipped,
    /// The same legacy `url` appeared twice; the most advanced record won.
    DuplicateUrl,
    /// A legacy `status` outside the documented set.
    UnknownStatus,
    /// A StreamingCommunity row whose `title_id`/`episode_id` could not be derived (DESIGN §7.6.3a).
    ScIdsUnresolved,
    /// A legacy `shelve` file was found next to a readable JSON file, so the JSON won.
    ShelfIgnored,
    /// One field of an otherwise usable record was dropped (an uncontainable folder, say).
    FieldDropped,
}

impl WarningCode {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RecordSkipped => "record_skipped",
            Self::FileSkipped => "file_skipped",
            Self::DuplicateUrl => "duplicate_url",
            Self::UnknownStatus => "unknown_status",
            Self::ScIdsUnresolved => "sc_ids_unresolved",
            Self::ShelfIgnored => "shelf_ignored",
            Self::FieldDropped => "field_dropped",
        }
    }
}

impl std::fmt::Display for WarningCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One non-fatal finding.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Warning {
    /// What kind of finding this is.
    pub code: WarningCode,
    /// The human detail, always naming the file (and the index, for a record).
    pub detail: Box<str>,
}

impl Warning {
    /// Builds a warning.
    #[must_use]
    pub fn new(code: WarningCode, detail: impl Into<Box<str>>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// Why an error was recorded. Whether it *fails* the import is the policy's decision, not the
/// code's (DESIGN §7.6.1).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportErrorCode {
    /// Malformed JSON, `schema_version ∉ {1,2}`, the wrong `kind`, no `items` array, or an
    /// unreadable file.
    FileInvalid,
    /// A legacy `shelve` (pickle) database with no JSON counterpart. Always fatal — pickle import
    /// is out of scope (BRIEF).
    ShelfPresent,
    /// `STATE_DIR` is missing or unreadable. Always fatal.
    StateDirUnreadable,
    /// The destination database rejected the import. Always fatal.
    DbWriteFailed,
    /// The database already holds imported state and `--force` was not given.
    AlreadyImported,
}

impl ImportErrorCode {
    /// The wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::FileInvalid => "file_invalid",
            Self::ShelfPresent => "shelf_present",
            Self::StateDirUnreadable => "state_dir_unreadable",
            Self::DbWriteFailed => "db_write_failed",
            Self::AlreadyImported => "already_imported",
        }
    }

    /// Whether this class of error can ever be downgraded by `AULOS_IMPORT_ON_ERROR=skip`.
    ///
    /// Only a *file* error can; everything else is fatal with no policy escape (DESIGN §7.6.1).
    #[must_use]
    pub const fn is_file_error(self) -> bool {
        matches!(self, Self::FileInvalid)
    }
}

impl std::fmt::Display for ImportErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One error finding.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ImportError {
    /// What kind of error this is.
    pub code: ImportErrorCode,
    /// The human detail, naming the file or directory.
    pub detail: Box<str>,
}

impl ImportError {
    /// Builds an error finding.
    #[must_use]
    pub fn new(code: ImportErrorCode, detail: impl Into<Box<str>>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// What an import did (DESIGN §7.6.6).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ImportReport {
    /// When the import ran, unix ms.
    pub imported_at: UnixMs,
    /// The directory the legacy files were read from.
    pub state_dir: PathBuf,
    /// One entry per file the importer looked at, in the documented order.
    pub files: Vec<FileReport>,
    /// Non-fatal findings. A non-empty list never fails a boot.
    pub warnings: Vec<Warning>,
    /// Errors. Whether they fail the import is `AULOS_IMPORT_ON_ERROR`'s decision.
    pub errors: Vec<ImportError>,
    /// How many `subscription_seen` rows were written.
    pub seen_ids_imported: u64,
    /// Rows written, by status. Always carries [`REPORTED_STATUSES`].
    pub items: BTreeMap<Status, u64>,
}

impl ImportReport {
    /// An empty report for `state_dir`, with the four counted statuses seeded at zero.
    #[must_use]
    pub fn new(state_dir: PathBuf, imported_at: UnixMs) -> Self {
        Self {
            imported_at,
            state_dir,
            files: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
            seen_ids_imported: 0,
            items: REPORTED_STATUSES.into_iter().map(|s| (s, 0)).collect(),
        }
    }

    /// Records one imported row against its status.
    pub fn count_item(&mut self, status: Status) {
        *self.items.entry(status).or_insert(0) += 1;
    }

    /// How many rows were written in total.
    #[must_use]
    pub fn items_total(&self) -> u64 {
        self.items.values().sum()
    }

    /// The names of the files that were dropped under `AULOS_IMPORT_ON_ERROR=skip`.
    ///
    /// `healthz.components.importer` is `degraded` for the life of the process when this is not
    /// empty, with these names in `detail` (DESIGN §7.6.1).
    #[must_use]
    pub fn skipped_files(&self) -> Vec<&str> {
        self.warnings
            .iter()
            .filter(|w| w.code == WarningCode::FileSkipped)
            .map(|w| &*w.detail)
            .collect()
    }

    /// Whether the importer component should report `degraded`.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        !self.skipped_files().is_empty()
    }

    /// The report as pretty JSON — what the `.aulos-imported` marker and `meta.import_report`
    /// hold.
    ///
    /// # Errors
    /// Never in practice: every field is a string, a number, a path or a list of them. A
    /// serialisation failure is reported rather than papered over because the marker file is what
    /// stops a second boot re-importing stale JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// The `INFO` table (DESIGN §7.6.6).
    ///
    /// Deterministic: no timestamps, no map iteration order surprises (`items` is a `BTreeMap`
    /// over a `Status` whose `Ord` is its declaration order). That is what lets the acceptance
    /// test assert a `--dry-run` table equals the real run's byte for byte.
    #[must_use]
    pub fn render_table(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::with_capacity(512);
        let _ = writeln!(out, "legacy import from {}", self.state_dir.display());
        let _ = writeln!(
            out,
            "  {:<26} {:>6} {:>8} {:>9} {:>8}",
            "file", "schema", "records", "imported", "skipped"
        );
        for f in &self.files {
            let schema = f
                .schema_version
                .map_or_else(|| "-".to_owned(), |v| v.to_string());
            let _ = writeln!(
                out,
                "  {:<26} {:>6} {:>8} {:>9} {:>8}",
                f.file, schema, f.records, f.imported, f.skipped
            );
        }

        let items = self
            .items
            .iter()
            .map(|(s, n)| format!("{s}={n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let _ = writeln!(out, "  items: {items}");
        let _ = writeln!(out, "  seen ids: {}", self.seen_ids_imported);

        let _ = writeln!(out, "  warnings: {}", self.warnings.len());
        for w in &self.warnings {
            let _ = writeln!(out, "    {}: {}", w.code, w.detail);
        }
        let _ = writeln!(out, "  errors: {}", self.errors.len());
        for e in &self.errors {
            let _ = writeln!(out, "    {}: {}", e.code, e.detail);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> ImportReport {
        let mut r = ImportReport::new(PathBuf::from("/downloads/.metube"), 1_757_000_000_000);
        r.files.push(FileReport {
            file: "queue.json".into(),
            schema_version: Some(2),
            records: 3,
            imported: 3,
            skipped: 0,
        });
        r.files.push(FileReport {
            file: "telegram_bot_config.json".into(),
            schema_version: None,
            records: 2,
            imported: 2,
            skipped: 0,
        });
        r.count_item(Status::Queued);
        r.count_item(Status::Queued);
        r.count_item(Status::Finished);
        r.seen_ids_imported = 3_121;
        r
    }

    #[test]
    fn the_json_shape_matches_the_design_example() {
        let json: serde_json::Value = serde_json::from_str(&report().to_json().unwrap()).unwrap();
        assert_eq!(json["imported_at"], 1_757_000_000_000_i64);
        assert_eq!(json["state_dir"], "/downloads/.metube");
        assert_eq!(json["files"][0]["file"], "queue.json");
        assert_eq!(json["files"][0]["schema_version"], 2);
        assert!(
            json["files"][1]["schema_version"].is_null(),
            "the telegram file has no envelope"
        );
        assert_eq!(json["seen_ids_imported"], 3_121);
        // All four counted statuses are present, zeros included.
        for s in REPORTED_STATUSES {
            assert!(
                json["items"].get(s.as_str()).is_some(),
                "{s} must be reported"
            );
        }
        assert_eq!(json["items"]["queued"], 2);
        assert_eq!(json["items"]["canceled"], 0);
        assert!(json["warnings"].as_array().unwrap().is_empty());
        assert!(json["errors"].as_array().unwrap().is_empty());
    }

    #[test]
    fn the_report_round_trips() {
        let r = report();
        let back: ImportReport = serde_json::from_str(&r.to_json().unwrap()).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn the_table_is_deterministic_and_carries_no_timestamp() {
        let a = report();
        let mut b = report();
        b.imported_at = 999;
        assert_eq!(a.render_table(), b.render_table());
        let table = a.render_table();
        assert!(table.contains("queue.json"), "{table}");
        assert!(table.contains("items: queued=2"), "{table}");
        assert!(table.contains("seen ids: 3121"), "{table}");
        assert!(!table.contains("1757000000000"), "{table}");
    }

    #[test]
    fn a_skipped_file_marks_the_report_degraded() {
        let mut r = report();
        assert!(!r.is_degraded());
        r.warnings
            .push(Warning::new(WarningCode::FileSkipped, "completed.json"));
        assert!(r.is_degraded());
        assert_eq!(r.skipped_files(), vec!["completed.json"]);
        // A record warning is not a degradation.
        r.warnings
            .push(Warning::new(WarningCode::RecordSkipped, "queue.json[2]"));
        assert_eq!(r.skipped_files().len(), 1);
    }

    #[test]
    fn only_a_file_error_can_be_downgraded_by_the_policy() {
        assert!(ImportErrorCode::FileInvalid.is_file_error());
        for fatal in [
            ImportErrorCode::ShelfPresent,
            ImportErrorCode::StateDirUnreadable,
            ImportErrorCode::DbWriteFailed,
            ImportErrorCode::AlreadyImported,
        ] {
            assert!(!fatal.is_file_error(), "{fatal} must stay fatal");
        }
    }

    #[test]
    fn codes_serialise_as_their_documented_strings() {
        for c in [
            WarningCode::RecordSkipped,
            WarningCode::FileSkipped,
            WarningCode::DuplicateUrl,
            WarningCode::UnknownStatus,
            WarningCode::ScIdsUnresolved,
            WarningCode::ShelfIgnored,
            WarningCode::FieldDropped,
        ] {
            assert_eq!(
                serde_json::to_string(&c).unwrap(),
                format!("\"{}\"", c.as_str())
            );
        }
        for c in [
            ImportErrorCode::FileInvalid,
            ImportErrorCode::ShelfPresent,
            ImportErrorCode::StateDirUnreadable,
            ImportErrorCode::DbWriteFailed,
            ImportErrorCode::AlreadyImported,
        ] {
            assert_eq!(
                serde_json::to_string(&c).unwrap(),
                format!("\"{}\"", c.as_str())
            );
        }
    }
}
