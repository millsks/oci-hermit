#!/usr/bin/env bash
# Boot a guest kernel under the bundled QEMU, with no initramfs and no root disk, and check that it gets as far as
# looking for init: the console must show the kernel's "No working init found" or "Unable to mount root fs" panic.
#
# Usage: guest/kernel/boot-test.sh <x86_64|aarch64> <qemu-bundle-dir> <kernel-image> [timeout-seconds]
#
# Runs on any host the QEMU bundle runs on. Written for bash 3.2, which macOS ships; it has no `timeout`, so the
# deadline is enforced here.
set -euo pipefail

arch=${1:?usage: guest/kernel/boot-test.sh <arch> <qemu-bundle-dir> <kernel-image> [timeout-seconds]}
qemu=${2:?missing QEMU bundle directory}
kernel=${3:?missing kernel image}
limit=${4:-300}

case "$arch" in
  x86_64)
    cmd=("$qemu/bin/qemu-system-x86_64" -L "$qemu/share/qemu" -M microvm)
    console="console=ttyS0 earlyprintk=serial,ttyS0"
    ;;
  aarch64)
    cmd=("$qemu/bin/qemu-system-aarch64" -M virt -cpu max)
    console="console=ttyAMA0 earlycon"
    ;;
  *) echo "unsupported architecture: $arch (x86_64 or aarch64)" >&2; exit 1 ;;
esac
# -nodefaults: no default NIC (whose option ROM the bundle does not ship) or other devices; just the serial console.
cmd+=(-accel tcg -m 256 -nodefaults -nographic -serial stdio -no-reboot -kernel "$kernel" -append "$console panic=-1")

reached='Kernel panic - not syncing: (No working init found|VFS: Unable to mount root fs)'
log=$(mktemp)
trap 'rm -f "$log"' EXIT

echo "+ ${cmd[*]}"
"${cmd[@]}" < /dev/null > "$log" 2>&1 &
pid=$!

result=timeout
elapsed=0
while [ "$elapsed" -lt "$limit" ]; do
  if grep -Eq "$reached" "$log"; then
    result=booted
    break
  fi
  if ! kill -0 "$pid" 2> /dev/null; then
    # QEMU exited (panic=-1 with -no-reboot); look once more in case the line landed just before.
    grep -Eq "$reached" "$log" && result=booted || result=exited
    break
  fi
  sleep 1
  elapsed=$((elapsed + 1))
done
kill "$pid" 2> /dev/null || true
wait "$pid" 2> /dev/null || true

cat "$log"
echo
case "$result" in
  booted)
    echo "boot test passed after ${elapsed}s: $(grep -Eo "$reached[^\"]*" "$log" | head -1)"
    ;;
  exited)
    echo "boot test failed: QEMU exited before the kernel looked for init" >&2
    exit 1
    ;;
  timeout)
    echo "boot test failed: the kernel did not reach init within ${limit}s" >&2
    exit 1
    ;;
esac
