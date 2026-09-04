//! Domain vocabulary shared by every other crate: item identity and ordering, the closed status
//! enum, the download request and selection types, the `ItemView` wire shape, the event router,
//! the format/quality catalog, configuration loading, health and reload reports, and the error
//! taxonomy.
//!
//! `aulos-core` depends on nothing else in the workspace and every type that appears in a
//! `DomainEvent` payload lives here (DESIGN §3, §4, §5, §17).
