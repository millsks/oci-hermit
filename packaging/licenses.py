"""Collects the license files, and where the corresponding source is, for everything oci-hermit ships prebuilt.

Each part records what it bundles in ``<dest>/licenses/``: one folder per component with its license texts (and the
packager's patches, where there are any), plus ``SOURCES.json``, a list of components with their version, license,
and the source archives (URL and sha256) they were built from. ``packaging/package.py`` turns these into a package's
``THIRD-PARTY-LICENSES``, and the release fetches every source a copyleft license asks for.

Subcommands:

  qemu  <bundle> <qemu-source> [patches-dir]   after qemu/build.sh bundled the libraries QEMU loads
  glibc <dest> <agent-env-prefix>              the static agent's C library, from that environment's sysroot

The Linux kernel's entry is written by guest/kernel/build.sh itself, in the same format. Standard library only: it
runs under pixi on Linux and macOS and under MSYS2's Python on Windows.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

LICENSE_NAMES = re.compile(r"^(LICEN[CS]E|COPYING|COPYRIGHT|NOTICE|AUTHORS)([.-].*)?$", re.IGNORECASE)


def fail(message: str) -> None:
    """Stops with a message: a bundle whose licenses are not known must not ship."""
    raise SystemExit(f"licenses: {message}")


def write_sources(dest: Path, components: list[dict]) -> None:
    """Writes ``SOURCES.json``, merging with what is there already."""
    path = dest / "SOURCES.json"
    existing = json.loads(path.read_text()) if path.exists() else []
    names = {c["name"] for c in components}
    merged = [c for c in existing if c["name"] not in names] + components
    path.write_text(json.dumps(sorted(merged, key=lambda c: c["name"]), indent=2) + "\n")


def copy_licenses(src_dir: Path, dest: Path, recursive: bool = False) -> list[str]:
    """Copies the license-looking files of a source tree (top level only unless ``recursive``) into ``dest``."""
    dest.mkdir(parents=True, exist_ok=True)
    found = []
    files = src_dir.rglob("*") if recursive else src_dir.iterdir()
    for file in sorted(files):
        if file.is_file() and (recursive or LICENSE_NAMES.match(file.name)):
            target = dest / file.relative_to(src_dir)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(file, target)
            found.append(str(file.relative_to(src_dir)))
    return found


def recipe_sources(recipe_dir: Path) -> list[dict]:
    """The source archives a conda recipe names: every ``url`` with the ``sha256`` that follows it."""
    for name in ("rendered_recipe.yaml", "meta.yaml", "recipe.yaml"):
        path = recipe_dir / name
        if path.exists():
            text = path.read_text(errors="replace")
            break
    else:
        return []
    # The first `source:` key, top level (conda-build) or nested under `recipe:` (rattler-build's rendered recipe),
    # and the lines below it: more indented, or list items at its own indentation.
    lines = text.splitlines()
    start = next((i for i, line in enumerate(lines) if re.match(r"^\s*source:\s*$", line)), None)
    if start is None:
        return []
    indent = len(lines[start]) - len(lines[start].lstrip())
    block = []
    for line in lines[start + 1 :]:
        depth = len(line) - len(line.lstrip())
        if line.strip() and (depth < indent or (depth == indent and not line.lstrip().startswith("-"))):
            break
        block.append(line)
    sources = []
    for line in block:
        url = re.match(r"^\s*-?\s*url:\s*['\"]?([^'\"\s]+)", line)
        sha = re.match(r"^\s*-?\s*sha256:\s*['\"]?([0-9a-fA-F]{64})", line)
        if url:
            sources.append({"url": url.group(1), "sha256": None})
        elif sha and sources and sources[-1]["sha256"] is None:
            sources[-1]["sha256"] = sha.group(1).lower()
    return [s for s in sources if "${{" not in s["url"] and "{{" not in s["url"]]


def conda_owner(prefix: Path, file_name: str) -> dict:
    """The conda package in ``prefix`` that installed a library named ``file_name``."""
    for meta in sorted((prefix / "conda-meta").glob("*.json")):
        record = json.loads(meta.read_text())
        if any(os.path.basename(f) == file_name for f in record.get("files", [])):
            return record
    fail(f"no conda package in {prefix} installed {file_name}")
    return {}


def downloaded_info(record: dict) -> Path:
    """The ``info/`` folder of a conda package, downloaded from where the environment got it and checked against the
    sha256 the environment recorded. Handles ``.conda`` (a zip holding ``info-*.tar.zst``) and ``.tar.bz2``."""
    import hashlib
    import io
    import tarfile
    import tempfile
    import urllib.request
    import zipfile

    url, expected = record.get("url"), record.get("sha256")
    if not url or not expected:
        fail(f"{record['name']}: no package cache and no URL and sha256 to fetch it from")
    request = urllib.request.Request(url, headers={"User-Agent": "oci-hermit-release"})
    with urllib.request.urlopen(request, timeout=300) as response:
        data = response.read()
    if hashlib.sha256(data).hexdigest() != expected:
        fail(f"{url}: sha256 does not match the environment's record")
    target = Path(tempfile.mkdtemp(prefix=f"{record['name']}-"))
    if url.endswith(".conda"):
        with zipfile.ZipFile(io.BytesIO(data)) as z:
            member = next((n for n in z.namelist() if n.startswith("info-") and n.endswith(".tar.zst")), None)
            if member is None:
                fail(f"{url}: no info-*.tar.zst inside")
            with tarfile.open(fileobj=io.BytesIO(z.read(member)), mode="r:zst") as t:
                t.extractall(target, filter="data")
            # Some packages carry part of info/ (their info/licenses, say) in the payload half.
            payload = next((n for n in z.namelist() if n.startswith("pkg-") and n.endswith(".tar.zst")), None)
            if payload:
                with tarfile.open(fileobj=io.BytesIO(z.read(payload)), mode="r:zst") as t:
                    members = [m for m in t.getmembers() if m.name.startswith("info/")]
                    t.extractall(target, members=members, filter="data")
    else:
        with tarfile.open(fileobj=io.BytesIO(data), mode="r:bz2") as t:
            members = [m for m in t.getmembers() if m.name.startswith("info/")]
            t.extractall(target, members=members, filter="data")
    return target / "info"


def conda_component(record: dict, licenses: Path, installed: list[Path] | None = None) -> dict:
    """Copies a conda package's license files (and its recipe's patches) and describes its source. ``installed``
    names folders in the environment that hold more of its license files, as a sysroot keeps glibc's."""
    package_dir = Path(record.get("extracted_package_dir") or record.get("link", {}).get("source", ""))
    info = package_dir / "info"
    if not info.is_dir():
        # An environment restored from CI's cache has no package cache beside it: fetch the package itself.
        info = downloaded_info(record)
    dest = licenses / record["name"]
    found = copy_licenses(info / "licenses", dest, recursive=True) if (info / "licenses").is_dir() else []
    for folder in installed or []:
        found += copy_licenses(folder, dest, recursive=True)
    if not found:
        fail(f"{record['name']} {record['version']} ships no license files")
    patches = sorted((info / "recipe").rglob("*.patch")) + sorted((info / "recipe").rglob("*.diff"))
    if patches:
        (dest / "recipe-patches").mkdir(exist_ok=True)
        for patch in patches:
            shutil.copyfile(patch, dest / "recipe-patches" / patch.name)
    return {
        "name": record["name"],
        "version": record["version"],
        "license": record.get("license", "unknown"),
        "from": f"conda-forge {record.get('url', record['name'])}",
        "sources": recipe_sources(info / "recipe"),
        "patches": [p.name for p in patches],
    }


