//! Executing a community `[[hook]]` (DESIGN §13.4, BRIEF §13).
//!
//! WP-10 owns the schema, its validation and every clamp; this module owns nothing but execution:
//! render the templates, make the request or spawn the command, retry, count. That split is why a
//! plugin author's mistake is a load-time error with a key name rather than a runtime surprise.
//!
//! The debounce is the dispatcher's, shared with [`crate::jellyfin`], so `{count}`,
//! `{titles_json}` and `{filenames_json}` mean the same thing for a Plex refresh as for the
//! built-in.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aulos_core::item::ItemView;
use aulos_core::status::TerminalStatus;
use aulos_provider::command::template::json_escape;
use aulos_provider::command::{Escape, HookAction, HookSpec, HttpMethod, TemplateCtx, render_argv};
use aulos_provider::proc::{EnvPolicy, SpawnSpec};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::error::HookError;
use crate::ffprobe;
use crate::hook::{Debounce, Hook, HookCtx, HookHealth, SkipReason};

/// The `tool` label a spawned community hook reports as, matching the `command` provider's.
pub const TOOL: &str = "plugin";

/// The backoff between attempts (DESIGN §13.4: "exponential 2 s / 8 s").
pub const BACKOFF: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(8)];

/// One validated `[[hook]]` table, ready to run.
#[derive(Debug)]
pub struct ManifestHook {
    spec: HookSpec,
    client: reqwest::Client,
    plugin_dir: PathBuf,
    body_is_json: bool,
}

impl ManifestHook {
    /// Wraps one [`HookSpec`]. `plugins_dir` is `AULOS_PLUGINS_DIR`, from which the hook's own
    /// directory is recovered (its id is `hook:<dir>/<local id>`) for the default `cwd`.
    #[must_use]
    pub fn new(spec: HookSpec, plugins_dir: &std::path::Path) -> Self {
        let plugin_dir =
            dir_of(&spec.id).map_or_else(|| plugins_dir.to_path_buf(), |d| plugins_dir.join(d));
        let body_is_json = body_looks_like_json(&spec);
        Self {
            spec,
            client: reqwest::Client::new(),
            plugin_dir,
            body_is_json,
        }
    }

    /// The spec this hook executes.
    #[must_use]
    pub const fn spec(&self) -> &HookSpec {
        &self.spec
    }

    /// Whether the `http.body` template parses as JSON, which is what decides between
    /// JSON-escaping and raw insertion for `body` and `headers` (DESIGN §13.4).
    #[must_use]
    pub const fn body_is_json(&self) -> bool {
        self.body_is_json
    }

    /// The context `body` and `headers` render from.
    ///
    /// DESIGN §13.4: a placeholder inside a `body` or a header "is JSON-escaped when the body
    /// parses as JSON, otherwise inserted raw". Applied through the *context* rather than through
    /// [`Escape::Json`] on the whole template, because three of the hook tokens are not strings:
    /// `{count}` is a number and `{titles_json}` / `{filenames_json}` are JSON arrays, and
    /// escaping those would turn `\"titles\": {titles_json}` into a syntax error. So the string
    /// fields are escaped and the structural ones are left alone.
    fn value_ctx(&self, tctx: &TemplateCtx) -> TemplateCtx {
        if !self.body_is_json {
            return tctx.clone();
        }
        let esc = |s: &String| json_escape(s);
        let mut out = tctx.clone();
        out.title = esc(&tctx.title);
        out.filename = esc(&tctx.filename);
        out.folder = esc(&tctx.folder);
        out.download_url = esc(&tctx.download_url);
        out.error_code = esc(&tctx.error_code);
        out.error_message = esc(&tctx.error_message);
        out.provider = esc(&tctx.provider);
        out.status = esc(&tctx.status);
        out.format = esc(&tctx.format);
        out.quality = esc(&tctx.quality);
        out.media_id = esc(&tctx.media_id);
        out
    }

    /// DESIGN §13.4's `when.*` allow-lists, evaluated against the wire view.
    ///
    /// Delegates to [`aulos_provider::command::HookFilter::matches_view`], which is the single
    /// implementation of the three axes — including "an item with no provider fails a
    /// `when.provider` filter rather than passing it". This crate never holds an `Item`
    /// (see [`crate::hook`]), which is why the view form exists.
    #[must_use]
    pub fn filter_matches(&self, item: &ItemView) -> bool {
        self.spec.when.matches_view(item)
    }

