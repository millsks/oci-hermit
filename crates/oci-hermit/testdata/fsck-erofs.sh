#!/bin/sh
# Checks the sample `erofs::tests::writes_a_sample_for_fsck_erofs` writes with erofs-utils, the reference EROFS
# tools: fsck.erofs must pass, and the tree it extracts must match the expected listing, contents included.
# Run as root (whiteouts extract as device nodes, owners are kept): sudo sh fsck-erofs.sh <samples dir>
set -eu
image=$1/sample.erofs
expected=$1/sample.expected
fsck.erofs "$image"
work=$(mktemp -d)
fsck.erofs --extract="$work/root" --preserve "$image"
cd "$work/root"
find . -printf '%p %y %m %U %G %s\n' | while read -r path kind mode uid gid size; do
    shown=${path#.}
    [ -n "$shown" ] || shown=/
    if [ "$kind" = f ]; then
        sum=$(sha256sum "$path" | cut -d' ' -f1)
    else
        size=0
        sum=-
    fi
    echo "$shown $kind $mode $uid $gid $size $sum"
done | LC_ALL=C sort >"$work/actual"
LC_ALL=C sort "$expected" | diff -u - "$work/actual"
echo "fsck.erofs: $(wc -l <"$work/actual") entries match"
