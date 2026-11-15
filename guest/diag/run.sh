#!/usr/bin/env bash
# Diagnostic for #54: boot one program as /init under x86_64 microvm with a given CPU model, and report whether its
# console line appeared (ready), init died (panic, with the faulting address), or nothing happened (silent, with
# instruction-pointer samples from the QEMU monitor). Temporary; not merged.
#
# Usage: guest/diag/run.sh <name> <program> <qemu-bundle-dir> <kernel> <cpu-model> <work-dir> [timeout-seconds]
set -euo pipefail

name=$1 program=$2 qemu=$3 kernel=$4 cpu=$5 work=$6 limit=${7:-30}
mkdir -p "$work"
initrd="$work/$name.cpio"
log="$work/$name-$cpu.log"
monitor="$work/m.sock"
pixi run -e qemu-build python -I guest/diag/mkinitramfs.py "$program" "$initrd"
rm -f "$log" "$monitor"

"$qemu/bin/qemu-system-x86_64" -L "$qemu/share/qemu" -M microvm -cpu "$cpu" -accel tcg -m 256 -display none \
  -nodefaults -no-reboot -serial "file:$log" -monitor "unix:$monitor,server,nowait" -kernel "$kernel" \
  -initrd "$initrd" -append "console=ttyS0 panic=-1 print-fatal-signals=1" > "$work/$name-$cpu.qemu" 2>&1 &
pid=$!

result=silent
for _ in $(seq 1 "$limit"); do
  if grep -q 'hermit-agent: ready' "$log" 2> /dev/null; then result=ready; break; fi
  if grep -q 'Kernel panic' "$log" 2> /dev/null; then result=panic; break; fi
  kill -0 "$pid" 2> /dev/null || { result=exited; break; }
  sleep 1
done

samples=""
if [ "$result" = silent ]; then
  for _ in 1 2 3; do
    samples="$samples $(printf 'info registers\n' | timeout 2 nc -U "$monitor" 2> /dev/null | grep -aoE 'RIP=[0-9a-f]+ RFL=[0-9a-f]+|CPL=[0-9]' | tr '\n' ' ')"
    sleep 1
  done
fi
kill "$pid" 2> /dev/null || true
wait "$pid" 2> /dev/null || true

trap_line=$(grep -m1 -oE 'trap [a-z ]+ ip:[0-9a-f]+' "$log" || true)
echo "RESULT $name cpu=$cpu: $result ${trap_line:+($trap_line)}${samples:+ samples:$samples}"
grep -E 'hermit-agent|traps:|Kernel panic|Run /init' "$log" | head -8 | sed 's/^/  /' || true
