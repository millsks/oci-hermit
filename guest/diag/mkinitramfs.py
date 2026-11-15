"""Diagnostic for #54: write a newc initramfs with one program as /init and a /dev/console node. Temporary."""

import sys
from pathlib import Path


def entry(name: str, mode: int, data: bytes = b"", rdev: tuple[int, int] = (0, 0), ino: int = 1) -> bytes:
    """One newc cpio record, padded to 4 bytes."""
    name_bytes = name.encode() + b"\0"
    fields = [ino, mode, 0, 0, 1, 0, len(data), 0, 0, rdev[0], rdev[1], len(name_bytes), 0]
    header = b"070701" + b"".join(f"{f:08x}".encode() for f in fields)
    record = header + name_bytes
    record += b"\0" * (-len(record) % 4)
    record += data
    record += b"\0" * (-len(record) % 4)
    return record


def main(agent: str, out: str) -> None:
    """Build the archive from the agent binary."""
    parts = [
        entry("dev", 0o040755, ino=1),
        entry("dev/console", 0o020600, rdev=(5, 1), ino=2),
        entry("init", 0o100755, Path(agent).read_bytes(), ino=3),
        entry("TRAILER!!!", 0, ino=0),
    ]
    Path(out).write_bytes(b"".join(parts))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