    /// The template context for one invocation (DESIGN §13.4's token table).
    #[must_use]
    pub fn template_ctx(&self, ctx: &HookCtx<'_>) -> TemplateCtx {
        let item = ctx.item;
        let batch = ctx.batch;
        let first = batch.first();
        let error = first.and_then(|b| b.error.as_ref());
        TemplateCtx {
            url: Url::parse(&item.url).ok(),
            title: item.title.to_string(),
            download_type: Some(item.selection.download_type),
            format: item.selection.format.to_string(),
            quality: item.selection.quality.to_string(),
            item_id: item.id.to_string(),
            provider: item.provider.as_deref().unwrap_or_default().to_owned(),
            status: first.map_or_else(
                || item.status.as_str().to_owned(),
                |b| b.status.as_str().to_owned(),
            ),
            filename: item.filename.as_deref().unwrap_or_default().to_owned(),
            folder: item.folder.as_deref().unwrap_or_default().to_owned(),
            download_url: download_url(ctx),
            size: item.size,
            error_code: error
                .map(|e| e.code.as_str().to_owned())
                .unwrap_or_default(),
            error_message: error.map(|e| e.message.to_string()).unwrap_or_default(),
            count: ctx.count(),
            titles: batch.iter().map(|b| b.title.to_string()).collect(),
            filenames: batch
                .iter()
                .map(|b| b.filename.as_deref().unwrap_or_default().to_owned())
                .collect(),
            plugin_dir: self.plugin_dir.clone(),
            ..TemplateCtx::default()
        }
    }

    /// One HTTP attempt.
    async fn http_attempt(
        &self,
        method: HttpMethod,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<(), HookError> {
        let mut request = self
            .client
            .request(reqwest_method(method), url)
            .timeout(Duration::from_millis(self.spec.timeout_ms));
        for (k, v) in headers {
            request = request.header(k.as_str(), v.as_str());
        }
        if !body.is_empty() {
            request = request.body(body.to_owned());
        }
        let response = request
            .send()
            .await
            .map_err(|e| HookError::transport(format!("{} {url}: {e}", method.as_str())))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let text = response.text().await.unwrap_or_default();
        let detail: String = text.chars().take(512).collect();
        Err(HookError::http_status(
            format!(
                "{} {url} returned HTTP {}: {}",
                method.as_str(),
                status.as_u16(),
                detail.trim()
            ),
            status.as_u16(),
        ))
    }

    /// One command attempt.
    async fn command_attempt(
        &self,
        argv: &[String],
        cwd: &std::path::Path,
        cancel: &CancellationToken,
    ) -> Result<(), HookError> {
        let Some(program) = argv.first() else {
            return Err(HookError::Template("command: empty argv".into()));
        };
        let spec = SpawnSpec::new(TOOL, program)
            .args(argv.iter().skip(1).cloned())
            .cwd(cwd)
            .env(EnvPolicy {
                clear: true,
                pass: Vec::new(),
                set: Vec::new(),
            })
            .stdout_piped(true);
        let captured = ffprobe::capture(
            &spec,
            Duration::from_millis(self.spec.timeout_ms),
            cancel,
            &mut |_| {},
        )
        .await?;
        if captured.success {
            return Ok(());
        }
        Err(HookError::Tool {
            tool: TOOL,
            detail: format!(
                "exit {}: {}",
                captured
                    .code
                    .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
                captured.stderr.trim()
            )
            .into(),
        })
    }
}

#[async_trait::async_trait]
impl Hook for ManifestHook {
    fn id(&self) -> Arc<str> {
        Arc::clone(&self.spec.id)
    }

    fn ordering(&self) -> i16 {
        self.spec.ordering
    }

    fn debounce(&self) -> Debounce {
        Debounce::capped(
            Duration::from_millis(self.spec.debounce_ms),
            Duration::from_millis(self.spec.max_wait_ms),
        )
    }

    fn timeout(&self) -> Duration {
        let attempts = u32::from(self.spec.retries).saturating_add(1);
        Duration::from_millis(self.spec.timeout_ms).saturating_mul(attempts)
            + BACKOFF.iter().copied().sum::<Duration>()
            + Duration::from_secs(5)
    }

    fn applies(&self, item: &ItemView, outcome: TerminalStatus) -> bool {
        self.skip_reason(item, outcome).is_none()
    }

