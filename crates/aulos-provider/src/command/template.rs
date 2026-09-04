//! The `{token}` template language of DESIGN §6.5.1 and §13.4.
//!
//! Three properties are the whole point of this module, and each one is a plugin-author complaint
//! the design set out to fix:
//!
//! 1. **An unknown token is a load-time error, not a silent empty string.** [`Template::parse`]
//!    rejects `{out_dirr}` with the byte offset it appears at, so a typo surfaces in
//!    `healthz` as `Degraded("download.command[3]: unknown token {out_dirr} at offset 7")`
//!    instead of producing a download into the current directory six weeks later.
//! 2. **Substitution is argv-level, never shell-level.** [`render_argv`] templates each argv
//!    element independently and hands it to `execvp` verbatim. A title containing
//!    `"; rm -rf / #"` is one argv element, exactly as a title containing `"Hello"` is: the argv
//!    **length is invariant** under the entry's content, which is what makes injection
//!    structurally impossible rather than merely escaped-for-now.
//! 3. **The two token tables are one type, split by [`TokenScope`].** A provider manifest may not
//!    use `{status}` and a hook may not use `{out_dir}`; both are rejected at load time with the
//!    scope named, rather than rendering as nothing.
//!
//! # Braces that are not tokens
//!
//! A `{` starts a token only when it is followed by a token-shaped name (`[a-z][a-z0-9_.]*`) and a
//! `}`. Everything else is literal text, so a JSON hook body — `{"title": "{title}"}` — needs no
//! escaping and still gets `{title}` substituted. `{{` and `}}` are accepted as explicit escapes
//! for a literal brace where the surrounding text would otherwise look token-shaped.

use std::fmt;
use std::path::{Path, PathBuf};

use aulos_core::selection::DownloadType;
use serde_json::Value;
use url::Url;

/// Which of the two DESIGN token tables a template is allowed to draw on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TokenScope {
    /// A provider manifest: `resolve.command`, `download.command`, `env.set`, `headers`
    /// (DESIGN §6.5.1).
    Provider,
    /// A community `[[hook]]`: `http.url`, `http.headers`, `http.body`, `command`
    /// (DESIGN §13.4).
    Hook,
}

impl TokenScope {
    /// The name used in an error message.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Hook => "hook",
        }
    }
}

impl fmt::Display for TokenScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One placeholder. The union of the DESIGN §6.5.1 and §13.4 tables.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Token {
    // --- shared by both tables ---
    /// `{url}` — the item URL.
    Url,
    /// `{title}` — the entry title.
    Title,
    /// `{download_type}`.
    DownloadType,
    /// `{format}`.
    Format,
    /// `{quality}`.
    Quality,

    // --- DESIGN §6.5.1, provider only ---
    /// `{url_host}`.
    UrlHost,
    /// `{url_path}`.
    UrlPath,
    /// `{url_query}`.
    UrlQuery,
    /// `{media_id}`.
    MediaId,
    /// `{out_dir}` — absolute, already created.
    OutDir,
    /// `{tmp_dir}` — absolute, already created.
    TmpDir,
    /// `{out_name}` — sanitised basename without extension.
    OutName,
    /// `{out_path}` — `{out_dir}/{out_name}.{output_ext}`.
    OutPath,
    /// `{output_ext}`.
    OutputExt,
    /// `{codec}`.
    Codec,
    /// `{subtitle_language}`.
    SubtitleLanguage,
    /// `{subtitle_mode}`.
    SubtitleMode,
    /// `{state}` — the entry's provider state as compact JSON.
    State,
    /// `{state.<key>}` — one field of it, JSON-scalar-stringified.
    StateField(Box<str>),
    /// `{playlist_index}`.
    PlaylistIndex,
    /// `{playlist_count}`.
    PlaylistCount,
    /// `{playlist_title}`.
    PlaylistTitle,
    /// `{cookies_file}`.
    CookiesFile,
    /// `{headers_curl}` — `-H "K: V"` argv pairs.
    HeadersCurl,
    /// `{headers_crlf}` — a `K: V\r\n` blob, ffmpeg style.
    HeadersCrlf,
    /// `{plugin_dir}`.
    PluginDir,

    // --- DESIGN §13.4, hook only ---
    /// `{id}` — the item ULID.
    Id,
    /// `{provider}`.
    Provider,
    /// `{status}`.
    Status,
    /// `{filename}` — relative to the download root.
    Filename,
    /// `{folder}`.
    Folder,
    /// `{download_url}` — the public URL, percent-encoded.
    DownloadUrl,
    /// `{size}`.
    Size,
    /// `{error_code}`.
    ErrorCode,
    /// `{error_message}`.
    ErrorMessage,
    /// `{count}` — coalesced events in a debounced batch.
    Count,
    /// `{titles_json}`.
    TitlesJson,
    /// `{filenames_json}`.
    FilenamesJson,
}

