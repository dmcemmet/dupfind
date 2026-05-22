#!/usr/bin/env bash
set -e

BINARY="$(cd "$(dirname "$0")" && pwd)/target/release/dupfinder"
TESTDIR="$(mktemp -d)"
trap "rm -rf '$TESTDIR'; rm -rf ~/.dupfinder/" EXIT

PASS=0
FAIL=0

assert_eq() {
    local desc="$1" expected="$2" actual="$3"
    if [ "$expected" = "$actual" ]; then
        echo "  ✓ $desc"
        PASS=$((PASS + 1))
    else
        echo "  ✗ $desc (expected: $expected, got: $actual)"
        FAIL=$((FAIL + 1))
    fi
}

assert_contains() {
    local desc="$1" needle="$2" haystack="$3"
    if echo "$haystack" | grep -q "$needle"; then
        echo "  ✓ $desc"
        PASS=$((PASS + 1))
    else
        echo "  ✗ $desc (expected to contain: $needle)"
        FAIL=$((FAIL + 1))
    fi
}

get_groups() { echo "$1" | grep "Found" | awk -F'Found ' '{print $2}' | awk '{print $1}'; }
get_dupes() { echo "$1" | grep "Found" | awk -F'(' '{print $2}' | awk '{print $1}'; }

echo "=== Building... ==="
cargo build --release --quiet

echo "=== Generating test data in $TESTDIR ==="
mkdir -p "$TESTDIR/dir_a/photos" "$TESTDIR/dir_a/docs" "$TESTDIR/dir_a/deep/nested/path"
mkdir -p "$TESTDIR/dir_b/backup" "$TESTDIR/dir_b/archive"

# Small duplicates within dir_a (group 1: 3 files)
echo "hello world" > "$TESTDIR/dir_a/file1.txt"
echo "hello world" > "$TESTDIR/dir_a/file1_copy.txt"
echo "hello world" > "$TESTDIR/dir_a/docs/file1_another.txt"

# Small duplicates within dir_b (group 2: 2 files)
echo "backup data" > "$TESTDIR/dir_b/backup/note.txt"
echo "backup data" > "$TESTDIR/dir_b/archive/note_old.txt"

# Cross-directory duplicates (group 3: 2 files)
echo "shared content across dirs" > "$TESTDIR/dir_a/shared.txt"
echo "shared content across dirs" > "$TESTDIR/dir_b/shared_copy.txt"

# Large duplicates 100KB (group 4: 3 files)
dd if=/dev/urandom bs=1024 count=100 of="$TESTDIR/dir_a/photos/big_photo.bin" 2>/dev/null
cp "$TESTDIR/dir_a/photos/big_photo.bin" "$TESTDIR/dir_a/photos/big_photo_copy.bin"
cp "$TESTDIR/dir_a/photos/big_photo.bin" "$TESTDIR/dir_b/backup/big_backup.bin"

# Medium duplicates 10KB (group 5: 2 files)
dd if=/dev/urandom bs=1024 count=10 of="$TESTDIR/dir_a/medium.bin" 2>/dev/null
cp "$TESTDIR/dir_a/medium.bin" "$TESTDIR/dir_a/deep/nested/path/medium_dup.bin"

# Same size, different content (should NOT group)
dd if=/dev/urandom bs=1024 count=100 of="$TESTDIR/dir_a/different1.bin" 2>/dev/null
dd if=/dev/urandom bs=1024 count=100 of="$TESTDIR/dir_a/different2.bin" 2>/dev/null

# Same head+tail, different middle (should NOT group)
python3 -c "
head = b'H' * 4096; tail = b'T' * 4096
open('$TESTDIR/dir_a/tricky1.bin','wb').write(head + b'A'*92160 + tail)
open('$TESTDIR/dir_a/tricky2.bin','wb').write(head + b'B'*92160 + tail)
"

# Unique files
echo "unique a" > "$TESTDIR/dir_a/unique.txt"
dd if=/dev/urandom bs=512 count=1 of="$TESTDIR/dir_a/unique_size.bin" 2>/dev/null

# Empty files (should be ignored)
touch "$TESTDIR/dir_a/empty1.txt" "$TESTDIR/dir_b/empty2.txt"

echo ""
echo "=== Test 1: Single directory, full hash ==="
rm -rf ~/.dupfinder/
OUT=$("$BINARY" "$TESTDIR" --dry-run 2>&1)
assert_eq "5 groups found" "5" "$(get_groups "$OUT")"
assert_eq "12 duplicate files" "12" "$(get_dupes "$OUT")"
TRICKY_IN_GROUPS=$(echo "$OUT" | grep "tricky" | grep -v "Scanned\|Found" || true)
assert_eq "tricky files not grouped" "" "$TRICKY_IN_GROUPS"

