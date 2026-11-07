//! An MCP server that hands agents book pages as images.
//!
//! Formats are none of its business: a [`backend::Backend`] turns
//! `(book, page)` into image bytes, and the server crops, scales and serves
//! them. Backends are either external commands (see `folio.example.toml`) or
//! Rust types registered with [`library::Library::with_backend`].

pub mod backend;
pub mod config;
pub mod library;
pub mod render;
pub mod server;
pub mod template;
