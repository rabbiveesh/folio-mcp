# folio-mcp

An MCP server that gives agents book pages **as images**: the page as printed, with no
text-extraction surprises.

folio doesn't know or care about formats. A *backend* turns `(book, page)` into image
bytes (PNG/JPEG/TIFF/WebP/GIF/BMP), and folio handles the rest: cropping, downscaling
to a token-sane size, grayscale, caching in memory, and the MCP plumbing. Encrypted
formats are just backends that decrypt. folio never sees keys and never writes a page to
disk.

## Tools

| tool | what |
|---|---|
| `get_page(book, page, count?, region?, color?, max_edge?)` | page image(s) plus a line like `book 12 «title», page 37 of 412` |
| `book_info(book)` | title and page count |
| `list_books(query?)` | ids and titles |

`region` zooms in on part of the page (`bottom`, `top-right`, `"0,0.75,1,0.25"`, …), so
the agent can read small print without paying for the whole page in high resolution.

## Backends

- **`command`**: any program. folio runs it as argv (no shell) with `{locator}`, `{page}`,
  `{book}` filled in and reads the image from stdout. A non-zero exit is an error, and
  stderr is passed to the agent. See `examples/encrypted-dir.sh`.
- **`files`**: a path template such as `~/scans/{locator}/{page:04}.jpg`.
- **In-process Rust**: implement `folio_mcp::backend::Backend` and register it with
  `Library::with_backend`.

## Book ids

Ids are arbitrary strings: numbers, names, Hebrew, anything without slashes. An id is
resolved through the `[books]` table, then a `catalog.lookup` command, then
`catalog.default_backend` (the id itself becomes the locator). See `folio.example.toml`.

## Running

```sh
cp folio.example.toml ~/.config/folio/folio.toml   # and edit
cargo build --release

folio-mcp books                                    # sanity-check the catalog
folio-mcp page 1 12 -o /tmp/p.jpg                  # one page through the full pipeline
folio-mcp page 1 12 --region bottom -o /tmp/b.jpg

claude mcp add folio -- /path/to/folio-mcp         # stdio
folio-mcp serve --http 127.0.0.1:9050              # or streamable HTTP at /mcp
```

The HTTP mode has no auth. Keep it on localhost; `--allow-host` adds accepted `Host`
values if you put it behind something.

Grep for `HEY YO LISTEN - VEEEESH` to find the places that need your own code.
