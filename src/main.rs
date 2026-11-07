use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use folio_mcp::{
    config::{self, Config},
    library::Library,
    render::{Region, RenderOpts},
    server::Folio,
};
use rmcp::{
    ServiceExt,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};

#[derive(Parser)]
#[command(version, about = "MCP server that serves book pages as images")]
struct Cli {
    /// Config file [default: ~/.config/folio/folio.toml]
    #[arg(long, short, env = "FOLIO_CONFIG", global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server (the default): stdio, or streamable HTTP with --http.
    Serve {
        /// Serve over HTTP at this address (endpoint /mcp), e.g. 127.0.0.1:9050.
        #[arg(long)]
        http: Option<SocketAddr>,
        /// Extra Host header values to accept over HTTP (localhost is always allowed).
        #[arg(long = "allow-host")]
        allow_hosts: Vec<String>,
    },
    /// Fetch one page through the full pipeline and write it to a file; handy for
    /// testing a backend without an agent.
    Page {
        book: String,
        page: u32,
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long)]
        region: Option<Region>,
        #[arg(long)]
        color: bool,
        /// Write the backend's bytes untouched.
        #[arg(long)]
        raw: bool,
    },
    /// Print a book's resolved entry and page count.
    Info { book: String },
    /// List books, optionally filtered.
    Books { query: Option<String> },
}

#[tokio::main]
async fn main() -> Result<()> {
    // stdout belongs to the MCP protocol on stdio, so logs go to stderr.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let path = cli.config.unwrap_or_else(config::default_path);
    // HEY YO LISTEN - VEEEESH: in-process Rust backends get registered here, e.g.
    // `.with_backend("mine", Arc::new(MyBackend::new()))` — implement
    // `folio_mcp::backend::Backend` (two methods). Config-only command backends
    // need nothing here.
    let lib = Arc::new(Library::new(Config::load(&path)?));

    match cli.command.unwrap_or(Command::Serve {
        http: None,
        allow_hosts: Vec::new(),
    }) {
        Command::Serve { http: None, .. } => {
            tracing::info!("serving on stdio ({})", path.display());
            Folio::new(lib)
                .serve(rmcp::transport::stdio())
                .await?
                .waiting()
                .await?;
        }
        Command::Serve {
            http: Some(addr),
            allow_hosts,
        } => serve_http(lib, addr, allow_hosts).await?,
        Command::Page {
            book,
            page,
            out,
            region,
            color,
            raw,
        } => {
            let book = lib.resolve(&book).await?;
            let bytes = if raw {
                lib.raw_page(&book, page).await?.to_vec()
            } else {
                let opts = RenderOpts {
                    region,
                    max_edge: lib.render.max_edge,
                    grayscale: !color && lib.render.grayscale,
                    format: lib.render.format,
                    quality: lib.render.quality,
                };
                let img = lib.render_page(&book, page, opts).await?;
                eprintln!(
                    "{}x{} {} ({} bytes) from {}x{}",
                    img.width,
                    img.height,
                    img.mime,
                    img.data.len(),
                    img.source_width,
                    img.source_height
                );
                img.data
            };
            std::fs::write(&out, bytes).with_context(|| format!("writing {}", out.display()))?;
        }
        Command::Info { book } => {
            let book = lib.resolve(&book).await?;
            let pages = lib.page_count(&book).await?;
            println!("{book:#?}\npage count: {pages:?}");
        }
        Command::Books { query } => {
            for b in lib.list(query.as_deref()).await? {
                println!("{}\t{}", b.id, b.title.unwrap_or_default());
            }
        }
    }
    Ok(())
}

async fn serve_http(lib: Arc<Library>, addr: SocketAddr, allow_hosts: Vec<String>) -> Result<()> {
    let mut config = StreamableHttpServerConfig::default();
    config.allowed_hosts.extend(allow_hosts);
    let ct = config.cancellation_token.clone();
    let service: StreamableHttpService<Folio, LocalSessionManager> = StreamableHttpService::new(
        move || Ok(Folio::new(lib.clone())),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("serving on http://{addr}/mcp");
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            ct.cancel();
        })
        .await?;
    Ok(())
}