impl Token {
    /// The token spelling, without braces. `{state.<key>}` renders as `state.<key>`.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Self::StateField(k) => format!("state.{k}"),
            other => other.fixed_name().unwrap_or("state").to_owned(),
        }
    }

    /// The fixed spelling, for every variant but [`Token::StateField`].
    const fn fixed_name(&self) -> Option<&'static str> {
        Some(match self {
            Self::Url => "url",
            Self::Title => "title",
            Self::DownloadType => "download_type",
            Self::Format => "format",
            Self::Quality => "quality",
            Self::UrlHost => "url_host",
            Self::UrlPath => "url_path",
            Self::UrlQuery => "url_query",
            Self::MediaId => "media_id",
            Self::OutDir => "out_dir",
            Self::TmpDir => "tmp_dir",
            Self::OutName => "out_name",
            Self::OutPath => "out_path",
            Self::OutputExt => "output_ext",
            Self::Codec => "codec",
            Self::SubtitleLanguage => "subtitle_language",
            Self::SubtitleMode => "subtitle_mode",
            Self::State => "state",
            Self::PlaylistIndex => "playlist_index",
            Self::PlaylistCount => "playlist_count",
            Self::PlaylistTitle => "playlist_title",
            Self::CookiesFile => "cookies_file",
            Self::HeadersCurl => "headers_curl",
            Self::HeadersCrlf => "headers_crlf",
            Self::PluginDir => "plugin_dir",
            Self::Id => "id",
            Self::Provider => "provider",
            Self::Status => "status",
            Self::Filename => "filename",
            Self::Folder => "folder",
            Self::DownloadUrl => "download_url",
            Self::Size => "size",
            Self::ErrorCode => "error_code",
            Self::ErrorMessage => "error_message",
            Self::Count => "count",
            Self::TitlesJson => "titles_json",
            Self::FilenamesJson => "filenames_json",
            Self::StateField(_) => return None,
        })
    }

    /// Parses a token name, or `None` when it is not a token at all.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        if let Some(key) = name.strip_prefix("state.") {
            return if key.is_empty() {
                None
            } else {
                Some(Self::StateField(key.into()))
            };
        }
        Self::ALL.iter().find(|t| t.name() == name).cloned()
    }

    /// Every fixed token, in the order the two DESIGN tables list them.
    pub const ALL: [Self; 37] = [
        Self::Url,
        Self::UrlHost,
        Self::UrlPath,
        Self::UrlQuery,
        Self::MediaId,
        Self::Title,
        Self::OutDir,
        Self::TmpDir,
        Self::OutName,
        Self::OutPath,
        Self::OutputExt,
        Self::DownloadType,
        Self::Format,
        Self::Quality,
        Self::Codec,
        Self::SubtitleLanguage,
        Self::SubtitleMode,
        Self::State,
        Self::PlaylistIndex,
        Self::PlaylistCount,
        Self::PlaylistTitle,
        Self::CookiesFile,
        Self::HeadersCurl,
        Self::HeadersCrlf,
        Self::PluginDir,
        Self::Id,
        Self::Provider,
        Self::Status,
        Self::Filename,
        Self::Folder,
        Self::DownloadUrl,
        Self::Size,
        Self::ErrorCode,
        Self::ErrorMessage,
        Self::Count,
        Self::TitlesJson,
        Self::FilenamesJson,
    ];

    /// Whether this token is legal in `scope`.
    #[must_use]
    pub const fn in_scope(&self, scope: TokenScope) -> bool {
        let shared = matches!(
            self,
            Self::Url | Self::Title | Self::DownloadType | Self::Format | Self::Quality
        );
        if shared {
            return true;
        }
        let hook_only = matches!(
            self,
            Self::Id
                | Self::Provider
                | Self::Status
                | Self::Filename
                | Self::Folder
                | Self::DownloadUrl
                | Self::Size
                | Self::ErrorCode
                | Self::ErrorMessage
                | Self::Count
                | Self::TitlesJson
                | Self::FilenamesJson
        );
        match scope {
            TokenScope::Hook => hook_only,
            TokenScope::Provider => !hook_only,
        }
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{{}}}", self.name())
    }
}