echo ""
echo "=== Test 2: Single directory, --fast ==="
rm -rf ~/.dupfinder/
OUT=$("$BINARY" "$TESTDIR" --fast --dry-run 2>&1)
assert_eq "5 groups found (fast)" "5" "$(get_groups "$OUT")"
assert_eq "12 duplicate files (fast)" "12" "$(get_dupes "$OUT")"

echo ""
echo "=== Test 3: Two directories, full hash ==="
rm -rf ~/.dupfinder/
OUT=$("$BINARY" "$TESTDIR/dir_a" "$TESTDIR/dir_b" --dry-run 2>&1)
assert_eq "5 groups found (multi-root)" "5" "$(get_groups "$OUT")"
assert_eq "12 duplicate files (multi-root)" "12" "$(get_dupes "$OUT")"
assert_contains "cross-dir duplicate found" "shared" "$OUT"
assert_contains "big cross-dir duplicate found" "big_backup" "$OUT"

echo ""
echo "=== Test 4: Two directories, --fast ==="
rm -rf ~/.dupfinder/
OUT=$("$BINARY" "$TESTDIR/dir_a" "$TESTDIR/dir_b" --fast --dry-run 2>&1)
assert_eq "5 groups found (multi-root fast)" "5" "$(get_groups "$OUT")"
assert_eq "12 duplicate files (multi-root fast)" "12" "$(get_dupes "$OUT")"

echo ""
echo "=== Test 5: Exclude pattern ==="
rm -rf ~/.dupfinder/
OUT=$("$BINARY" "$TESTDIR" --exclude "photos" --dry-run 2>&1)
BIG_PHOTO=$(echo "$OUT" | grep "big_photo" || true)
assert_eq "big_photo excluded" "" "$BIG_PHOTO"

echo ""
echo "=== Test 6: Min-size filter ==="
rm -rf ~/.dupfinder/
OUT=$("$BINARY" "$TESTDIR" --min-size 1KB --dry-run 2>&1)
# Count actual Group headers in output (post-filter)
SHOWN_GROUPS=$(echo "$OUT" | grep -c "^Group " || true)
assert_eq "only large groups shown with min-size" "2" "$SHOWN_GROUPS"

echo ""
echo "=== Test 7: --fast with 1 sample (default) ==="
rm -rf ~/.dupfinder/
OUT=$("$BINARY" "$TESTDIR" --fast --dry-run 2>&1)
assert_eq "5 groups (fast 1)" "5" "$(get_groups "$OUT")"

echo ""
echo "=== Test 8: --fast 3 (3 middle samples) ==="
rm -rf ~/.dupfinder/
OUT=$("$BINARY" "$TESTDIR" --fast 3 --dry-run 2>&1)
assert_eq "5 groups (fast 3)" "5" "$(get_groups "$OUT")"

echo ""
echo "=== Test 9: --fast 10 on small files (should cap samples) ==="
rm -rf ~/.dupfinder/
# Create 20KB duplicates: middle region = 20KB - 8KB = 12KB = 3 chunks max
dd if=/dev/urandom bs=1024 count=20 of="$TESTDIR/small20k_a.bin" 2>/dev/null
cp "$TESTDIR/small20k_a.bin" "$TESTDIR/small20k_b.bin"
# Create a 20KB file that differs at offset 8000 (within a middle sample region)
cp "$TESTDIR/small20k_a.bin" "$TESTDIR/small20k_diff.bin"
python3 -c "
f = open('$TESTDIR/small20k_diff.bin', 'r+b')
f.seek(8000)  # middle region with 3 samples: offsets ~7168, ~10240, ~13312
f.write(b'XXXX')
f.close()
"
OUT=$("$BINARY" "$TESTDIR/small20k_a.bin" "$TESTDIR/small20k_b.bin" "$TESTDIR/small20k_diff.bin" --fast 10 --dry-run 2>&1 || true)
# With --fast 10 on 20KB files, it caps to 3 samples. The diff file differs at offset 5000
# which is within the first middle sample region, so it should be caught
# But we're passing files as args not dirs - let's use the dir instead
rm -rf ~/.dupfinder/
OUT=$("$BINARY" "$TESTDIR" --fast 10 --dry-run 2>&1)
# small20k_a and small20k_b should be grouped, small20k_diff should NOT
SMALL_GROUP=$(echo "$OUT" | grep -A3 "20.0 KB" | grep "small20k" | wc -l | tr -d ' ')
assert_eq "fast 10 finds 20KB duplicates" "2" "$SMALL_GROUP"
# Verify small20k_diff is not in any group
DIFF_IN_GROUP=$(echo "$OUT" | grep "small20k_diff" || true)
assert_eq "fast 10 excludes different 20KB file" "" "$DIFF_IN_GROUP"

echo ""
echo "================================"
echo "Results: $PASS passed, $FAIL failed"
if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
echo "All tests passed!"