def msys2_component(dll: Path, licenses: Path) -> dict:
    """Copies an MSYS2 package's license files and names its source package."""

    def pacman(*args: str) -> str:
        return subprocess.run(["pacman", *args], check=True, capture_output=True, text=True).stdout

    package = pacman("-Qqo", str(dll)).strip()
    version = pacman("-Q", package).split()[1]
    fields = dict(
        line.split(":", 1) for line in pacman("-Qi", package).splitlines() if ":" in line and not line.startswith(" ")
    )
    license_ = fields.get("Licenses", "unknown").strip()
    base = re.sub(r"^mingw-w64-(ucrt|clang|mingw)-[a-z0-9_]+-", "mingw-w64-", package)
    short = re.sub(r"^mingw-w64-(ucrt|clang|mingw)-[a-z0-9_]+-", "", package)
    root = Path(os.environ.get("MINGW_PREFIX", "/ucrt64"))
    dest = licenses / package
    found = []
    for candidate in (root / "share" / "licenses" / short, root / "share" / "licenses" / package):
        if candidate.is_dir():
            found = copy_licenses(candidate, dest, recursive=True)
            break
    if not found:
        fail(f"{package} {version} ships no license files under {root}/share/licenses")
    return {
        "name": package,
        "version": version,
        "license": license_,
        "from": "MSYS2",
        "sources": [{"url": f"https://repo.msys2.org/mingw/sources/{base}-{version}.src.tar.zst", "sha256": None}],
        "patches": [],
    }