/// What went wrong parsing or rendering a template.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum TemplateError {
    /// A token-shaped placeholder that is not in either table.
    #[error("unknown token {{{token}}} at offset {at}")]
    UnknownToken {
        /// The offending name, without braces.
        token: Box<str>,
        /// Byte offset of the opening brace in the template source.
        at: usize,
    },
    /// A known token used in the wrong table.
    #[error("token {{{token}}} at offset {at} is not available to a {scope} template")]
    OutOfScope {
        /// The offending name, without braces.
        token: Box<str>,
        /// Byte offset of the opening brace.
        at: usize,
        /// The scope that rejected it.
        scope: TokenScope,
    },
    /// `{state.<key>}` in a manifest without `capabilities.resolve` (DESIGN §6.5.2).
    #[error("token {{{token}}} needs capabilities.resolve = true")]
    StateNeedsResolve {
        /// The offending name, without braces.
        token: Box<str>,
    },
    /// A `{` with a token-shaped name and no closing `}`.
    #[error("unterminated token at offset {at}")]
    Unterminated {
        /// Byte offset of the opening brace.
        at: usize,
    },
    /// A token whose value the caller did not supply. A bug in the caller, not in the manifest.
    #[error("no value for {{{token}}} in this context")]
    Missing {
        /// The token that had no value.
        token: Box<str>,
    },
}

/// The characters a token name may contain: `{a-z0-9_.}` (DESIGN §6.5.1's own spellings).
const fn is_name_char(c: char) -> bool {
    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.'
}

/// One piece of a parsed template.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Seg {
    Literal(Box<str>),
    Token(Token),
}

/// A parsed `{token}` template (DESIGN §6.5.1).
///
/// Parsing is where validation happens; rendering is infallible for every token the caller
/// supplied a value for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Template {
    segs: Vec<Seg>,
    source: Box<str>,
}

impl Template {
    /// Parses `s` in `scope`.
    ///
    /// # Errors
    /// [`TemplateError::UnknownToken`], [`TemplateError::OutOfScope`] or
    /// [`TemplateError::Unterminated`], each carrying the byte offset of the opening brace.
    pub fn parse(s: &str, scope: TokenScope) -> Result<Self, TemplateError> {
        let mut segs = Vec::new();
        let mut literal = String::new();
        let bytes = s.as_bytes();
        let mut i = 0usize;

        while i < s.len() {
            match bytes[i] {
                b'{' if bytes.get(i + 1) == Some(&b'{') => {
                    literal.push('{');
                    i += 2;
                }
                b'}' if bytes.get(i + 1) == Some(&b'}') => {
                    literal.push('}');
                    i += 2;
                }
                b'{' => {
                    let rest = &s[i + 1..];
                    let name_len = rest.find(|c: char| !is_name_char(c)).unwrap_or(rest.len());
                    let name = &rest[..name_len];
                    // A `{` that is not followed by a token-shaped name is ordinary text — which
                    // is what makes a JSON hook body work without escaping.
                    if name.is_empty() || !name.starts_with(|c: char| c.is_ascii_lowercase()) {
                        literal.push('{');
                        i += 1;
                        continue;
                    }
                    if rest[name_len..].starts_with('}') {
                        let token =
                            Token::parse(name).ok_or_else(|| TemplateError::UnknownToken {
                                token: name.into(),
                                at: i,
                            })?;
                        if !token.in_scope(scope) {
                            return Err(TemplateError::OutOfScope {
                                token: name.into(),
                                at: i,
                                scope,
                            });
                        }
                        if !literal.is_empty() {
                            segs.push(Seg::Literal(std::mem::take(&mut literal).into()));
                        }
                        segs.push(Seg::Token(token));
                        i += 1 + name_len + 1;
                    } else if rest[name_len..].is_empty() {
                        return Err(TemplateError::Unterminated { at: i });
                    } else {
                        // `{a b}` and `{"a":1}` are literal text.
                        literal.push('{');
                        i += 1;
                    }
                }
                _ => {
                    let ch = s[i..].chars().next().unwrap_or('\u{fffd}');
                    literal.push(ch);
                    i += ch.len_utf8();
                }
            }
        }
        if !literal.is_empty() {
            segs.push(Seg::Literal(literal.into()));
        }
        Ok(Self {
            segs,
            source: s.into(),
        })
    }

    /// A template that is nothing but literal text.
    #[must_use]
    pub fn literal(s: &str) -> Self {
        Self {
            segs: if s.is_empty() {
                Vec::new()
            } else {
                vec![Seg::Literal(s.into())]
            },
            source: s.into(),
        }
    }