    fn skip_reason(&self, item: &ItemView, outcome: TerminalStatus) -> Option<SkipReason> {
        if !self.spec.fires_on(outcome) {
            return Some(SkipReason::owned(format!("`on` does not list {outcome}")));
        }
        if !self.filter_matches(item) {
            return Some(SkipReason::new(
                "the `when` filter does not match this item",
            ));
        }
        None
    }

    async fn run(&self, ctx: HookCtx<'_>) -> Result<(), HookError> {
        let tctx = self.template_ctx(&ctx);
        let value_ctx = self.value_ctx(&tctx);
        let template =
            |e: aulos_provider::command::TemplateError| HookError::Template(e.to_string().into());

        // Rendering once, outside the retry loop: a template failure is a bug, not a hiccup.
        let action: Prepared = match &self.spec.action {
            HookAction::Http {
                method,
                url,
                headers,
                body,
            } => {
                let url = url
                    .render_escaped(&tctx, Escape::Percent)
                    .map_err(template)?;
                let mut rendered = Vec::with_capacity(headers.len());
                for (name, value) in headers {
                    rendered.push((name.clone(), value.render(&value_ctx).map_err(template)?));
                }
                let body = body.render(&value_ctx).map_err(template)?;
                Prepared::Http {
                    method: *method,
                    url,
                    headers: rendered,
                    body,
                }
            }
            HookAction::Command { argv, cwd } => {
                let argv = render_argv(argv, &tctx).map_err(template)?;
                let cwd = if cwd.as_os_str().is_empty() {
                    self.plugin_dir.clone()
                } else {
                    cwd.clone()
                };
                Prepared::Command { argv, cwd }
            }
        };

        let attempts = usize::from(self.spec.retries) + 1;
        let mut last = HookError::other("no attempt was made");
        for i in 0..attempts {
            if ctx.cancel.is_cancelled() {
                return Err(HookError::Canceled);
            }
            let result = match &action {
                Prepared::Http {
                    method,
                    url,
                    headers,
                    body,
                } => self.http_attempt(*method, url, headers, body).await,
                Prepared::Command { argv, cwd } => {
                    self.command_attempt(argv, cwd, ctx.cancel).await
                }
            };
            match result {
                Ok(()) => {
                    tracing::info!(hook = %self.spec.id, items = ctx.count(), "community hook fired");
                    return Ok(());
                }
                Err(e) => {
                    let retryable = e.retryable();
                    last = e;
                    if !retryable || i + 1 == attempts {
                        break;
                    }
                    let wait = BACKOFF.get(i).copied().unwrap_or(BACKOFF[1]);
                    tracing::warn!(
                        hook = %self.spec.id,
                        attempt = i + 1,
                        error = %last,
                        "retrying the community hook in {wait:?}"
                    );
                    tokio::select! {
                        () = ctx.cancel.cancelled() => return Err(HookError::Canceled),
                        () = tokio::time::sleep(wait) => {}
                    }
                }
            }
        }
        Err(last)
    }

