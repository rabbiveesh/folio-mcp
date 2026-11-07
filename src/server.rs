//! The MCP surface: `get_page`, `book_info`, `list_books`.

use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer};

use crate::{
    backend::BookRef,
    library::Library,
    render::{REGION_HELP, Region, RenderOpts},
};

const INSTRUCTIONS: &str = "Serves pages of books as images, so you read the page as printed. \
Books are identified by an id (see list_books). Pages are 1-based physical pages of the scan, \
which often differ from the numbers printed on the page. If text is too small to read, request \
the same page again with a `region` to zoom in.";

#[derive(Clone)]
pub struct Folio {
    lib: Arc<Library>,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetPageArgs {
    /// Book id, as given by list_books.
    #[serde(deserialize_with = "string_or_number")]
    #[schemars(with = "String")]
    pub book: String,
    /// 1-based physical page number.
    pub page: u32,
    /// How many consecutive pages to return, starting at `page` (default 1; capped by the server).
    #[serde(default)]
    pub count: Option<u32>,
    /// Zoom into part of the page: top, bottom, left, right, top-left, top-right, bottom-left,
    /// bottom-right, center, or "x,y,w,h" as fractions of the page.
    #[serde(default)]
    pub region: Option<String>,
    /// Return color instead of grayscale.
    #[serde(default)]
    pub color: Option<bool>,
    /// Longest edge of the returned image in pixels (default from server config; lower saves tokens).
    #[serde(default)]
    pub max_edge: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BookArgs {
    /// Book id, as given by list_books.
    #[serde(deserialize_with = "string_or_number")]
    #[schemars(with = "String")]
    pub book: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListArgs {
    /// Case-insensitive substring to match against ids and titles.
    #[serde(default)]
    pub query: Option<String>,
}

/// Agents will send `1234` as often as `"1234"`.
fn string_or_number<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Id {
        S(String),
        N(serde_json::Number),
    }
    Ok(match Id::deserialize(d)? {
        Id::S(s) => s,
        Id::N(n) => n.to_string(),
    })
}

fn fail(msg: impl std::fmt::Display) -> Result<CallToolResult, ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(
        msg.to_string(),
    )]))
}

fn describe(book: &BookRef) -> String {
    match &book.title {
        Some(t) => format!("book {} «{t}»", book.id),
        None => format!("book {}", book.id),
    }
}

#[tool_router]
impl Folio {
    pub fn new(lib: Arc<Library>) -> Self {
        Self {
            lib,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Get one or more pages of a book as images.")]
    async fn get_page(
        &self,
        Parameters(args): Parameters<GetPageArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let lib = &self.lib;
        let book = match lib.resolve(&args.book).await {
            Ok(b) => b,
            Err(e) => return fail(format!("{e:#}")),
        };
        let region = match args.region.as_deref().map(str::parse::<Region>).transpose() {
            Ok(r) => r,
            Err(e) => return fail(format!("{e:#}")),
        };
        let total = match lib.page_count(&book).await {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("page count for {}: {e:#}", book.id);
                None
            }
        };
        if args.page == 0 {
            return fail("pages are 1-based");
        }
        if let Some(total) = total
            && args.page > total
        {
            return fail(format!(
                "{} has {total} pages; page {} doesn't exist",
                describe(&book),
                args.page
            ));
        }

        let max = lib.render.max_pages_per_call.max(1);
        let count = args.count.unwrap_or(1).clamp(1, max);
        let last = match total {
            Some(total) => (args.page + count - 1).min(total),
            None => args.page + count - 1,
        };
        let opts = RenderOpts {
            region,
            max_edge: args
                .max_edge
                .unwrap_or(lib.render.max_edge)
                .clamp(200, lib.render.max_edge.max(200)),
            grayscale: !args.color.unwrap_or(!lib.render.grayscale),
            format: lib.render.format,
            quality: lib.render.quality,
        };
        let of_total = total.map(|t| format!(" of {t}")).unwrap_or_default();
        let zoom = args
            .region
            .as_deref()
            .map(|r| format!(", region {r}"))
            .unwrap_or_default();

        let mut content = Vec::new();
        for page in args.page..=last {
            match lib.render_page(&book, page, opts.clone()).await {
                Ok(img) => {
                    content.push(ContentBlock::text(format!(
                        "{}, page {page}{of_total}{zoom} ({}x{} from {}x{} source)",
                        describe(&book),
                        img.width,
                        img.height,
                        img.source_width,
                        img.source_height
                    )));
                    content.push(ContentBlock::image(STANDARD.encode(&img.data), img.mime));
                }
                // A later page failing (e.g. past the end of a book whose length we
                // don't know) shouldn't throw away the pages we already have.
                Err(e) if !content.is_empty() => {
                    content.push(ContentBlock::text(format!("page {page}: {e:#}")));
                    break;
                }
                Err(e) => return fail(format!("{}, page {page}: {e:#}", describe(&book))),
            }
        }
        if args.count.unwrap_or(1) > max {
            content.push(ContentBlock::text(format!(
                "(at most {max} pages per call)"
            )));
        }
        Ok(CallToolResult::success(content))
    }

    #[tool(description = "Title and page count of a book.")]
    async fn book_info(
        &self,
        Parameters(args): Parameters<BookArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let book = match self.lib.resolve(&args.book).await {
            Ok(b) => b,
            Err(e) => return fail(format!("{e:#}")),
        };
        let pages = match self.lib.page_count(&book).await {
            Ok(Some(n)) => serde_json::json!(n),
            Ok(None) => serde_json::json!("unknown"),
            Err(e) => serde_json::json!(format!("unknown ({e:#})")),
        };
        let info = serde_json::json!({ "id": book.id, "title": book.title, "pages": pages });
        Ok(CallToolResult::success(vec![ContentBlock::text(
            info.to_string(),
        )]))
    }

    #[tool(
        description = "List the available books, optionally filtered by a substring of the id or title."
    )]
    async fn list_books(
        &self,
        Parameters(args): Parameters<ListArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        match self.lib.list(args.query.as_deref()).await {
            Ok(books) if books.is_empty() => Ok(CallToolResult::success(vec![ContentBlock::text(
                "no matching books (books outside the catalog listing may still be requested by id)",
            )])),
            Ok(books) => Ok(CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string(&books).unwrap_or_default(),
            )])),
            Err(e) => fail(format!("{e:#}")),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Folio {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("folio-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!("{INSTRUCTIONS} Regions: {REGION_HELP}."))
    }
}
