//! Book lookup, backend dispatch, and an in-memory page cache.

use std::{
    collections::{BTreeMap, HashMap},
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    backend::{self, Backend, BookRef},
    config::{BookEntry, CatalogConfig, Config, RenderConfig},
    render::{self, RenderOpts, Rendered},
    template::Vars,
};

type PageCache = lru::LruCache<(String, u32), Arc<Vec<u8>>>;

const CATALOG_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Library {
    pub render: RenderConfig,
    catalog: CatalogConfig,
    books: BTreeMap<String, BookEntry>,
    backends: HashMap<String, Arc<dyn Backend>>,
    cache: Mutex<PageCache>,
    counts: Mutex<HashMap<String, Option<u32>>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookSummary {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl Library {
    pub fn new(config: Config) -> Self {
        let backends = config
            .backends
            .iter()
            .map(|(name, cfg)| (name.clone(), Arc::from(backend::from_config(cfg))))
            .collect();
        let cache_size = NonZeroUsize::new(config.render.cache_pages).unwrap_or(NonZeroUsize::MIN);
        Self {
            render: config.render,
            catalog: config.catalog,
            books: config.books,
            backends,
            cache: Mutex::new(lru::LruCache::new(cache_size)),
            counts: Mutex::new(HashMap::new()),
        }
    }

    /// Registers an in-process backend (or replaces a configured one).
    pub fn with_backend(mut self, name: impl Into<String>, backend: Arc<dyn Backend>) -> Self {
        self.backends.insert(name.into(), backend);
        self
    }

    pub async fn resolve(&self, id: &str) -> Result<BookRef> {
        check_id(id)?;
        let entry = match self.books.get(id) {
            Some(entry) => entry.clone(),
            None => match self.lookup(id).await? {
                Some(entry) => entry,
                None if self.catalog.default_backend.is_some() => BookEntry::default(),
                None => bail!("no book {id:?}; use list_books to see what's available"),
            },
        };
        let backend = entry
            .backend
            .or_else(|| self.catalog.default_backend.clone())
            .with_context(|| format!("book {id:?} has no backend"))?;
        if !self.backends.contains_key(&backend) {
            bail!("book {id:?} uses unknown backend {backend:?}");
        }
        Ok(BookRef {
            id: id.to_string(),
            title: entry.title,
            backend,
            locator: entry.locator.unwrap_or_else(|| id.to_string()),
            pages: entry.pages,
        })
    }

    async fn lookup(&self, id: &str) -> Result<Option<BookEntry>> {
        let Some(template) = &self.catalog.lookup else {
            return Ok(None);
        };
        let vars = Vars {
            book: id,
            locator: id,
            page: None,
        };
        let argv = backend::expand_argv(template, &vars)?;
        // A failing lookup means "not found"; the reason still goes to the log.
        let out = match backend::run(&argv, &BTreeMap::new(), CATALOG_TIMEOUT).await {
            Ok(out) => out,
            Err(e) => {
                tracing::debug!("lookup {id:?}: {e:#}");
                return Ok(None);
            }
        };
        if out.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        serde_json::from_slice(&out)
            .map(Some)
            .with_context(|| format!("lookup command output for {id:?} isn't a JSON book entry"))
    }

    pub async fn page_count(&self, book: &BookRef) -> Result<Option<u32>> {
        if book.pages.is_some() {
            return Ok(book.pages);
        }
        let key = format!("{}\0{}", book.backend, book.locator);
        if let Some(n) = self.counts.lock().unwrap().get(&key) {
            return Ok(*n);
        }
        let n = self.backend(book).page_count(book).await?;
        self.counts.lock().unwrap().insert(key, n);
        Ok(n)
    }

    pub async fn raw_page(&self, book: &BookRef, page: u32) -> Result<Arc<Vec<u8>>> {
        let key = (book.id.clone(), page);
        if let Some(bytes) = self.cache.lock().unwrap().get(&key) {
            return Ok(bytes.clone());
        }
        let bytes = Arc::new(self.backend(book).page(book, page).await?);
        self.cache.lock().unwrap().put(key, bytes.clone());
        Ok(bytes)
    }

    pub async fn render_page(
        &self,
        book: &BookRef,
        page: u32,
        opts: RenderOpts,
    ) -> Result<Rendered> {
        let bytes = self.raw_page(book, page).await?;
        tokio::task::spawn_blocking(move || render::render(&bytes, &opts)).await?
    }

    pub async fn list(&self, query: Option<&str>) -> Result<Vec<BookSummary>> {
        let mut books: BTreeMap<String, BookSummary> = self
            .books
            .iter()
            .map(|(id, e)| {
                (
                    id.clone(),
                    BookSummary {
                        id: id.clone(),
                        title: e.title.clone(),
                    },
                )
            })
            .collect();
        if let Some(list) = &self.catalog.list {
            let out = backend::run(list, &BTreeMap::new(), CATALOG_TIMEOUT).await?;
            let listed: Vec<BookSummary> = serde_json::from_slice(&out)
                .context("list command output isn't a JSON array of {id, title}")?;
            for b in listed {
                books.entry(b.id.clone()).or_insert(b);
            }
        }
        let query = query.map(str::to_lowercase);
        Ok(books
            .into_values()
            .filter(|b| match &query {
                None => true,
                Some(q) => {
                    b.id.to_lowercase().contains(q)
                        || b.title
                            .as_deref()
                            .is_some_and(|t| t.to_lowercase().contains(q))
                }
            })
            .collect())
    }

    fn backend(&self, book: &BookRef) -> &dyn Backend {
        // resolve() only hands out BookRefs with a registered backend.
        self.backends[&book.backend].as_ref()
    }
}

/// Book ids come from the agent and end up in argv and file paths, so keep
/// them from looking like options or paths.
fn check_id(id: &str) -> Result<()> {
    let bad = id.is_empty()
        || id.len() > 256
        || id.starts_with(['-', '.'])
        || id.contains(['/', '\\'])
        || id.chars().any(char::is_control);
    if bad {
        bail!("invalid book id {id:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Fake {
        calls: AtomicU32,
    }

    #[async_trait]
    impl Backend for Fake {
        async fn page(&self, book: &BookRef, page: u32) -> Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(format!("{}:{page}", book.locator).into_bytes())
        }
        async fn page_count(&self, _: &BookRef) -> Result<Option<u32>> {
            Ok(Some(10))
        }
    }

    fn library(toml: &str) -> (Library, Arc<Fake>) {
        let config: Config = toml::from_str(toml).unwrap();
        let fake = Arc::new(Fake {
            calls: AtomicU32::new(0),
        });
        (
            Library::new(config).with_backend("fake", fake.clone()),
            fake,
        )
    }

    #[tokio::test]
    async fn resolves_listed_and_default_books() {
        let (lib, _) = library(
            r#"
            [catalog]
            default_backend = "fake"
            [books.12]
            title = "Twelve"
            locator = "/x/12.bin"
            pages = 3
            "#,
        );
        let b = lib.resolve("12").await.unwrap();
        assert_eq!((b.locator.as_str(), b.pages), ("/x/12.bin", Some(3)));
        assert_eq!(lib.page_count(&b).await.unwrap(), Some(3));

        let other = lib.resolve("שערי יושר").await.unwrap();
        assert_eq!(other.locator, "שערי יושר");
        assert_eq!(lib.page_count(&other).await.unwrap(), Some(10));

        for bad in ["", "../etc", "a/b", "-rf", "x\ny"] {
            assert!(lib.resolve(bad).await.is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn unknown_book_without_default_is_an_error() {
        let (lib, _) = library("");
        let err = lib.resolve("99").await.unwrap_err().to_string();
        assert!(err.contains("list_books"), "{err}");
    }

    #[tokio::test]
    async fn lookup_command_fills_in_books() {
        let (lib, _) = library(
            r#"
            [catalog]
            lookup = ["sh", "-c", 'test "$1" = 7 && echo "{{\"backend\":\"fake\",\"locator\":\"seven\",\"title\":\"T\"}}"', "sh", "{book}"]
            "#,
        );
        let b = lib.resolve("7").await.unwrap();
        assert_eq!(
            (b.locator.as_str(), b.title.as_deref()),
            ("seven", Some("T"))
        );
        assert!(lib.resolve("8").await.is_err());
    }

    #[tokio::test]
    async fn caches_pages() {
        let (lib, fake) = library("[catalog]\ndefault_backend = \"fake\"");
        let b = lib.resolve("1").await.unwrap();
        assert_eq!(*lib.raw_page(&b, 2).await.unwrap(), b"1:2");
        lib.raw_page(&b, 2).await.unwrap();
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lists_and_filters() {
        let (lib, _) = library(
            r#"
            [catalog]
            default_backend = "fake"
            list = ["echo", '[{"id":"3","title":"Ketzos"},{"id":"1","title":"dup"}]']
            [books.1]
            title = "Shaarei Yosher"
            "#,
        );
        let all = lib.list(None).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].title.as_deref(), Some("Shaarei Yosher"));
        let hits = lib.list(Some("ketz")).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "3");
    }
}
