#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
target=${1:-}
if [ -z "$target" ]; then
    case "$(uname -m)" in
        arm64) target=aarch64-apple-darwin ;;
        x86_64) target=x86_64-apple-darwin ;;
        *) printf '%s\n' 'Unsupported macOS architecture.' >&2; exit 2 ;;
    esac
fi
case "$target" in
    aarch64-apple-darwin) platform=macos-aarch64; binary_arch=arm64 ;;
    x86_64-apple-darwin) platform=macos-x86_64; binary_arch=x86_64 ;;
    *) printf '%s\n' 'Usage: package-macos.sh [aarch64-apple-darwin|x86_64-apple-darwin] [--skip-build] [--without-example]' >&2; exit 2 ;;
esac
if [ "$#" -gt 0 ]; then shift; fi
skip_build=false
example=true
for option in "$@"; do
    case "$option" in
        --skip-build) skip_build=true ;;
        --without-example) example=false ;;
        *) printf 'Unknown option: %s\n' "$option" >&2; exit 2 ;;
    esac
done
toolchain=${FINCH_RUST_TOOLCHAIN:-1.99.1}
cd "$root"
if [ "$skip_build" = false ]; then
    if [ "$example" = true ]; then
        cargo "+$toolchain" build --release --locked --target "$target" -p finch-app -p finch-text-stats
    else
        cargo "+$toolchain" build --release --locked --target "$target" -p finch-app
    fi
fi
mkdir -p "$root/target"
metadata=$(mktemp "$root/target/package-metadata.XXXXXX")
trap 'rm -f -- "$metadata"' EXIT HUP INT TERM
cargo "+$toolchain" metadata --locked --format-version 1 --filter-platform "$target" > "$metadata"
version=$(python3 -c 'import json,sys; print(next(p["version"] for p in json.load(open(sys.argv[1]))["packages"] if p["name"] == "finch-app"))' "$metadata")
build="$root/target/$target/release"
if [ ! -f "$build/finch" ] && rustc "+$toolchain" -vV | grep -qx "host: $target"; then build="$root/target/release"; fi
[ -f "$build/finch" ] || { printf 'Missing release executable: %s\n' "$build/finch" >&2; exit 1; }
pdfium="$root/runtime/pdfium"
[ -f "$pdfium/.finch-pdfium.json" ] || { printf '%s\n' 'Run scripts/fetch-pdfium.sh first.' >&2; exit 1; }
python3 - "$pdfium" "$platform" <<'PY'
import hashlib, json, sys
from pathlib import Path
root, platform = Path(sys.argv[1]), sys.argv[2]
expected = {
    "macos-aarch64": "52e94ca5aa8847934330daf3f8150c190682c5ca93831468794f8b90d4392e40",
    "macos-x86_64": "6dedf83990e0e3d6b7c93c9e7589c5a126b0ae14b7464d76120cff7a26afb18b",
}
stamp = json.loads((root / ".finch-pdfium.json").read_text(encoding="utf-8-sig"))
assert stamp["release"] == "7881" and stamp["platform"] == platform, "PDFium platform/version mismatch"
assert stamp["archive_sha256"] == expected[platform], "PDFium archive provenance mismatch"
assert hashlib.sha256((root / "lib/libpdfium.dylib").read_bytes()).hexdigest() == stamp["library_sha256"], "PDFium library checksum mismatch"
PY
for binary in "$build/finch" "$pdfium/lib/libpdfium.dylib"; do
    lipo -verify_arch "$binary_arch" "$binary"
done
output="$root/dist/Finch-$version-$platform"
zip="$output.zip"
if [ -e "$output" ] || [ -e "$zip" ]; then printf 'Output already exists: %s\n' "$output" >&2; exit 1; fi
app="$output/Finch.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Frameworks" "$app/Contents/Resources/licenses/pdfium"
cp "$build/finch" "$app/Contents/MacOS/finch"
chmod +x "$app/Contents/MacOS/finch"
cp "$pdfium/lib/libpdfium.dylib" "$app/Contents/Frameworks/libpdfium.dylib"
cp "$pdfium/LICENSE" "$pdfium/VERSION" "$app/Contents/Resources/licenses/pdfium/"
cp "$pdfium/.finch-pdfium.json" "$app/Contents/Resources/licenses/pdfium/provenance.json"
cp -R "$pdfium/licenses" "$app/Contents/Resources/licenses/pdfium/"
python3 - "$root" "$output" "$metadata" "$version" "$platform" "$build" "$example" <<'PY'
import hashlib, json, plistlib, re, shutil, subprocess, sys
from pathlib import Path
root, output, metadata, version, platform, build, example = sys.argv[1:]
root, output, build = Path(root), Path(output), Path(build)
app = output / "Finch.app"
resources = app / "Contents/Resources"
licenses = resources / "licenses"
for license in root.glob("LICENSE*"):
    if license.is_file(): shutil.copy2(license, licenses / license.name)
notices = ["Finch third-party dependency notices; inventory follows Cargo.lock.",
           "PDFium 7881: https://github.com/bblanchon/pdfium-binaries/releases/tag/chromium%2F7881"]
