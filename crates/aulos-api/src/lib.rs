//! The HTTP surface: the v2 REST routes, the WebSocket at `<prefix>ws` with its snapshot/delta
//! protocol, the v1 compatibility shim translating the legacy request and response shapes over the
//! same v2 core, static serving of completed downloads, `healthz`/`livez`, CORS, request tracing
//! and auth.
//!
//! The API is a leaf: only `aulos-server` may depend on it, and it never sees SQL (DESIGN §3 rules
//! A3/A4, §11, §14, §16.3).