    /// The template source, verbatim. This is what `GET api/v2/providers` shows an operator.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// Every token this template references, in order.
    pub fn tokens(&self) -> impl Iterator<Item = &Token> {
        self.segs.iter().filter_map(|s| match s {
            Seg::Token(t) => Some(t),
            Seg::Literal(_) => None,
        })
    }

    /// Whether this template has no tokens at all.
    #[must_use]
    pub fn is_literal(&self) -> bool {
        self.tokens().next().is_none()
    }

    /// The single token this template consists of, when it consists of exactly one and nothing
    /// else. This is what lets `{headers_curl}` expand to several argv elements only when it *is*
    /// the element.
    #[must_use]
    pub fn sole_token(&self) -> Option<&Token> {
        match self.segs.as_slice() {
            [Seg::Token(t)] => Some(t),
            _ => None,
        }
    }

    /// Renders with no escaping.
    ///
    /// # Errors
    /// [`TemplateError::Missing`] when the context has no value for a referenced token.
    pub fn render(&self, ctx: &TemplateCtx) -> Result<String, TemplateError> {
        self.render_escaped(ctx, Escape::None)
    }

    /// Renders, escaping every substituted **value** (never the literal text) per `esc`.
    ///
    /// DESIGN §13.4: a placeholder inside an `http.url` is percent-encoded, and one inside a
    /// `body` or a header is JSON-escaped when the body parses as JSON.
    ///
    /// # Errors
    /// As [`Self::render`].
    pub fn render_escaped(&self, ctx: &TemplateCtx, esc: Escape) -> Result<String, TemplateError> {
        let mut out = String::with_capacity(self.source.len() + 32);
        for seg in &self.segs {
            match seg {
                Seg::Literal(l) => out.push_str(l),
                Seg::Token(t) => {
                    let v = ctx.value(t)?;
                    let text = match v {
                        Rendered::Single(s) => s,
                        Rendered::Argv(parts) => join_shell(&parts),
                    };
                    out.push_str(&esc.apply(&text));
                }
            }
        }
        Ok(out)
    }
}

/// How a substituted value is escaped (DESIGN §13.4).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Escape {
    /// Inserted raw. Correct for argv elements and for a non-JSON body.
    #[default]
    None,
    /// Percent-encoded, for a value inside a URL.
    Percent,
    /// JSON string-escaped, for a value inside a JSON body or header.
    Json,
}

impl Escape {
    /// Applies the escaping.
    #[must_use]
    pub fn apply(self, s: &str) -> String {
        match self {
            Self::None => s.to_owned(),
            Self::Percent => percent_encode(s),
            Self::Json => json_escape(s),
        }
    }
}

/// Percent-encodes everything outside RFC 3986's unreserved set.
///
/// Written here rather than pulled from `percent-encoding` because DESIGN §3's dependency row for
/// `aulos-provider` does not budget for it, and this is the only place the crate needs it.
#[must_use]
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(upper_hex(b >> 4));
            out.push(upper_hex(b & 0x0f));
        }
    }
    out
}

const fn upper_hex(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + nibble - 10) as char,
    }
}

/// Escapes `s` for the inside of a JSON string literal, without the surrounding quotes.
#[must_use]
pub fn json_escape(s: &str) -> String {
    let quoted = Value::String(s.to_owned()).to_string();
    // `to_string` on a JSON string is always `"…"`, so trimming one byte each side is safe.
    quoted
        .get(1..quoted.len().saturating_sub(1))
        .unwrap_or_default()
        .to_owned()
}

/// A rendered token value: one string, or the several argv elements `{headers_curl}` becomes.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Rendered {
    Single(String),
    Argv(Vec<String>),
}

