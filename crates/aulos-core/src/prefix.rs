//! `URL_PREFIX` normalisation (DESIGN §17.1 step 4).
//!
//! Legacy only appended the trailing `/`, so `URL_PREFIX=metube` produced routes like
//! `metubeadd` and a container `HEALTHCHECK` that fetched `…:8081metubehealthz`. [`Prefix`] is the
//! **only** type in the process allowed to build a path, which makes prefix drift structurally
//! impossible.

use std::fmt;

use serde::Serialize;

/// A normalised URL prefix: always starts and ends with `/`. `""` becomes `"/"`.
#[derive(Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Prefix(Box<str>);

/// What [`Prefix::normalize`] had to fix, so the caller can log it at WARN.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrefixFixup {
    /// A missing leading `/` was added. Legacy did not do this; DESIGN §17.1 logs a WARN.
    AddedLeadingSlash,
    /// A missing trailing `/` was added. Legacy did this too, silently.
    AddedTrailingSlash,
}

impl Prefix {
    /// The root prefix, `"/"`.
    #[must_use]
    pub fn root() -> Self {
        Self("/".into())
    }

    /// Normalises a raw `URL_PREFIX` value, reporting every fixup applied.
    ///
    /// `""` → `/`; `metube` → `/metube/` (both fixups); `/metube` → `/metube/`; `/metube/`
    /// unchanged. Repeated inner slashes are collapsed so `//a//b//` normalises to `/a/b/`.
    #[must_use]
    pub fn normalize(raw: &str) -> (Self, Vec<PrefixFixup>) {
        let mut fixups = Vec::new();
        let trimmed = raw.trim();

        if !trimmed.is_empty() {
            if !trimmed.starts_with('/') {
                fixups.push(PrefixFixup::AddedLeadingSlash);
            }
            if !trimmed.ends_with('/') {
                fixups.push(PrefixFixup::AddedTrailingSlash);
            }
        }

        let segments: Vec<&str> = trimmed.split('/').filter(|s| !s.is_empty()).collect();
        let normalised = if segments.is_empty() {
            "/".to_owned()
        } else {
            format!("/{}/", segments.join("/"))
        };

        (Self(normalised.into_boxed_str()), fixups)
    }

    /// The prefix, always `/`-delimited on both ends.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is the root prefix.
    #[must_use]
    pub fn is_root(&self) -> bool {
        &*self.0 == "/"
    }

    /// Builds an absolute route path: `prefix.route("api/v2/downloads")` → `/api/v2/downloads`.
    ///
    /// A leading `/` on `suffix` is tolerated and collapsed, so callers need not care.
    #[must_use]
    pub fn route(&self, suffix: &str) -> String {
        format!("{}{}", self.0, suffix.trim_start_matches('/'))
    }
}

impl Default for Prefix {
    fn default() -> Self {
        Self::root()
    }
}

impl fmt::Display for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Prefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Prefix({:?})", &*self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_becomes_root_with_no_warning() {
        let (p, fixups) = Prefix::normalize("");
        assert_eq!(p.as_str(), "/");
        assert!(fixups.is_empty());
        assert!(p.is_root());
    }

    #[test]
    fn missing_leading_slash_is_added_and_reported() {
        let (p, fixups) = Prefix::normalize("metube");
        assert_eq!(p.as_str(), "/metube/");
        assert_eq!(
            fixups,
            vec![
                PrefixFixup::AddedLeadingSlash,
                PrefixFixup::AddedTrailingSlash
            ]
        );
    }

    #[test]
    fn missing_trailing_slash_is_added() {
        let (p, fixups) = Prefix::normalize("/metube");
        assert_eq!(p.as_str(), "/metube/");
        assert_eq!(fixups, vec![PrefixFixup::AddedTrailingSlash]);
    }

    #[test]
    fn a_normalised_prefix_is_left_alone() {
        let (p, fixups) = Prefix::normalize("/metube/");
        assert_eq!(p.as_str(), "/metube/");
        assert!(fixups.is_empty());
    }

    #[test]
    fn inner_slashes_are_collapsed() {
        let (p, _) = Prefix::normalize("//a//b//");
        assert_eq!(p.as_str(), "/a/b/");
        let (root, _) = Prefix::normalize("///");
        assert_eq!(root.as_str(), "/");
    }

    #[test]
    fn route_never_double_slashes() {
        let (p, _) = Prefix::normalize("metube");
        assert_eq!(p.route("healthz"), "/metube/healthz");
        assert_eq!(p.route("/healthz"), "/metube/healthz");
        assert_eq!(Prefix::root().route("api/v2/state"), "/api/v2/state");
    }
}
