//! A scripted in-process [`ScHttp`] for the unit tests.
//!
//! Every scrape step is tested against checked-in HTML/JSON captured from the real site rather
//! than against a live host, so the suite is deterministic, offline and fast. The mock also counts
//! requests per URL, which is what turns "a season resolves in 2 requests" and "the m3u8 is never
//! fetched" from prose into assertions.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use url::Url;

use crate::error::ScError;
use crate::http::{ScHttp, ScReq, ScRes};

/// One scripted URL: a queue of responses, the last of which repeats forever.
#[derive(Clone, Debug)]
struct Scripted {
    responses: Vec<(u16, String)>,
}

/// The recorded facts about one request.
#[derive(Clone, Debug)]
struct Recorded {
    headers: Vec<(String, String)>,
}

/// A scripted `ScHttp`.
#[derive(Debug, Default)]
pub struct MockHttp {
    routes: Mutex<HashMap<String, Scripted>>,
    calls: Mutex<Vec<(String, Recorded)>>,
    cookies: String,
}

impl MockHttp {
    /// An empty mock: every request is a 404 with an empty body.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares the `Cookie:` header value the mock hands out.
    #[must_use]
    pub fn with_cookies(mut self, cookies: &str) -> Self {
        self.cookies = cookies.to_owned();
        self
    }

    /// Scripts one URL with a single response that repeats.
    #[must_use]
    pub fn on(self, url: &str, status: u16, body: &str) -> Self {
        self.on_sequence(url, vec![(status, body.to_owned())])
    }

    /// Scripts one URL with a sequence of responses; the last repeats.
    #[must_use]
    pub fn on_sequence(self, url: &str, responses: Vec<(u16, String)>) -> Self {
        self.routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(url.to_owned(), Scripted { responses });
        self
    }

    /// How many times `url` was requested.
    #[must_use]
    pub fn count(&self, url: &str) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(u, _)| u == url)
            .count()
    }

    /// Every URL requested, in order.
    #[must_use]
    pub fn urls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(u, _)| u.clone())
            .collect()
    }

    /// The total number of requests.
    #[must_use]
    pub fn total(&self) -> usize {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The headers sent with the first request to `url`.
    #[must_use]
    pub fn headers_for(&self, url: &str) -> Vec<(String, String)> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(u, _)| u == url)
            .map(|(_, r)| r.headers.clone())
            .unwrap_or_default()
    }

    /// Whether any request URL contains `needle`.
    #[must_use]
    pub fn requested_anything_containing(&self, needle: &str) -> bool {
        self.urls().iter().any(|u| u.contains(needle))
    }
}

#[async_trait]
impl ScHttp for MockHttp {
    async fn get(&self, req: ScReq) -> Result<ScRes, ScError> {
        let key = req.url.to_string();
        let seen = self.count(&key);
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((
                key.clone(),
                Recorded {
                    headers: req
                        .headers
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                },
            ));
        let scripted = self
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .cloned();
        let (status, body) = match scripted {
            Some(s) => {
                let idx = seen.min(s.responses.len().saturating_sub(1));
                s.responses
                    .get(idx)
                    .cloned()
                    .unwrap_or((404, String::new()))
            }
            None => (404, String::new()),
        };
        Ok(ScRes {
            status,
            url: Url::parse(&key).unwrap_or(req.url),
            body,
        })
    }

    fn impersonating(&self) -> bool {
        // The plain client is what the whole test matrix exercises (DESIGN §10.1: there is no
        // BoringSSL in CI), so the mock reports the same thing it does.
        false
    }

    fn cookie_header(&self) -> String {
        self.cookies.clone()
    }
}
