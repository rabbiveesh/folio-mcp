//! `folio.toml`: render settings, backends, and the book catalog.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub render: RenderConfig,
    #[serde(default)]
    pub catalog: CatalogConfig,
    #[serde(default)]
    pub backends: BTreeMap<String, BackendConfig>,
    #[serde(default)]
    pub books: BTreeMap<String, BookEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RenderConfig {
    /// Longest edge of a returned image, in pixels. Claude downscales anything
    /// past ~1568 anyway, so going higher only costs bandwidth.
    pub max_edge: u32,
    pub format: OutputFormat,
    /// JPEG quality, 1-100.
    pub quality: u8,
    /// Default for the `color` tool argument (inverted).
    pub grayscale: bool,
    /// How many raw backend pages to keep in memory (never on disk).
    pub cache_pages: usize,
    /// Upper bound on `count` in a single `get_page` call.
    pub max_pages_per_call: u32,
}

impl Default for RenderConfig {
    fn default() -> Self {
        Self {
            max_edge: 1568,
            format: OutputFormat::Jpeg,
            quality: 85,
            grayscale: true,
            cache_pages: 64,
            max_pages_per_call: 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Jpeg,
    Png,
}

/// How book ids that aren't listed under `[books]` get resolved.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CatalogConfig {
    /// Command run with an unknown book id; prints a JSON book entry
    /// (`{"title", "backend", "locator", "pages"}`, all optional) or exits non-zero.
    pub lookup: Option<Vec<String>>,
    /// Command that prints the available books as a JSON array of
    /// `{"id", "title"}`; used by `list_books` in addition to `[books]`.
    pub list: Option<Vec<String>>,
    /// Backend for books that neither `[books]` nor `lookup` name one for.
    /// The book id doubles as the locator unless a locator is given.
    pub default_backend: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BackendConfig {
    /// Runs an external program per page; it writes image bytes to stdout.
    Command {
        page: Vec<String>,
        #[serde(default)]
        count: Option<Vec<String>>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default = "default_timeout")]
        timeout_secs: u64,
    },
    /// Reads page images straight off disk from a path template.
    Files { path: String },
}

fn default_timeout() -> u64 {
    30
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookEntry {
    pub title: Option<String>,
    pub backend: Option<String>,
    pub locator: Option<String>,
    pub pages: Option<u32>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let known = |name: &str| self.backends.contains_key(name);
        if let Some(name) = &self.catalog.default_backend
            && !known(name)
        {
            bail!("catalog.default_backend = {name:?} is not a configured backend");
        }
        for (id, book) in &self.books {
            match &book.backend {
                Some(name) if !known(name) => {
                    bail!("book {id:?} uses unknown backend {name:?}")
                }
                None if self.catalog.default_backend.is_none() => {
                    bail!("book {id:?} has no backend and there is no catalog.default_backend")
                }
                _ => {}
            }
        }
        for (name, backend) in &self.backends {
            if let BackendConfig::Command { page, .. } = backend
                && page.is_empty()
            {
                bail!("backend {name:?}: `page` command is empty");
            }
        }
        Ok(())
    }
}

/// `$FOLIO_CONFIG` is handled by clap; this is the fallback.
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("folio").join("folio.toml")
}

/// Expands a leading `~/` so config paths can be written the natural way.
pub fn expand_home(s: &str) -> String {
    match (s.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => s.to_string(),
    }
}
