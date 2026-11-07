//! Backends turn (book, page) into image bytes. The server never looks inside a
//! book; whatever a backend returns just has to decode as an image.

use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use tokio::process::Command;

use crate::{
    config::{BackendConfig, expand_home},
    template::{Vars, expand},
};

/// A resolved book: which backend serves it and how that backend finds it.
#[derive(Debug, Clone)]
pub struct BookRef {
    pub id: String,
    pub title: Option<String>,
    pub backend: String,
    /// Opaque to everything but the backend (a path, a key, a URL...).
    pub locator: String,
    /// Page count from the catalog, if it gave one.
    pub pages: Option<u32>,
}

impl BookRef {
    fn vars(&self, page: Option<u32>) -> Vars<'_> {
        Vars {
            book: &self.id,
            locator: &self.locator,
            page,
        }
    }
}

/// Implement this to plug a format in-process; or skip Rust entirely and use a
/// `command` backend.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Image bytes (PNG, JPEG, TIFF, WebP, GIF or BMP) for a 1-based page.
    async fn page(&self, book: &BookRef, page: u32) -> Result<Vec<u8>>;

    /// Number of pages, when the backend can tell.
    async fn page_count(&self, _book: &BookRef) -> Result<Option<u32>> {
        Ok(None)
    }
}

pub fn from_config(config: &BackendConfig) -> Box<dyn Backend> {
    match config.clone() {
        BackendConfig::Command {
            page,
            count,
            env,
            timeout_secs,
        } => Box::new(CommandBackend {
            page,
            count,
            env,
            timeout: Duration::from_secs(timeout_secs),
        }),
        BackendConfig::Files { path } => Box::new(FilesBackend {
            path: expand_home(&path),
        }),
    }
}

/// Runs `page` (and optionally `count`) as argv — no shell — with placeholders
/// expanded per argument. The page image goes to stdout; a non-zero exit is an
/// error and its stderr is passed along.
pub struct CommandBackend {
    page: Vec<String>,
    count: Option<Vec<String>>,
    env: BTreeMap<String, String>,
    timeout: Duration,
}

#[async_trait]
impl Backend for CommandBackend {
    async fn page(&self, book: &BookRef, page: u32) -> Result<Vec<u8>> {
        let argv = expand_argv(&self.page, &book.vars(Some(page)))?;
        let out = run(&argv, &self.env, self.timeout).await?;
        if out.is_empty() {
            bail!("backend printed nothing for page {page}");
        }
        Ok(out)
    }

    async fn page_count(&self, book: &BookRef) -> Result<Option<u32>> {
        let Some(count) = &self.count else {
            return Ok(None);
        };
        let argv = expand_argv(count, &book.vars(None))?;
        let out = run(&argv, &self.env, self.timeout).await?;
        let text = String::from_utf8_lossy(&out);
        let n = text
            .trim()
            .parse()
            .with_context(|| format!("count command printed {:?}, not a number", text.trim()))?;
        Ok(Some(n))
    }
}

/// Reads `path` with placeholders expanded, e.g. `/scans/{locator}/{page:04}.png`.
pub struct FilesBackend {
    path: String,
}

impl FilesBackend {
    fn page_path(&self, book: &BookRef, page: u32) -> Result<PathBuf> {
        Ok(PathBuf::from(expand(&self.path, &book.vars(Some(page)))?))
    }
}

#[async_trait]
impl Backend for FilesBackend {
    async fn page(&self, book: &BookRef, page: u32) -> Result<Vec<u8>> {
        let path = self.page_path(book, page)?;
        tokio::task::spawn_blocking(move || {
            std::fs::read(&path).with_context(|| format!("page {page} ({})", path.display()))
        })
        .await?
    }

    /// Probes for the last existing page, assuming pages are numbered 1..=n
    /// without gaps.
    async fn page_count(&self, book: &BookRef) -> Result<Option<u32>> {
        let exists = |p: u32| self.page_path(book, p).map(|path| path.is_file());
        if !exists(1)? {
            return Ok(None);
        }
        let (mut lo, mut hi) = (1u32, 2u32);
        while exists(hi)? {
            lo = hi;
            hi = hi.checked_mul(2).ok_or_else(|| anyhow!("too many pages"))?;
        }
        // lo exists, hi doesn't.
        while hi - lo > 1 {
            let mid = lo + (hi - lo) / 2;
            if exists(mid)? {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ok(Some(lo))
    }
}

pub fn expand_argv(template: &[String], vars: &Vars) -> Result<Vec<String>> {
    template.iter().map(|arg| expand(arg, vars)).collect()
}

/// Runs argv with no stdin and returns stdout, failing on a non-zero exit.
pub async fn run(
    argv: &[String],
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let (prog, args) = argv.split_first().context("empty command")?;
    let prog = expand_home(prog);
    let child = Command::new(&prog)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k, expand_home(v))))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {prog}"))?;
    let out = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("{prog} timed out after {}s", timeout.as_secs()))??;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = stderr.trim();
        let tail = &stderr[stderr.floor_char_boundary(stderr.len().saturating_sub(500))..];
        bail!("{prog} failed ({}): {tail}", out.status);
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(locator: &str) -> BookRef {
        BookRef {
            id: "b1".into(),
            title: None,
            backend: "x".into(),
            locator: locator.into(),
            pages: None,
        }
    }

    fn sh(script: &str) -> Vec<String> {
        vec!["sh".into(), "-c".into(), script.into(), "sh".into()]
    }

    #[tokio::test]
    async fn command_backend_passes_args_and_reads_stdout() {
        let mut page = sh(r#"printf '%s|%s|%s' "$1" "$2" "$FOO""#);
        page.extend(["{locator}".into(), "{page:03}".into()]);
        let backend = CommandBackend {
            page,
            count: Some(sh("echo 12")),
            env: [("FOO".into(), "bar".into())].into(),
            timeout: Duration::from_secs(5),
        };
        let b = book("loc with spaces");
        let out = backend.page(&b, 7).await.unwrap();
        assert_eq!(out, b"loc with spaces|007|bar");
        assert_eq!(backend.page_count(&b).await.unwrap(), Some(12));
    }

    #[tokio::test]
    async fn command_backend_reports_failures() {
        let backend = CommandBackend {
            page: sh("echo 'no such page' >&2; exit 3"),
            count: None,
            env: BTreeMap::new(),
            timeout: Duration::from_secs(5),
        };
        let err = backend.page(&book("x"), 1).await.unwrap_err().to_string();
        assert!(err.contains("no such page"), "{err}");

        let slow = CommandBackend {
            page: sh("sleep 5"),
            count: None,
            env: BTreeMap::new(),
            timeout: Duration::from_millis(200),
        };
        let err = slow.page(&book("x"), 1).await.unwrap_err().to_string();
        assert!(err.contains("timed out"), "{err}");
    }

    #[tokio::test]
    async fn files_backend_counts_pages() {
        let dir = tempfile::tempdir().unwrap();
        for p in 1..=37 {
            std::fs::write(dir.path().join(format!("{p:04}.png")), b"x").unwrap();
        }
        let backend = FilesBackend {
            path: format!("{}/{{page:04}}.png", dir.path().display()),
        };
        let b = book("");
        assert_eq!(backend.page_count(&b).await.unwrap(), Some(37));
        assert_eq!(backend.page(&b, 3).await.unwrap(), b"x");
        assert!(backend.page(&b, 38).await.is_err());
    }
}
