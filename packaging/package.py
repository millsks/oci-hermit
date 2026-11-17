"""Builds one host's ready-to-run oci-hermit package, as CI and the release ship it.

    oci-hermit/
      oci-hermit(.exe)        the host tool
      hrmt(.exe)              the short name: a symlink on Linux and macOS, a copy on Windows (#44)
      qemu/                   the QEMU bundle (qemu/build.sh), with its licenses in qemu/share/licenses/
      guest/<arch>/           kernel and hermit-agent for x86_64 and aarch64, with their licenses
      LICENSE                 oci-hermit's own (Apache-2.0)
      THIRD-PARTY-LICENSES    every bundled component, Rust crate included, with its license text
      README.txt              how to start

Usage:

    python -I packaging/package.py --host osx-arm64 --version 0.1.0 --exe target/release/oci-hermit \\
        --qemu qemu/out --guest guest/out/sandbox --out dist [--archive-name NAME] [--keep-dir]

Writes ``<out>/<name>.tar.gz`` (``.zip`` for win-64) and ``<out>/<name>.sources.json``, the source archives the
release must carry for the copyleft components. ``<name>`` defaults to ``oci-hermit-<version>-<host>``. With
``--keep-dir`` the unpacked ``<out>/oci-hermit/`` stays too.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import stat
import subprocess
import tarfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GUEST_ARCHES = ("x86_64", "aarch64")
WORKSPACE = {"oci-hermit", "hermit-agent", "hermit-proto"}
CRATE_LICENSE_FILES = ("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE", "COPYRIGHT")
AGENT_TARGET = "x86_64-unknown-linux-gnu"

README = """oci-hermit {version} for {host}

Run OCI container images in a sealed, software-emulated Linux VM: no Docker, no admin rights, no hypervisor, no WSL.

  {exe} version
  {exe} machine info
  {exe} run -it public.ecr.aws/docker/library/debian:bookworm-slim bash

`hrmt` is the same program under a shorter name. Everything runs from this folder; nothing needs installing.
Documentation: https://millsks.github.io/oci-hermit/

oci-hermit is Apache-2.0 (LICENSE). It bundles QEMU, a Linux kernel and other components under their own licenses,
listed with their texts in THIRD-PARTY-LICENSES; the release this package came from carries their source.
"""


def fail(message: str) -> None:
    """Stops: a package that is missing a part must not be built."""
    raise SystemExit(f"package: {message}")


def components(licenses_dir: Path) -> list[dict]:
    """The components a ``licenses/`` folder describes (see packaging/licenses.py), with their license files."""
    sources = licenses_dir / "SOURCES.json"
    if not sources.exists():
        fail(f"{sources} is missing: the part was built without its licenses")
    found = []
    for component in json.loads(sources.read_text()):
        folder = licenses_dir / component["name"]
        files = sorted(
            f for f in folder.rglob("*") if f.is_file() and "patches" not in f.relative_to(folder).parts
        ) if folder.is_dir() else []
        if not files:
            fail(f"{component['name']}: no license files in {folder}")
        found.append({**component, "files": files})
    return found


def cargo_crates(package: str, target: str) -> list[dict]:
    """The crates built into ``package`` for ``target``: its normal dependencies, transitively, not build or dev."""
    command = ["cargo", "metadata", "--format-version", "1", "--locked", "--manifest-path", str(ROOT / "Cargo.toml")]
    command += ["--filter-platform", target]
    metadata = json.loads(subprocess.run(command, check=True, capture_output=True, text=True).stdout)
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    root = next(p["id"] for p in metadata["packages"] if p["name"] == package and p["source"] is None)
    seen, pending = set(), [root]
    while pending:
        node = nodes[pending.pop()]
        for dep in node["deps"]:
            if any(kind["kind"] is None for kind in dep["dep_kinds"]) and dep["pkg"] not in seen:
                seen.add(dep["pkg"])
                pending.append(dep["pkg"])
    crates = []
    for crate_id in sorted(seen, key=lambda i: (packages[i]["name"], packages[i]["version"])):
        crate = packages[crate_id]
        if crate["name"] in WORKSPACE:
            continue
        folder = Path(crate["manifest_path"]).parent
        files = sorted(
            f for f in folder.iterdir() if f.is_file() and f.name.upper().startswith(CRATE_LICENSE_FILES)
        )
        if crate.get("license_file"):
            extra = folder / crate["license_file"]
            if extra.is_file() and extra not in files:
                files.append(extra)
        crates.append(
            {
                "name": crate["name"],
                "version": crate["version"],
                "license": crate.get("license") or "see its license file",
                "files": files,
            }
        )
    return crates


def host_target() -> str:
    """The Rust target the toolchain builds for by default: the host's, e.g. aarch64-apple-darwin."""
    info = subprocess.run(["rustc", "-vV"], check=True, capture_output=True, text=True).stdout
    return next(line.split(": ", 1)[1] for line in info.splitlines() if line.startswith("host: "))