/// Joins argv elements the way a shell command line would read them, so `{headers_curl}` embedded
/// in a larger element (`sh -c "curl {headers_curl} …"`) is still usable.
fn join_shell(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| {
            if p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':'))
            {
                p.clone()
            } else {
                format!("\"{}\"", p.replace('\\', "\\\\").replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Everything a template can substitute (DESIGN §6.5.1, §13.4).
///
/// One flat struct rather than two, because `{url}`, `{title}`, `{download_type}`, `{format}` and
/// `{quality}` appear in both tables and [`TokenScope`] already stops a manifest from reaching
/// into the other half. Every field has a harmless default, so a caller fills in what its context
/// actually knows: the provider fills the §6.5.1 half, `aulos-hooks` fills the §13.4 half.
#[derive(Clone, Debug, Default)]
pub struct TemplateCtx {
    /// `{url}` and the source of `{url_host}` / `{url_path}` / `{url_query}`.
    pub url: Option<Url>,
    /// `{media_id}`.
    pub media_id: String,
    /// `{title}`.
    pub title: String,
    /// `{out_dir}`.
    pub out_dir: PathBuf,
    /// `{tmp_dir}`.
    pub tmp_dir: PathBuf,
    /// `{out_name}` — sanitised, without extension.
    pub out_name: String,
    /// `{output_ext}` — without a dot.
    pub output_ext: String,
    /// `{download_type}`.
    pub download_type: Option<DownloadType>,
    /// `{format}`.
    pub format: String,
    /// `{quality}`.
    pub quality: String,
    /// `{codec}`.
    pub codec: String,
    /// `{subtitle_language}`.
    pub subtitle_language: String,
    /// `{subtitle_mode}`.
    pub subtitle_mode: String,
    /// `{state}` and `{state.<key>}`.
    pub state: Value,
    /// `{playlist_index}`.
    pub playlist_index: Option<u32>,
    /// `{playlist_count}`.
    pub playlist_count: Option<u32>,
    /// `{playlist_title}`.
    pub playlist_title: String,
    /// `{cookies_file}` — `""` when there is no cookie jar.
    pub cookies_file: String,
    /// `{headers_curl}` / `{headers_crlf}`, already rendered.
    pub headers: Vec<(String, String)>,
    /// `{plugin_dir}`.
    pub plugin_dir: PathBuf,
    /// `{id}` — the item ULID.
    pub item_id: String,
    /// `{provider}`.
    pub provider: String,
    /// `{status}`.
    pub status: String,
    /// `{filename}`.
    pub filename: String,
    /// `{folder}`.
    pub folder: String,
    /// `{download_url}` — already a URL; [`Escape::Percent`] encodes it again where required.
    pub download_url: String,
    /// `{size}`.
    pub size: Option<u64>,
    /// `{error_code}`.
    pub error_code: String,
    /// `{error_message}`.
    pub error_message: String,
    /// `{count}` — 1 for an undebounced hook.
    pub count: u32,
    /// `{titles_json}`.
    pub titles: Vec<String>,
    /// `{filenames_json}`.
    pub filenames: Vec<String>,
}

impl TemplateCtx {
    fn value(&self, t: &Token) -> Result<Rendered, TemplateError> {
        let url = || -> Result<&Url, TemplateError> {
            self.url.as_ref().ok_or_else(|| TemplateError::Missing {
                token: "url".into(),
            })
        };
        let single = |s: String| Ok(Rendered::Single(s));
        match t {
            Token::Url => single(url()?.to_string()),
            Token::UrlHost => single(url()?.host_str().unwrap_or_default().to_owned()),
            Token::UrlPath => single(url()?.path().to_owned()),
            Token::UrlQuery => single(url()?.query().unwrap_or_default().to_owned()),
            Token::MediaId => single(self.media_id.clone()),
            Token::Title => single(self.title.clone()),
            Token::OutDir => single(path_string(&self.out_dir)),
            Token::TmpDir => single(path_string(&self.tmp_dir)),
            Token::OutName => single(self.out_name.clone()),
            Token::OutPath => single(self.out_path()),
            Token::OutputExt => single(self.output_ext.clone()),
            Token::DownloadType => single(
                self.download_type
                    .map(|d| d.as_str().to_owned())
                    .unwrap_or_default(),
            ),
            Token::Format => single(self.format.clone()),
            Token::Quality => single(self.quality.clone()),
            Token::Codec => single(self.codec.clone()),
            Token::SubtitleLanguage => single(self.subtitle_language.clone()),
            Token::SubtitleMode => single(self.subtitle_mode.clone()),
            Token::State => single(compact_json(&self.state)),
            Token::StateField(key) => single(
                self.state
                    .get(&**key)
                    .map(scalar_string)
                    .unwrap_or_default(),
            ),
            Token::PlaylistIndex => single(opt_number(self.playlist_index)),
            Token::PlaylistCount => single(opt_number(self.playlist_count)),
            Token::PlaylistTitle => single(self.playlist_title.clone()),
            Token::CookiesFile => single(self.cookies_file.clone()),
            Token::HeadersCurl => Ok(Rendered::Argv(
                self.headers
                    .iter()
                    .flat_map(|(k, v)| ["-H".to_owned(), format!("{k}: {v}")])
                    .collect(),
            )),
            Token::HeadersCrlf => single(
                self.headers
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}\r\n"))
                    .collect(),
            ),
            Token::PluginDir => single(path_string(&self.plugin_dir)),
            Token::Id => single(self.item_id.clone()),
            Token::Provider => single(self.provider.clone()),
            Token::Status => single(self.status.clone()),
            Token::Filename => single(self.filename.clone()),
            Token::Folder => single(self.folder.clone()),
            Token::DownloadUrl => single(self.download_url.clone()),
            Token::Size => single(opt_number(self.size)),
            Token::ErrorCode => single(self.error_code.clone()),
            Token::ErrorMessage => single(self.error_message.clone()),
            Token::Count => single(self.count.to_string()),
            Token::TitlesJson => single(json_array(&self.titles)),
            Token::FilenamesJson => single(json_array(&self.filenames)),
        }
    }

    /// `{out_path}` — `{out_dir}/{out_name}.{output_ext}`, with the dot dropped when there is no
    /// extension.
    #[must_use]
    pub fn out_path(&self) -> String {
        let name = if self.output_ext.is_empty() {
            self.out_name.clone()
        } else {
            format!("{}.{}", self.out_name, self.output_ext)
        };
        path_string(&self.out_dir.join(name))
    }
}

fn path_string(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn opt_number<T: fmt::Display>(v: Option<T>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => String::new(),
    }
}

fn compact_json(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_owned())
}

