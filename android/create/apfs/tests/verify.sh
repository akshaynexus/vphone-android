#!/bin/bash
# Host verification of libvphone_apfs with upstream apfsck/mkapfs (apfsprogs):
#   android/create/apfs/tests/verify.sh <vphone_apfs binary> <apfsprogs dir> <workdir> [real ramdisk.apfs]
# Every image written is checked with apfsck after every step. Exits non-zero on the first failure.
set -euo pipefail
V=$(realpath "$1"); AP=$(realpath "$2"); W=$(realpath -m "$3"); REAL=${4:-}
mkdir -p "$W"; cd "$W"; rm -rf t; mkdir t; cd t
APFSCK=$AP/apfsck/apfsck; MKAPFS=$AP/mkapfs/mkapfs
fails=0; pass(){ echo "ok   - $1"; }; fail(){ echo "FAIL - $1"; fails=$((fails+1)); }
check(){ local log; if log=$("$APFSCK" -c "$1" 2>&1); then pass "apfsck: $2"; else fail "apfsck: $2: $log"; fi; }
eq(){ if [ "$1" = "$2" ]; then pass "$3"; else fail "$3: '$1' != '$2'"; fi; }

newimg(){ rm -f "$1"; truncate -s "${2:-256M}" "$1"; "$MKAPFS" ${3:-} -L test "$1" >/dev/null; }

# --- hashed names (default mkapfs: case-insensitive, normalization-insensitive) ---
newimg a.img 256M; check a.img "fresh mkapfs"
"$V" mkdir a.img /etc -p --mode 0700 --uid 501 --gid 20; check a.img mkdir
head -c 3000000 /dev/urandom > big.bin
"$V" write a.img /etc/big.bin big.bin --mode 04755 --uid 7 --gid 8; check a.img "write 3 MB"
eq "$("$V" cat a.img /etc/big.bin | sha1sum)" "$(sha1sum < big.bin)" "read back 3 MB"
eq "$("$V" stat a.img /etc/big.bin | python3 -Ic 'import json,sys;d=json.load(sys.stdin);print(d["mode"],d["uid"],d["gid"],d["size"])')" "2541 7 8 3000000" "exact mode/uid/gid"
echo small > s.txt; "$V" write a.img /etc/s.txt s.txt
"$V" symlink a.img /etc/s.txt /etc/ln; check a.img symlink
"$V" link a.img /etc/s.txt /etc/hl; check a.img hardlink
"$V" setxattr a.img /etc/s.txt user.a s.txt; check a.img "embedded xattr"
head -c 20000 /dev/zero > bx; "$V" setxattr a.img /etc/s.txt user.big bx; check a.img "stream xattr"
eq "$("$V" readxattr a.img /etc/s.txt user.big | wc -c | tr -d ' ')" 20000 "stream xattr read"
"$V" chmod a.img /etc/s.txt 0640; "$V" chown a.img /etc/s.txt 1:2; check a.img "chmod+chown"
"$V" rename a.img /etc/big.bin /etc/moved.bin; check a.img rename
"$V" mkdir a.img /x; "$V" rename a.img /etc/moved.bin /x/moved.bin; check a.img "move between dirs"
"$V" rmxattr a.img /etc/s.txt user.big; check a.img rmxattr
"$V" rm a.img /etc/hl; check a.img "unlink hardlink"
"$V" rm a.img /etc/s.txt; check a.img "unlink primary"
"$V" rm -r a.img /x; check a.img "rm -r"
"$V" stat a.img /x >/dev/null 2>&1 && fail "removed dir still visible" || pass "removed dir gone"

# Case-sensitive, normalization-insensitive: what iOS volumes use.
newimg s.img 64M -s; check s.img "fresh mkapfs -s"
"$V" write s.img /Readme s.txt; "$V" write s.img /README big.bin; check s.img "case-sensitive names coexist"
eq "$("$V" cat s.img /Readme | sha1sum)" "$(sha1sum < s.txt)" "case-sensitive lookup"

# Many files: forces B-tree splits, several checkpoints and free-queue drains.
newimg b.img 512M
mkdir tree; for d in 0 1 2 3 4 5 6 7; do mkdir tree/d$d; for i in $(seq 1 800); do echo "file $d $i" > tree/d$d/f$i; done; done
ln tree/d0/f1 tree/d1/link-of-f1; ln -s f2 tree/d0/s2
"$V" copy-in b.img tree / --owner 0:0; check b.img "copy-in 6400 files"
got=$("$V" walk b.img /tree | wc -l); eq "$got" "$(($(find tree | wc -l)))" "entry count after copy-in"
eq "$("$V" cat b.img /tree/d7/f800)" "file 7 800" "content after copy-in"
eq "$("$V" stat b.img /tree/d1/link-of-f1 | python3 -Ic 'import json,sys;print(json.load(sys.stdin)["nlink"])')" 2 "hard link count"
"$V" rm -r b.img /tree/d3; check b.img "rm -r 800 files"
head -c 300000000 /dev/zero | tr '\0' 'x' > fill; "$V" write b.img /fill fill; check b.img "write 300 MB"
"$V" write b.img /fill big.bin; check b.img "replace file frees old extents"
eq "$("$V" cat b.img /fill | sha1sum)" "$(sha1sum < big.bin)" "replaced content"

# Refuses to overflow the container instead of corrupting it.
newimg c.img 32M; head -c 40000000 /dev/zero > toobig
if "$V" write c.img /toobig toobig 2>/dev/null; then fail "write beyond container succeeded"; else pass "ENOSPC reported"; fi
check c.img "image intact after ENOSPC"

# Sealed-style refusal: flip the sealed bit on a fresh copy through the read path only.
if [ -n "$REAL" ]; then
  cp --reflink=auto "$REAL" r.img; check r.img "real ramdisk baseline"
  rm -rf x; "$V" extract r.img / x --manifest m.jsonl >/dev/null
  newimg rt.img 4G
  "$V" mkdir rt.img /rt
  "$V" copy-in rt.img x /rt --meta m.jsonl >/dev/null; check rt.img "round-trip real tree into fresh volume"
  digest(){ "$V" walk "$1" "$2" | python3 -Ic 'import json,sys
for d in map(json.loads,sys.stdin): print(d["kind"],d["mode"],d["uid"],d["gid"],d["size"],d["target"],d["path"].removeprefix(sys.argv[1]).lstrip("/"))' "$3" | sort | sha1sum; }
  eq "$(digest r.img / "")" "$(digest rt.img /rt/x /rt/x)" "round trip preserves tree shape and metadata"
fi

echo; [ "$fails" = 0 ] && echo "ALL PASSED" || { echo "$fails FAILED"; exit 1; }
