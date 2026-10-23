#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
platform=${1:-}
if [ -z "$platform" ]; then
    case "$(uname -s)-$(uname -m)" in
        Darwin-arm64) platform=macos-aarch64 ;;
        Darwin-x86_64) platform=macos-x86_64 ;;
        *) printf '%s\n' 'Usage: fetch-pdfium.sh macos-aarch64|macos-x86_64 [local-archive]' >&2; exit 2 ;;
    esac
fi
case "$platform" in
    macos-aarch64) asset=pdfium-mac-arm64.tgz; expected=52e94ca5aa8847934330daf3f8150c190682c5ca93831468794f8b90d4392e40 ;;
    macos-x86_64) asset=pdfium-mac-x64.tgz; expected=6dedf83990e0e3d6b7c93c9e7589c5a126b0ae14b7464d76120cff7a26afb18b ;;
    *) printf 'Unsupported platform: %s\n' "$platform" >&2; exit 2 ;;
esac
release=7881
cache="$root/target/downloads/pdfium-$release"
mkdir -p "$cache" "$root/runtime"
hash() { shasum -a 256 "$1" | awk '{print $1}'; }
archive=${2:-"$cache/$asset"}
if [ "$#" -lt 2 ] && { [ ! -f "$archive" ] || [ "$(hash "$archive")" != "$expected" ]; }; then
    curl --fail --location --retry 3 --proto '=https' --tlsv1.2 \
        --output "$archive.download" \
        "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F$release/$asset"
    if [ "$(hash "$archive.download")" != "$expected" ]; then
        rm -f -- "$archive.download"
        printf '%s\n' 'PDFium SHA-256 mismatch; archive was not extracted.' >&2
        exit 1
    fi
    mv -- "$archive.download" "$archive"
fi
[ "$(hash "$archive")" = "$expected" ] || { printf '%s\n' 'PDFium SHA-256 mismatch.' >&2; exit 1; }
stage=$(mktemp -d "$root/runtime/.pdfium-stage.XXXXXX")
backup="$stage.previous"
trap 'rm -rf -- "$stage"' EXIT HUP INT TERM
tar -tzf "$archive" > "$stage/archive-entries.txt"
if grep -E '(^/)|(^[A-Za-z]:)|(^|/)\.\.(/|$)' "$stage/archive-entries.txt" >/dev/null; then
    printf '%s\n' 'Unsafe PDFium archive path.' >&2
    exit 1
fi
rm -- "$stage/archive-entries.txt"
tar -xzf "$archive" -C "$stage"
for required in lib/libpdfium.dylib LICENSE licenses VERSION; do
    [ -e "$stage/$required" ] || { printf 'PDFium archive lacks %s\n' "$required" >&2; exit 1; }
done
library_hash=$(hash "$stage/lib/libpdfium.dylib")
printf '{"release":"%s","platform":"%s","archive_sha256":"%s","library_sha256":"%s"}\n' \
    "$release" "$platform" "$expected" "$library_hash" > "$stage/.finch-pdfium.json"
if [ -e "$root/runtime/pdfium" ]; then mv -- "$root/runtime/pdfium" "$backup"; fi
if ! mv -- "$stage" "$root/runtime/pdfium"; then
    if [ -e "$backup" ]; then mv -- "$backup" "$root/runtime/pdfium"; fi
    exit 1
fi
rm -rf -- "$backup"
printf '%s\n' "$root/runtime/pdfium/lib/libpdfium.dylib"