    fn health(&self) -> HookHealth {
        HookHealth::ok().with("action", self.spec.action.summary())
    }
}

/// The rendered action, so the retry loop renders nothing twice.
enum Prepared {
    Http {
        method: HttpMethod,
        url: String,
        headers: Vec<(String, String)>,
        body: String,
    },
    Command {
        argv: Vec<String>,
        cwd: PathBuf,
    },
}

fn reqwest_method(m: HttpMethod) -> reqwest::Method {
    match m {
        HttpMethod::Get => reqwest::Method::GET,
        HttpMethod::Post => reqwest::Method::POST,
        HttpMethod::Put => reqwest::Method::PUT,
        HttpMethod::Patch => reqwest::Method::PATCH,
        HttpMethod::Delete => reqwest::Method::DELETE,
        HttpMethod::Head => reqwest::Method::HEAD,
    }
}

/// `hook:<dir>/<id>` → `<dir>`.
fn dir_of(id: &str) -> Option<&str> {
    id.strip_prefix("hook:")?.split('/').next()
}

/// Whether the `http.body` template is JSON once every placeholder is substituted.
///
/// Deciding this from the template rather than from a rendered value keeps the escaping stable
/// across invocations: a title containing a quote must not be able to change how the *next* body
/// is escaped.
fn body_looks_like_json(spec: &HookSpec) -> bool {
    let HookAction::Http { body, .. } = &spec.action else {
        return false;
    };
    let src = body.as_str().trim();
    if src.is_empty() || !(src.starts_with('{') || src.starts_with('[')) {
        return false;
    }
    let probe = TemplateCtx {
        url: Url::parse("https://example.invalid/probe").ok(),
        ..TemplateCtx::default()
    };
    body.render(&probe)
        .is_ok_and(|rendered| serde_json::from_str::<Value>(&rendered).is_ok())
}

/// `{download_url}` — the view's own value when the engine filled one in, otherwise the
/// `PUBLIC_HOST_*` prefix plus the percent-encoded filename (DESIGN §4.6.2).
fn download_url(ctx: &HookCtx<'_>) -> String {
    if let Some(u) = ctx.item.download_url.as_deref() {
        return u.to_owned();
    }
    let Some(filename) = ctx.item.filename.as_deref() else {
        return String::new();
    };
    let prefix = ctx.cfg.public_host_prefix(ctx.item.selection.download_type);
    format!(
        "{prefix}{}",
        aulos_provider::command::template::percent_encode(filename)
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use aulos_provider::command::{Template, TokenScope};

    fn http_spec(body: &str) -> HookSpec {
        HookSpec {
            id: Arc::from("hook:media/ntfy"),
            on: vec![TerminalStatus::Finished],
            ordering: 50,
            debounce_ms: 0,
            max_wait_ms: 0,
            timeout_ms: 10_000,
            retries: 2,
            when: aulos_provider::command::HookFilter::default(),
            action: HookAction::Http {
                method: HttpMethod::Post,
                url: Template::literal("https://ntfy.test/topic"),
                headers: Vec::new(),
                body: Template::parse(body, TokenScope::Hook).unwrap(),
            },
        }
    }

    #[test]
    fn the_plugin_dir_comes_from_the_hook_id() {
        assert_eq!(dir_of("hook:media-server/plex"), Some("media-server"));
        assert_eq!(dir_of("plex"), None);
        let hook = ManifestHook::new(http_spec(""), std::path::Path::new("/config/plugins"));
        assert_eq!(hook.plugin_dir, PathBuf::from("/config/plugins/media"));
    }

    #[test]
    fn a_json_body_selects_json_escaping_and_anything_else_does_not() {
        assert!(
            ManifestHook::new(http_spec(r#"{"t":"{title}"}"#), std::path::Path::new("/p"))
                .body_is_json()
        );
        assert!(
            !ManifestHook::new(http_spec("{title}\n{filename}"), std::path::Path::new("/p"))
                .body_is_json()
        );
        assert!(!ManifestHook::new(http_spec(""), std::path::Path::new("/p")).body_is_json());
        // A JSON body escapes its string values but leaves `{count}` and the two JSON arrays
        // structural, which is what makes `\"titles\": {titles_json}` valid JSON.
        let json = ManifestHook::new(http_spec(r#"{"t":"{title}"}"#), std::path::Path::new("/p"));
        let mut tctx = TemplateCtx {
            title: "L'ultimo \"caso\"".to_owned(),
            count: 2,
            titles: vec!["a".to_owned(), "b".to_owned()],
            ..TemplateCtx::default()
        };
        let escaped = json.value_ctx(&tctx);
        assert_eq!(escaped.title, "L'ultimo \\\"caso\\\"");
        assert_eq!(escaped.count, 2);
        assert_eq!(escaped.titles, ["a", "b"]);

        tctx.title = "raw \"x\"".to_owned();
        let raw = ManifestHook::new(http_spec("{title}"), std::path::Path::new("/p"));
        assert_eq!(raw.value_ctx(&tctx).title, "raw \"x\"");
    }

    #[test]
    fn the_verb_mapping_is_total() {
        for (m, expected) in [
            (HttpMethod::Get, reqwest::Method::GET),
            (HttpMethod::Post, reqwest::Method::POST),
            (HttpMethod::Put, reqwest::Method::PUT),
            (HttpMethod::Patch, reqwest::Method::PATCH),
            (HttpMethod::Delete, reqwest::Method::DELETE),
            (HttpMethod::Head, reqwest::Method::HEAD),
        ] {
            assert_eq!(reqwest_method(m), expected);
        }
    }

    #[test]
    fn the_outer_timeout_covers_every_attempt_and_the_backoff() {
        let hook = ManifestHook::new(http_spec(""), std::path::Path::new("/p"));
        // 3 attempts × 10 s + 2 s + 8 s + 5 s
        assert_eq!(hook.timeout(), Duration::from_secs(45));
        assert_eq!(hook.ordering(), 50);
        assert!(!hook.debounce().is_armed());
    }
}
