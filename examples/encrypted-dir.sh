#!/bin/sh
# Example command backend: each book is a directory of pages encrypted with
# openssl (<dir>/0001.png.enc, ...). Decrypts in memory and writes the image to
# stdout. Swap the body for whatever your format needs; folio only cares that
# stdout is an image and the exit status is honest.
#
#   encrypted-dir.sh page <dir> <n>   -> image bytes on stdout
#   encrypted-dir.sh count <dir>      -> number of pages on stdout
#
# Key comes from $FOLIO_KEY_FILE (set via `env` in folio.toml).
#
# To encrypt a folder of PNGs:
#   for f in *.png; do openssl enc -aes-256-cbc -pbkdf2 -pass file:KEY -in "$f" -out "$f.enc"; done
set -eu

cmd=$1 dir=$2
: "${FOLIO_KEY_FILE:?set FOLIO_KEY_FILE}"

case $cmd in
page)
    f=$(printf '%s/%04d.png.enc' "$dir" "$3")
    [ -f "$f" ] || { echo "no page $3" >&2; exit 1; }
    # HEY YO LISTEN - VEEEESH: your real decoder goes here. Read page $3 of the
    # book at $dir, write the image bytes to stdout, exit non-zero on failure.
    exec openssl enc -d -aes-256-cbc -pbkdf2 -pass "file:$FOLIO_KEY_FILE" -in "$f"
    ;;
count)
    # HEY YO LISTEN - VEEEESH: print the page count (or drop `count` from the config).
    find "$dir" -maxdepth 1 -name '*.png.enc' | wc -l
    ;;
*)
    echo "usage: $0 page <dir> <n> | count <dir>" >&2
    exit 2
    ;;
esac