/// A JSON value as a plugin author expects to see it on a command line: a string unquoted, a
/// number or boolean in its literal form, anything else as compact JSON.
fn scalar_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => compact_json(other),
    }
}

fn json_array(items: &[String]) -> String {
    compact_json(&Value::Array(
        items.iter().map(|s| Value::String(s.clone())).collect(),
    ))
}

/// The [`Template`] free function of PLAN WP-10.
///
/// # Errors
/// As [`Template::render`].
pub fn render(t: &Template, ctx: &TemplateCtx) -> Result<String, TemplateError> {
    t.render(ctx)
}

/// Renders a whole argv, one element at a time (DESIGN §6.5.1: "argv-level, never shell-level").
///
/// Every element becomes exactly one argv entry, **except** an element that is nothing but
/// `{headers_curl}`, which becomes the `-H`/`K: V` pairs the manifest's `[headers]` table
/// declares. The expansion therefore depends only on the manifest, never on the entry's title,
/// URL or state — which is the invariant that makes argv injection impossible.
///
/// # Errors
/// As [`Template::render`].
pub fn render_argv(argv: &[Template], ctx: &TemplateCtx) -> Result<Vec<String>, TemplateError> {
    let mut out = Vec::with_capacity(argv.len());
    for t in argv {
        if let Some(Token::HeadersCurl) = t.sole_token() {
            match ctx.value(&Token::HeadersCurl)? {
                Rendered::Argv(parts) => out.extend(parts),
                Rendered::Single(s) => out.push(s),
            }
            continue;
        }
        out.push(t.render(ctx)?);
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn ctx() -> TemplateCtx {
        TemplateCtx {
            url: Some(Url::parse("https://bandcamp.com/album/914?x=1#frag").unwrap()),
            media_id: "bc:album:914".into(),
            title: "Album — Deluxe".into(),
            out_dir: PathBuf::from("/downloads/Music"),
            tmp_dir: PathBuf::from("/tmp/aulos"),
            out_name: "Album - Deluxe".into(),
            output_ext: "flac".into(),
            download_type: Some(DownloadType::Audio),
            format: "flac".into(),
            quality: "best".into(),
            codec: "auto".into(),
            subtitle_language: "en".into(),
            subtitle_mode: "prefer_manual".into(),
            state: serde_json::json!({ "stream_id": "a91f", "n": 3, "deep": { "k": 1 } }),
            playlist_index: Some(2),
            playlist_count: Some(8),
            playlist_title: "Album".into(),
            cookies_file: "/config/cookies.txt".into(),
            headers: vec![("Referer".into(), "https://bandcamp.com/".into())],
            plugin_dir: PathBuf::from("/config/plugins/bandcamp"),
            item_id: "01JABCDEF".into(),
            provider: "command:bandcamp".into(),
            status: "finished".into(),
            filename: "Music/Album.flac".into(),
            folder: "Music".into(),
            download_url: "https://host/downloads/Music/Album.flac".into(),
            size: Some(4096),
            error_code: String::new(),
            error_message: String::new(),
            count: 3,
            titles: vec!["A".into(), "B \"q\"".into()],
            filenames: vec!["a.flac".into()],
        }
    }

    fn p(s: &str) -> Template {
        Template::parse(s, TokenScope::Provider).unwrap()
    }

    #[test]
    fn every_provider_token_resolves() {
        let c = ctx();
        let cases = [
            ("{url}", "https://bandcamp.com/album/914?x=1#frag"),
            ("{url_host}", "bandcamp.com"),
            ("{url_path}", "/album/914"),
            ("{url_query}", "x=1"),
            ("{media_id}", "bc:album:914"),
            ("{title}", "Album — Deluxe"),
            ("{out_dir}", "/downloads/Music"),
            ("{tmp_dir}", "/tmp/aulos"),
            ("{out_name}", "Album - Deluxe"),
            ("{out_path}", "/downloads/Music/Album - Deluxe.flac"),
            ("{output_ext}", "flac"),
            ("{download_type}", "audio"),
            ("{format}", "flac"),
            ("{quality}", "best"),
            ("{codec}", "auto"),
            ("{subtitle_language}", "en"),
            ("{subtitle_mode}", "prefer_manual"),
            ("{state.stream_id}", "a91f"),
            ("{state.n}", "3"),
            ("{state.deep}", r#"{"k":1}"#),
            ("{state.absent}", ""),
            ("{playlist_index}", "2"),
            ("{playlist_count}", "8"),
            ("{playlist_title}", "Album"),
            ("{cookies_file}", "/config/cookies.txt"),
            ("{headers_crlf}", "Referer: https://bandcamp.com/\r\n"),
            ("{plugin_dir}", "/config/plugins/bandcamp"),
        ];
        for (src, want) in cases {
            assert_eq!(p(src).render(&c).unwrap(), want, "{src}");
        }
        // `{state}` is compact JSON with sorted-by-insertion keys.
        let state = p("{state}").render(&c).unwrap();
        assert!(state.starts_with('{') && state.contains("\"stream_id\":\"a91f\""));
        // `{headers_curl}` embedded in text joins shell-style.
        assert_eq!(
            p("curl {headers_curl}").render(&c).unwrap(),
            "curl -H \"Referer: https://bandcamp.com/\""
        );
    }

    #[test]
    fn every_hook_token_resolves() {
        let c = ctx();
        let h = |s: &str| {
            Template::parse(s, TokenScope::Hook)
                .unwrap()
                .render(&c)
                .unwrap()
        };
        assert_eq!(h("{id}"), "01JABCDEF");
        assert_eq!(h("{title}"), "Album — Deluxe");
        assert_eq!(h("{url}"), "https://bandcamp.com/album/914?x=1#frag");
        assert_eq!(h("{provider}"), "command:bandcamp");
        assert_eq!(h("{status}"), "finished");
        assert_eq!(h("{filename}"), "Music/Album.flac");
        assert_eq!(h("{folder}"), "Music");
        assert_eq!(
            h("{download_url}"),
            "https://host/downloads/Music/Album.flac"
        );
        assert_eq!(h("{size}"), "4096");
        assert_eq!(h("{download_type}"), "audio");
        assert_eq!(h("{format}"), "flac");
        assert_eq!(h("{quality}"), "best");
        assert_eq!(h("{error_code}"), "");
        assert_eq!(h("{error_message}"), "");
        assert_eq!(h("{count}"), "3");
        assert_eq!(h("{titles_json}"), r#"["A","B \"q\""]"#);
        assert_eq!(h("{filenames_json}"), r#"["a.flac"]"#);
    }

    #[test]
    fn an_unknown_token_is_a_load_time_error_with_an_offset() {
        let e = Template::parse("--out {out_dirr}", TokenScope::Provider).unwrap_err();
        assert_eq!(
            e,
            TemplateError::UnknownToken {
                token: "out_dirr".into(),
                at: 6
            }
        );
        assert_eq!(e.to_string(), "unknown token {out_dirr} at offset 6");
        // Unterminated.
        assert_eq!(
            Template::parse("{out_dir", TokenScope::Provider).unwrap_err(),
            TemplateError::Unterminated { at: 0 }
        );
        // `state.` with no key is not a token, so it is an unknown token, not a panic.
        assert!(matches!(
            Template::parse("{state.}", TokenScope::Provider).unwrap_err(),
            TemplateError::UnknownToken { .. }
        ));
    }

    #[test]
    fn scopes_are_enforced_in_both_directions() {
        let e = Template::parse("{status}", TokenScope::Provider).unwrap_err();
        assert!(matches!(
            e,
            TemplateError::OutOfScope {
                scope: TokenScope::Provider,
                ..
            }
        ));
        assert_eq!(
            e.to_string(),
            "token {status} at offset 0 is not available to a provider template"
        );
        assert!(matches!(
            Template::parse("{out_dir}", TokenScope::Hook).unwrap_err(),
            TemplateError::OutOfScope {
                scope: TokenScope::Hook,
                ..
            }
        ));
        // The five shared tokens are legal in both.
        for shared in [
            "{url}",
            "{title}",
            "{download_type}",
            "{format}",
            "{quality}",
        ] {
            assert!(
                Template::parse(shared, TokenScope::Provider).is_ok(),
                "{shared}"
            );
            assert!(
                Template::parse(shared, TokenScope::Hook).is_ok(),
                "{shared}"
            );
        }
    }

    #[test]
    fn braces_that_are_not_tokens_stay_literal() {
        let c = ctx();
        // A JSON body needs no escaping.
        let t = Template::parse(r#"{"title": "{title}", "n": 1}"#, TokenScope::Hook).unwrap();
        assert_eq!(
            t.render(&c).unwrap(),
            r#"{"title": "Album — Deluxe", "n": 1}"#
        );
        // Explicit brace escapes.
        assert_eq!(p("{{url}}").render(&c).unwrap(), "{url}");
        // Upper case and spaces are not token-shaped.
        assert_eq!(p("{URL} {a b}").render(&c).unwrap(), "{URL} {a b}");
        assert!(p("literal").is_literal());
        assert!(!p("{url}").is_literal());
        assert_eq!(p("{url}").sole_token(), Some(&Token::Url));
        assert_eq!(p("x{url}").sole_token(), None);
        assert_eq!(p("{url}").as_str(), "{url}");
    }

    #[test]
    fn escaping_modes_apply_to_values_only() {
        let c = ctx();
        let t = Template::parse("http://plex:32400/refresh?t={title}", TokenScope::Hook).unwrap();
        let url = t.render_escaped(&c, Escape::Percent).unwrap();
        assert_eq!(
            url,
            "http://plex:32400/refresh?t=Album%20%E2%80%94%20Deluxe"
        );
        // The literal `?` and `=` survive; only the value is encoded.
        assert!(url.starts_with("http://plex:32400/refresh?t="));
        let body = Template::parse(r#"{"t":"{title}"}"#, TokenScope::Hook).unwrap();
        let json = body.render_escaped(&c, Escape::Json).unwrap();
        assert_eq!(json, r#"{"t":"Album — Deluxe"}"#);
        let hostile = TemplateCtx {
            title: "a\"b\\c\nd".into(),
            ..c
        };
        let json = body.render_escaped(&hostile, Escape::Json).unwrap();
        assert_eq!(json, r#"{"t":"a\"b\\c\nd"}"#);
        assert!(serde_json::from_str::<Value>(&json).is_ok());
    }

    #[test]
    fn a_missing_value_names_the_token() {
        let empty = TemplateCtx::default();
        assert_eq!(
            p("{url}").render(&empty).unwrap_err(),
            TemplateError::Missing {
                token: "url".into()
            }
        );
        // Everything else has a harmless default.
        assert_eq!(p("{title}-{playlist_index}").render(&empty).unwrap(), "-");
    }

    #[test]
    fn headers_curl_expands_only_as_a_whole_element() {
        let c = ctx();
        let argv = [p("dl"), p("{headers_curl}"), p("{url}")];
        let rendered = render_argv(&argv, &c).unwrap();
        assert_eq!(
            rendered,
            [
                "dl",
                "-H",
                "Referer: https://bandcamp.com/",
                "https://bandcamp.com/album/914?x=1#frag"
            ]
        );
        // With no headers declared it contributes nothing at all.
        let bare = TemplateCtx {
            headers: Vec::new(),
            ..c
        };
        assert_eq!(render_argv(&argv, &bare).unwrap().len(), 2);
    }

    #[test]
    fn render_free_function_matches_the_method() {
        let c = ctx();
        let t = p("{title}");
        assert_eq!(render(&t, &c).unwrap(), t.render(&c).unwrap());
    }

    #[test]
    fn token_names_round_trip() {
        for t in &Token::ALL {
            let name = t.name();
            assert_eq!(Token::parse(&name).as_ref(), Some(t), "{name}");
            assert_eq!(t.to_string(), format!("{{{name}}}"));
        }
        assert_eq!(Token::parse("state.x"), Some(Token::StateField("x".into())));
        assert_eq!(Token::parse("nope"), None);
        assert_eq!(Token::StateField("x".into()).name(), "state.x");
        // Every fixed token is in exactly one scope-or-shared bucket.
        for t in &Token::ALL {
            assert!(
                t.in_scope(TokenScope::Provider) || t.in_scope(TokenScope::Hook),
                "{t} is in neither table"
            );
        }
    }
}
