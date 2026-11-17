"""Gathers the corresponding source a release must carry for the GPL and LGPL components in its packages.

    python -I packaging/sources.py <version> <packages-dir> <guest-dir> <out-dir>

Reads every ``*.sources.json`` that packaging/package.py wrote in ``<packages-dir>``, downloads each source archive
once (checking its sha256 where the recipe pins one), and adds the patches applied on top of them, taken from the
packages themselves, plus each guest kernel's config from ``<guest-dir>/<arch>/config``. Writes
``<out-dir>/oci-hermit-<version>-sources.tar`` with a ``SOURCES.md`` index at its top.
"""

from __future__ import annotations

import hashlib
import io
import json
import sys
import tarfile
import urllib.request
import zipfile
from pathlib import Path


def fail(message: str) -> None:
    """Stops: a release without its corresponding source must not be published."""
    raise SystemExit(f"sources: {message}")


def fetch(url: str, sha256: str | None) -> bytes:
    """Downloads ``url``, retrying twice, and checks it against ``sha256`` when one is given."""
    last = None
    for _ in range(3):
        try:
            request = urllib.request.Request(url, headers={"User-Agent": "oci-hermit-release"})
            with urllib.request.urlopen(request, timeout=300) as response:
                data = response.read()
            break
        except OSError as e:
            last = e
    else:
        fail(f"{url}: {last}")
    if sha256 and hashlib.sha256(data).hexdigest() != sha256:
        fail(f"{url}: sha256 {hashlib.sha256(data).hexdigest()}, expected {sha256}")
    return data


def package_files(packages: Path) -> dict[str, bytes]:
    """Patch files inside the packages (``.../licenses/<component>/{patches,recipe-patches}/*``), by archive path."""
    found: dict[str, bytes] = {}
    for archive in sorted(packages.glob("*.tar.gz")):
        with tarfile.open(archive) as t:
            for member in t.getmembers():
                parts = Path(member.name).parts
                if member.isfile() and "licenses" in parts and {"patches", "recipe-patches"} & set(parts):
                    found[member.name] = t.extractfile(member).read()
    for archive in sorted(packages.glob("*.zip")):
        with zipfile.ZipFile(archive) as z:
            for name in z.namelist():
                parts = Path(name).parts
                if "licenses" in parts and {"patches", "recipe-patches"} & set(parts) and not name.endswith("/"):
                    found[name] = z.read(name)
    return found


def main(version: str, packages: Path, guests: Path, out: Path) -> None:
    """Writes the sources archive described at the top of this file."""
    components: dict[tuple[str, str], dict] = {}
    hosts: dict[tuple[str, str], list[str]] = {}
    lists = sorted(packages.glob("*.sources.json"))
    if not lists:
        fail(f"no *.sources.json in {packages}")
    for listing in lists:
        host = listing.name.removesuffix(".sources.json")
        for component in json.loads(listing.read_text()):
            key = (component["name"], component["version"])
            components.setdefault(key, component)
            hosts.setdefault(key, []).append(host)

    out.mkdir(parents=True, exist_ok=True)
    tar_path = out / f"oci-hermit-{version}-sources.tar"
    index = [
        f"# Corresponding source for oci-hermit {version}",
        "",
        "The GPL and LGPL components in this release's packages, as built: the upstream source archives, the patches",
        "applied to them (by oci-hermit for QEMU, by conda-forge or MSYS2 for the libraries), and the guest kernels'",
        "configs. oci-hermit's own source is the repository at this release's tag; hermit-agent statically links",
        "glibc and can be relinked from it with `pixi run -e agent agent-build`.",
        "",
        "| Component | Version | License | Packages | Files |",
        "| --- | --- | --- | --- | --- |",
    ]
    with tarfile.open(tar_path, "w") as tar:

        def add(name: str, data: bytes) -> None:
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = 0o644
            tar.addfile(info, io.BytesIO(data))

        seen_urls: set[str] = set()
        for (name, ver), component in sorted(components.items()):
            files = []
            for source in component["sources"]:
                url = source["url"]
                file = f"{name}-{ver}/{url.rstrip('/').rsplit('/', 1)[-1]}"
                if url not in seen_urls:
                    add(file, fetch(url, source.get("sha256")))
                    seen_urls.add(url)
                files.append(f"`{file}`")
            if component.get("patches"):
                files.append(f"`patches/` ({len(component['patches'])})")
            used = ", ".join(sorted(set(hosts[(name, ver)])))
            index.append(f"| {name} | {ver} | {component['license']} | {used} | {', '.join(files)} |")
        for path, data in sorted(package_files(packages).items()):
            parts = Path(path).parts
            at = parts.index("licenses")
            component = parts[at + 1]
            add(f"patches/{component}/{'/'.join(parts[at + 2 :])}", data)
        for config in sorted(guests.glob("*/config")):
            add(f"linux-config/{config.parent.name}.config", config.read_bytes())
            index.append(f"| linux config | {config.parent.name} | | | `linux-config/{config.parent.name}.config` |")
        add("SOURCES.md", ("\n".join(index) + "\n").encode())
    sha = hashlib.sha256(tar_path.read_bytes()).hexdigest()
    (out / f"{tar_path.name}.sha256").write_text(f"{sha}  {tar_path.name}\n")


if __name__ == "__main__":
    if len(sys.argv) != 5:
        fail(__doc__ or "bad arguments")
    main(sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3]), Path(sys.argv[4]))
