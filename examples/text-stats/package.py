"""Package the already-built example executable for the current build platform."""
import argparse
import json
import platform
import shutil
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--profile", choices=("debug", "release"), default="release")
parser.add_argument("--target", help="Rust target triple when cross-compiling")
parser.add_argument("--output", type=Path, help="New output package directory")
args = parser.parse_args()
root = Path(__file__).resolve().parents[2]
if args.target:
    arch = args.target.split("-")[0]
    os_name = "windows" if "windows" in args.target else "macos" if "apple-darwin" in args.target else "linux" if "linux" in args.target else None
    if os_name is None:
        parser.error("Supported targets are Windows, macOS, and Linux")
else:
    os_name = {"Windows": "windows", "Darwin": "macos", "Linux": "linux"}[platform.system()]
    arch = {"AMD64": "x86_64", "arm64": "aarch64", "ARM64": "aarch64"}.get(platform.machine(), platform.machine())
platform_key = f"{os_name}-{arch}"
executable = "finch-text-stats" + (".exe" if os_name == "windows" else "")
binary = root / "target"
if args.target:
    binary /= args.target
binary = binary / args.profile / executable
if not binary.is_file():
    parser.error(f"Build the plugin first: missing {binary}")
output = args.output or root / "target" / "plugin-packages" / platform_key / "finch.text-stats"
output.mkdir(parents=True, exist_ok=False)
shutil.copy2(binary, output / executable)
for license_name in ("LICENSE-MIT", "LICENSE-APACHE"):
    shutil.copy2(root / license_name, output / license_name)
manifest = json.loads(Path(__file__).with_name("plugin.json").read_text(encoding="utf-8"))
manifest["executables"] = {platform_key: executable}
(output / "plugin.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
print(output)