def qemu(bundle: Path, source: Path, patches_dir: Path | None) -> None:
    """QEMU itself, its patches, and every library bundled beside it."""
    licenses = bundle / "share" / "licenses"
    if licenses.exists():
        shutil.rmtree(licenses)
    copy_licenses(source, licenses / "qemu")
    version_file = (source / "VERSION").read_text().strip() if (source / "VERSION").exists() else "unknown"
    patches = sorted(patches_dir.glob("*.patch")) if patches_dir else []
    if patches:
        (licenses / "qemu" / "patches").mkdir(parents=True)
        for patch in patches:
            shutil.copyfile(patch, licenses / "qemu" / "patches" / patch.name)
    build_sh = Path(__file__).resolve().parent.parent / "qemu" / "build.sh"
    sha = re.search(r"^QEMU_SHA256=([0-9a-f]{64})", build_sh.read_text(), re.MULTILINE)
    components = [
        {
            "name": "qemu",
            "version": version_file,
            "license": "GPL-2.0-only",
            "from": "https://www.qemu.org",
            "sources": [
                {
                    "url": f"https://download.qemu.org/qemu-{version_file}.tar.xz",
                    "sha256": sha.group(1) if sha else None,
                }
            ],
            "patches": [p.name for p in patches],
        }
    ]
    if os.environ.get("MINGW_PREFIX"):
        libs = sorted(p for p in (bundle / "bin").glob("*.dll"))
        owners: dict[str, dict] = {}
        for dll in libs:
            component = msys2_component(Path(os.environ["MINGW_PREFIX"]) / "bin" / dll.name, licenses)
            owners[component["name"]] = component
    else:
        prefix = Path(os.environ.get("CONDA_PREFIX") or fail("run under pixi run -e qemu-build") or "")
        libs = sorted((bundle / "lib").iterdir()) if (bundle / "lib").is_dir() else []
        owners = {}
        for lib in libs:
            record = conda_owner(prefix, lib.name)
            if record["name"] not in owners:
                owners[record["name"]] = conda_component(record, licenses)
    components.extend(owners.values())
    write_sources(licenses, components)


def glibc(dest: Path, prefix: Path) -> None:
    """The C library statically linked into hermit-agent: the sysroot package of the agent environment at ``prefix``."""
    records = [
        json.loads(m.read_text())
        for m in sorted((prefix / "conda-meta").glob("sysroot_linux-*.json"))
    ]
    if not records:
        fail(f"no sysroot_linux-* package in {prefix}")
    licenses = dest / "licenses"
    # glibc's own COPYING, COPYING.LIB and LICENSES, where the sysroot installs them.
    glibc_licenses = sorted(prefix.glob("*/sysroot/usr/share/licenses/glibc"))
    if not glibc_licenses:
        fail(f"no */sysroot/usr/share/licenses/glibc in {prefix}")
    components = [conda_component(r, licenses, glibc_licenses) for r in records]
    for c in components:
        c["license"] = c["license"] + " (statically linked into hermit-agent; relink from this repository's source)"
        # conda-forge repackages Rocky Linux's binary RPMs; glibc's corresponding source is the matching source RPM,
        # which Rocky keeps in its vault for every released build.
        rpm = next((s["url"] for s in c["sources"] if re.search(r"/glibc-\d[^/]*\.rpm$", s["url"])), "")
        built = re.match(
            r"https://dl\.rockylinux\.org/(?:pub|vault)/rocky/([^/]+)/[^/]+/[^/]+/os/Packages/g/glibc-(.+)\.[a-z0-9_]+\.rpm$",
            rpm,
        )
        if not built:
            fail(f"{c['name']}: cannot tell glibc's source package from {rpm or c['sources']}")
        release, nevr = built.group(1), built.group(2)
        url = f"https://dl.rockylinux.org/vault/rocky/{release}/BaseOS/source/tree/Packages/g/glibc-{nevr}.src.rpm"
        c["sources"] = [{"url": url, "sha256": None}]
        c["from"] += f", built from Rocky Linux {release} glibc {nevr}"
    write_sources(licenses, components)


def main(argv: list[str]) -> None:
    """Dispatches the subcommands documented at the top of this file."""
    match argv:
        case ["qemu", bundle, source]:
            qemu(Path(bundle), Path(source), None)
        case ["qemu", bundle, source, patches]:
            qemu(Path(bundle), Path(source), Path(patches))
        case ["glibc", dest, prefix]:
            glibc(Path(dest), Path(prefix))
        case _:
            fail(__doc__ or "bad arguments")


if __name__ == "__main__":
    main(sys.argv[1:])
