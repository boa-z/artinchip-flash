"""Check locked Cargo license metadata and collect release attributions."""

import argparse
from collections import Counter
from functools import cache
import json
from pathlib import Path
import re
import subprocess
from urllib.error import HTTPError
from urllib.request import urlopen


PERMISSIVE = {
    "0BSD", "Apache-2.0", "BSD-2-Clause", "BSD-3-Clause", "BSL-1.0", "CC0-1.0",
    "CDLA-Permissive-2.0", "ISC", "LLVM-exception", "MIT", "MIT-0", "Unicode-3.0",
    "Unlicense", "Zlib",
}

REVIEWED_TERMS = {
    "epaint_default_fonts": {"OFL-1.1", "Ubuntu-font-1.0"},
    "r-efi": {"LGPL-2.1-or-later"},
    "self_cell": {"GPL-2.0-only"},
    "serialport": {"MPL-2.0"},
    "unescaper": {"GPL-3.0-only"},
}

NOTICE_FILE = re.compile(r"(?:licen[cs]es?|copying|copyright|notice|authors)(?:[._-].*)?$", re.I)


def metadata(target, no_default_features):
    command = ["cargo", "metadata", "--locked", "--format-version", "1"]
    if target:
        command += ["--filter-platform", target]
    if no_default_features:
        command += ["--no-default-features"]
    return json.loads(subprocess.check_output(command))["packages"]


def build_packages(target, no_default_features, packages):
    command = [
        "cargo", "tree", "--locked", "--target", target,
        "--edges", "normal,build", "--prefix", "none", "--format", "{p}",
    ]
    if no_default_features:
        command += ["--no-default-features"]
    used = {tuple(line.split()[:2]) for line in subprocess.check_output(command, text=True).splitlines()}
    return [package for package in packages if (package["name"], f"v{package['version']}") in used]


def check_licenses(packages):
    for package in packages:
        name, expression = package["name"], package["license"]
        terms = set(re.findall(r"[a-zA-Z0-9.+-]+", expression or "")) - {"AND", "OR", "WITH"}
        unknown = terms - PERMISSIVE - REVIEWED_TERMS.get(name, set())
        if not terms or unknown:
            raise SystemExit(f"Review {name} {package['version']} license expression {expression!r}")
        if any(term.startswith(("GPL-", "LGPL-")) for term in terms):
            if expression not in {
                "MIT OR Apache-2.0 OR LGPL-2.1-or-later",
                "Apache-2.0 OR GPL-2.0-only",
                "MIT OR GPL-3.0-only",
            }:
                raise SystemExit(f"Review copyleft alternatives for {name}: {expression}")
    return Counter(package["license"] for package in packages)


@cache
def fetch(url):
    try:
        with urlopen(url, timeout=20) as response:
            return response.read().decode("utf-8").strip()
    except HTTPError as error:
        if error.code in (404, 410):
            return None
        raise


def upstream_license(package, root):
    repository = re.match(r"https?://github.com/([^/]+/[^/]+)", package["repository"] or "")
    vcs = root / ".cargo_vcs_info.json"
    if not repository or not vcs.exists():
        return None
    info = json.loads(vcs.read_text(encoding="utf-8"))
    revision = info.get("git", {}).get("sha1")
    if not revision:
        return None
    base = f"https://raw.githubusercontent.com/{repository[1].removesuffix('.git')}"
    if repository[1].lower() == "emilk/egui":
        revision = package["version"]
    url = f"{base}/{revision}"
    files = ("LICENSE", "LICENSE.md", "LICENSE.txt", "LICENSE-MIT", "LICENSE-APACHE", "LICENSE_ZLIB", "COPYING")
    for filename in files:
        text = fetch(f"{url}/{filename}")
        if text:
            return f"{url}/{filename}", text
    return None


def texts(package):
    root = Path(package["manifest_path"]).parent
    files = {
        path for path in root.rglob("*")
        if path.is_file() and (NOTICE_FILE.fullmatch(path.name) or "licenses" in path.relative_to(root).parts[:-1])
    }
    if package["license_file"]:
        files.add(root / package["license_file"])
    if package["name"] == "epaint_default_fonts":
        files.update(root.glob("fonts/*.txt"))

    entries = [(path.relative_to(root).as_posix(), path.read_text(encoding="utf-8", errors="replace").strip())
               for path in sorted(files) if path.stat().st_size < 500_000]
    if not any(NOTICE_FILE.fullmatch(Path(name).name) and Path(name).name.lower().startswith(("license", "licence", "copying"))
               for name, _ in entries) or package["name"] == "epaint_default_fonts":
        upstream = upstream_license(package, root)
        if upstream:
            entries.append(upstream)
        elif (package["name"], package["version"]) == ("dispatch", "0.2.0"):
            url = "https://raw.githubusercontent.com/spdx/license-list-data/v3.26.0/text/MIT.txt"
            entries.append(("MIT text (declared by dispatch 0.2.0 Cargo.toml)", fetch(url)))
        elif package["name"] in {"winapi-i686-pc-windows-gnu", "winapi-x86_64-pc-windows-gnu"}:
            entries.append(("Apache-2.0 (winapi-rs alternative)", Path("LICENSE").read_text(encoding="utf-8").strip()))
        else:
            raise SystemExit(f"Missing license text for {package['name']} {package['version']}")
    return entries


def write_attributions(output, packages):
    contents = {}
    entries = []
    for package in sorted(packages, key=lambda item: (item["name"], item["version"])):
        name, version, expression = (package[key] for key in ("name", "version", "license"))
        source = f"https://crates.io/api/v1/crates/{name}/{version}/download"
        entries.append(f"{name} {version} | {expression}\n  Corresponding source: {source}")
        for filename, text in texts(package):
            if text:
                number = contents.setdefault(text, len(contents) + 1)
                entries.append(f"  [{number}] {filename}")

    output.write_text(
        "artinchip-flash third-party licenses\n\n"
        "Cargo dependency graph for this target, including build-time dependencies.\n"
        "serialport is available under MPL-2.0 at its exact source URL below.\n"
        "Where offered, permissive alternatives are selected over GPL or LGPL.\n"
        "The native libusb library is LGPL-2.1-or-later. On Linux and macOS it is supplied by the system.\n"
        "On Windows the replaceable DLL and corresponding source archive accompany the programs.\n\n"
        + "\n".join(entries)
        + "\n\n"
        + "\n\n".join(f"===== License or notice {number} =====\n{text}" for text, number in contents.items())
        + "\n",
        encoding="utf-8",
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", help="Rust target triple for the release dependency graph")
    parser.add_argument("--output", type=Path, help="Generate an attribution file for --target")
    parser.add_argument("--no-default-features", action="store_true", help="Audit the CLI-only graph")
    args = parser.parse_args()
    if args.output and not args.target:
        parser.error("--output requires --target")
    packages = metadata(args.target, args.no_default_features)
    licenses = check_licenses(packages)
    print(f"Reviewed {len(packages)} Cargo packages ({len(licenses)} license expressions)")
    for expression, count in sorted(licenses.items()):
        print(f"{count:3}  {expression}")
    if args.output:
        dependencies = build_packages(args.target, args.no_default_features, packages)
        write_attributions(args.output, [package for package in dependencies if package["source"]])


if __name__ == "__main__":
    main()