supplements = root / "licenses/third-party"
for record in json.loads((supplements / "sources.json").read_text()):
    path = (supplements / record["file"]).resolve()
    assert path.is_relative_to(supplements.resolve()), "Invalid supplemental license path"
    assert hashlib.sha256(path.read_bytes()).hexdigest() == record["sha256"], f"License checksum mismatch: {path}"
shutil.copy2(supplements / "sources.json", licenses / "supplement-sources.json")
shutil.copy2(root / "licenses/NOTICE.txt", licenses / "NOTICE.txt")
data = json.loads(Path(metadata).read_text())
def find_license_files(source):
    return {p for p in source.rglob("*") if p.is_file() and (
        (re.match(r"^(LICENSE|COPYING|NOTICE|COPYRIGHT|OFL)([._-]|$)", p.name, re.I)
         and p.suffix.lower() not in {".rs", ".c", ".h", ".cpp", ".py"})
        or any(part.lower() in {"license", "licenses"} for part in p.relative_to(source).parts[:-1]))}
license_files = {}
for package in data["packages"]:
    source = Path(package["manifest_path"]).parent
    files = find_license_files(source)
    if package.get("license_file"):
        declared = (source / package["license_file"]).resolve()
        if declared.is_relative_to(source.resolve()) and declared.is_file(): files.add(declared)
    license_files[package["id"]] = files
for package in sorted(data["packages"], key=lambda p: (p["name"], p["version"])):
    notices.append(f'\n{package["name"]} {package["version"]}\nLicense: {package.get("license")}\nAuthors: {", ".join(package["authors"])}\nSource: {package.get("repository")}')
    source = Path(package["manifest_path"]).parent
    files = license_files[package["id"]]
    if not files and package["id"] in data["workspace_members"]:
        source = root
        files = {p for p in root.glob("LICENSE*") if p.is_file()}
        notices.append("License text: inherited Finch workspace license.")
    if not files and package.get("repository") in {"https://github.com/iced-rs/iced", "https://github.com/retep998/winapi-rs"}:
        shared = next((p for p in data["packages"] if p["repository"] == package["repository"] and license_files[p["id"]]), None)
        if shared:
            source = Path(shared["manifest_path"]).parent
            files = license_files[shared["id"]]
            notices.append(f'License text: shared upstream repository license, from {shared["name"]} {shared["version"]}.')
    destination = licenses / "rust" / f'{package["name"]}-{package["version"]}'
    for path in files:
        target = destination / path.relative_to(source)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(path, target)
    supplement = supplements / f'{package["name"]}-{package["version"]}'
    if supplement.is_dir():
        shutil.copytree(supplement, destination, dirs_exist_ok=True)
        notices.append("License text: supplemental pinned source; see licenses/supplement-sources.json and licenses/NOTICE.txt.")
    elif not files:
        raise RuntimeError(f'Missing license text for {package["name"]} {package["version"]}; review it before packaging.')
(resources / "THIRD-PARTY-NOTICES.txt").write_text("\n".join(notices) + "\n")
# Advertise the actual minimum requested by the compiled binary/runtime.
minimum = (11, 0)
for binary in (app / "Contents/MacOS/finch", app / "Contents/Frameworks/libpdfium.dylib"):
    load_commands = subprocess.check_output(["otool", "-l", str(binary)], text=True)
    for found in re.findall(r"\bminos\s+([\d.]+)", load_commands):
        minimum = max(minimum, tuple(map(int, found.split("."))))
plist = {
    "CFBundleExecutable": "finch", "CFBundleIdentifier": "app.finch.reader",
    "CFBundleName": "Finch", "CFBundleDisplayName": "Finch", "CFBundlePackageType": "APPL",
    "CFBundleShortVersionString": version, "CFBundleVersion": version,
    "LSMinimumSystemVersion": ".".join(map(str, minimum)),
    "LSApplicationCategoryType": "public.app-category.productivity", "NSHighResolutionCapable": True,
}
with (app / "Contents/Info.plist").open("wb") as handle: plistlib.dump(plist, handle)
if example == "true":
    plugin = output / "plugins/finch.text-stats"
    plugin.mkdir(parents=True)
    shutil.copy2(build / "finch-text-stats", plugin / "finch-text-stats")
    (plugin / "finch-text-stats").chmod(0o755)
    manifest = json.loads((root / "examples/text-stats/plugin.json").read_text())
    manifest["executables"] = {platform: "finch-text-stats"}
    (plugin / "plugin.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n")
(output / "README.txt").write_text("Finch — local PDF reading and management\n\nOpen Finch.app. PDFium is bundled under Contents/Frameworks.\nThe optional plugins/finch.text-stats package is installed manually in Finch.\nThis development bundle is unsigned, not notarized, and has no auto-updater.\n")
PY
plutil -lint "$app/Contents/Info.plist"
ditto -c -k --keepParent "$output" "$zip"
shasum -a 256 "$zip" > "$zip.sha256"
printf '%s\n' "$zip"