def third_party_licenses(host: str, version: str, parts: list[tuple[str, list[dict]]]) -> str:
    """THIRD-PARTY-LICENSES: a table of contents, then each distinct license text once, naming what it covers."""
    lines = [
        f"oci-hermit {version} for {host}: third-party components",
        "",
        "oci-hermit itself is Apache-2.0 (see LICENSE). This package also contains the components below, each under its",
        "own license. Their license texts follow, each distinct text once with the components it covers. For every",
        "component under the GPL or LGPL, the GitHub release this package came from carries the corresponding source",
        "(the archives listed in its *.sources.json), including the patches applied to it.",
        "",
    ]
    texts: dict[str, tuple[str, list[str]]] = {}
    for title, items in parts:
        lines.append(title)
        for item in items:
            label = f"{item['name']} {item['version']}"
            lines.append(f"  {label:<48} {item['license']}")
            if not item["files"]:
                key = f"no-file:{item['license']}"
                texts.setdefault(key, (f"(The crate ships no license file; its license is {item['license']}.)", []))
                texts[key][1].append(label)
            for file in item["files"]:
                content = file.read_text(errors="replace").strip()
                key = hashlib.sha256(content.encode()).hexdigest()
                texts.setdefault(key, (content, []))
                texts[key][1].append(f"{label} ({file.name})")
        lines.append("")
    for content, users in texts.values():
        lines += ["=" * 100, "Applies to:"]
        lines += [f"  {user}" for user in users]
        lines += ["-" * 100, content, ""]
    return "\n".join(lines) + "\n"


def build(args: argparse.Namespace) -> None:
    """Assembles the folder, writes the license and source lists, and archives it."""
    windows = args.host.startswith("win")
    exe = "oci-hermit.exe" if windows else "oci-hermit"
    out = Path(args.out).resolve()
    stage = out / "oci-hermit"
    if stage.exists():
        shutil.rmtree(stage)
    stage.mkdir(parents=True)

    binary = Path(args.exe)
    if not binary.is_file():
        fail(f"no oci-hermit binary at {binary}")
    shutil.copy2(binary, stage / exe)
    if windows:
        shutil.copy2(binary, stage / "hrmt.exe")
    else:
        (stage / "hrmt").symlink_to(exe)

    qemu = Path(args.qemu)
    if not (qemu / "BUNDLE.txt").is_file():
        fail(f"{qemu} is not a QEMU bundle")
    shutil.copytree(qemu, stage / "qemu", symlinks=True)
    parts = [("QEMU and the libraries it loads (qemu/)", components(stage / "qemu" / "share" / "licenses"))]

    guest_sources: dict[str, dict] = {}
    for arch in GUEST_ARCHES:
        src = Path(args.guest) / arch
        for name in ("kernel", "hermit-agent"):
            if not (src / name).is_file():
                fail(f"no {arch} {name} in {src}")
        shutil.copytree(src, stage / "guest" / arch)
        for component in components(stage / "guest" / arch / "licenses"):
            guest_sources.setdefault(f"{component['name']} {component['version']}", component)
        os.chmod(stage / "guest" / arch / "hermit-agent", 0o755)
    parts.append(("The guest: Linux kernel and the C library in hermit-agent (guest/)", list(guest_sources.values())))
    target = args.target or host_target()
    parts.append(("Rust crates in oci-hermit", cargo_crates("oci-hermit", target)))
    parts.append(("Rust crates in hermit-agent", cargo_crates("hermit-agent", AGENT_TARGET)))

    shutil.copyfile(ROOT / "LICENSE", stage / "LICENSE")
    (stage / "THIRD-PARTY-LICENSES").write_text(third_party_licenses(args.host, args.version, parts))
    launch = r".\oci-hermit\oci-hermit.exe" if windows else "./oci-hermit/oci-hermit"
    readme = README.format(version=args.version, host=args.host, exe=launch)
    (stage / "README.txt").write_text(readme.replace("\n", "\r\n") if windows else readme)
    if not windows:
        os.chmod(stage / exe, os.stat(stage / exe).st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)

    name = args.archive_name or f"oci-hermit-{args.version}-{args.host}"
    copyleft = []
    for _, items in parts[:2]:
        for item in items:
            if "GPL" in item["license"] and item.get("sources"):
                copyleft.append({k: item[k] for k in ("name", "version", "license", "sources", "patches", "from")})
    (out / f"{name}.sources.json").write_text(json.dumps(copyleft, indent=2) + "\n")

    if windows:
        archive = out / f"{name}.zip"
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
            for file in sorted(stage.rglob("*")):
                z.write(file, file.relative_to(out))
    else:
        archive = out / f"{name}.tar.gz"
        def anonymous(info: tarfile.TarInfo) -> tarfile.TarInfo:
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            return info

        with tarfile.open(archive, "w:gz") as t:
            t.add(stage, arcname="oci-hermit", filter=anonymous)
    if not args.keep_dir:
        shutil.rmtree(stage)
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    (out / f"{archive.name}.sha256").write_text(f"{digest}  {archive.name}\n")


def main() -> None:
    """Parses the command line and builds the package."""
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", required=True, choices=["win-64", "osx-arm64", "osx-64", "linux-64", "linux-aarch64"])
    parser.add_argument("--version", required=True)
    parser.add_argument("--exe", required=True)
    parser.add_argument("--qemu", required=True)
    parser.add_argument("--guest", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--target", help="the Rust target oci-hermit was built for (default: the host's)")
    parser.add_argument("--archive-name")
    parser.add_argument("--keep-dir", action="store_true")
    build(parser.parse_args())


if __name__ == "__main__":
    main()
