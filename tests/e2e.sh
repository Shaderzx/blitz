#!/usr/bin/env bash
# End-to-end proof for blitz: drives the real binary through real cargo builds of the
# two-crate workspace in tests/fixture. Every check prints "ok - <what>" and the script
# stops at the first failure.
#
# Usage: tests/e2e.sh <path to blitz binary>
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <path to blitz binary>" >&2
  exit 2
fi

here=$(cd "$(dirname "$0")" && pwd)
fixture=$here/fixture

case $1 in
  /*) blitz=$1 ;;
  *) blitz=$PWD/$1 ;;
esac
if [ ! -x "$blitz" ]; then
  echo "not ok - no executable blitz binary at $blitz" >&2
  exit 2
fi

work=$(mktemp -d "${TMPDIR:-/tmp}/blitz-e2e.XXXXXX")
trap 'rm -rf "$work"' EXIT

# A private cache and a clean environment: a wrapper, flag or cache directory inherited
# from the caller would change every key and hide what this script is trying to prove.
export BLITZ_CACHE_DIR=$work/cache
export CARGO_TERM_COLOR=never
unset BLITZ_CACHE_RO BLITZ_LOG BLITZ_DISABLE BLITZ_NO_WRAPPER BLITZ_READONLY
unset RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER RUSTFLAGS RUSTDOCFLAGS CARGO_TARGET_DIR

ok() {
  echo "ok - $*"
}

fail() {
  echo "not ok - $*" >&2
  exit 1
}

copy_fixture() {
  mkdir -p "$1"
  cp -R "$fixture/." "$1/"
}

build() {
  # build <dir> [cargo build args...]
  local dir=$1
  shift
  ( cd "$dir" && "$blitz" build --offline "$@" 2>&1 )
}

expect_restored() {
  case $1 in
    *"restored by blitz"*) ;;
    *) fail "$2: expected a whole-build restore; output was: $1" ;;
  esac
}

expect_miss() {
  case $1 in
    *"restored by blitz"*) fail "$2: expected a miss, got a whole-build restore" ;;
  esac
}

expect_output() {
  # expect_output <dir> <profile> <expected first line> <label>
  local out
  out=$("$1/target/$2/app") || fail "$4: the built binary did not run"
  [ "${out%%$'\n'*}" = "$3" ] || fail "$4: the binary printed '${out%%$'\n'*}' as its first line, expected '$3'"
}

expect_line() {
  # expect_line <dir> <profile> <line number> <expected> <label>
  local out
  out=$("$1/target/$2/app") || fail "$5: the built binary did not run"
  local got
  got=$(printf '%s\n' "$out" | sed -n "${3}p")
  [ "$got" = "$4" ] || fail "$5: line $3 of the binary output is '$got', expected '$4'"
}

# 1. A fresh checkout with an empty cache builds the workspace and runs.
first=$work/first
copy_fixture "$first"
out=$(build "$first") || fail "the first build failed: $out"
expect_miss "$out" "the first build"
expect_output "$first" debug fixture-value-one "the first build"
ok "a fresh build misses, compiles and prints the library value"

# 2. Deleting the target directory restores the whole build without compiling.
rm -rf "$first/target"
out=$(build "$first") || fail "the rebuilt checkout failed: $out"
expect_restored "$out" "the second build"
expect_output "$first" debug fixture-value-one "the restored build"
ok "a deleted target directory is restored from the whole-build cache"

# 3. A second checkout at another path shares the cache.
second=$work/second-checkout
copy_fixture "$second"
out=$(build "$second") || fail "the second checkout failed: $out"
expect_restored "$out" "the second checkout"
expect_output "$second" debug fixture-value-one "the second checkout"
ok "a second checkout at a different path hits the same cache"

# 4. Editing the library changes the key and the binary.
cat >"$first/lib/src/lib.rs" <<'EOF'
pub fn value() -> &'static str {
    "fixture-value-two"
}
EOF
out=$(build "$first") || fail "the build after editing the library failed: $out"
expect_miss "$out" "the build after editing the library"
expect_output "$first" debug fixture-value-two "the build after editing the library"
ok "editing the library misses, rebuilds and prints the new value"

# 5. The environment is part of the key.
out=$( cd "$first" && RUSTFLAGS="--cfg blitz_e2e" "$blitz" build --offline 2>&1 ) \
  || fail "the build with RUSTFLAGS failed: $out"
expect_miss "$out" "the build with RUSTFLAGS"
ok "changing RUSTFLAGS misses"

# 6. The per-unit cache works on its own, with cargo driving blitz as RUSTC_WRAPPER and
#    the whole-build cache out of the picture. The second release build after a clean
#    must answer the library from the per-unit cache.
unit=$work/unit
copy_fixture "$unit"
log=$work/unit.log
: >"$log"
( cd "$unit" && BLITZ_LOG=$log RUSTC_WRAPPER=$blitz cargo build --release --offline ) \
  >/dev/null 2>&1 || fail "the first per-unit build failed"
( cd "$unit" && cargo clean ) >/dev/null 2>&1 || fail "cargo clean failed"
: >"$log"
( cd "$unit" && BLITZ_LOG=$log RUSTC_WRAPPER=$blitz cargo build --release --offline ) \
  >/dev/null 2>&1 || fail "the second per-unit build failed"
grep -qE '^hit[[:space:]]+.*fixture[_-]lib' "$log" \
  || fail "no per-unit hit for fixture_lib in BLITZ_LOG: $(tr '\n' ' ' <"$log")"
expect_output "$unit" release fixture-value-one "the per-unit build"
ok "the per-unit cache answers the library after cargo clean and the binary runs"

# 7. A broken build fails with cargo's exit code and stores nothing, so the same broken
#    source fails again instead of being answered from the cache.
broken=$work/broken
copy_fixture "$broken"
cat >"$broken/lib/src/lib.rs" <<'EOF'
pub fn value() -> &'static str {
    "unterminated
}
EOF
status=0
first_out=$( cd "$broken" && "$blitz" build --offline 2>&1 ) || status=$?
[ "$status" -eq 101 ] \
  || fail "a failing build exited $status, expected cargo's 101; output was: $first_out"
status=0
second_out=$( cd "$broken" && "$blitz" build --offline 2>&1 ) || status=$?
[ "$status" -eq 101 ] \
  || fail "the second failing build exited $status instead of failing again with 101; output was: $second_out"
expect_miss "$second_out" "the second failing build"
ok "a failing build exits 101 and is not cached"

# 8. A file the ignore rules hide is still an input: the fixture reads a gitignored data file
#    with include_str!, so editing it has to miss instead of restoring the old binary.
ignored=$work/ignored
copy_fixture "$ignored"
( cd "$ignored" && git init -q . ) || fail "git init failed in the ignored-data fixture"
build "$ignored" >/dev/null || fail "the build with a gitignored data file failed"
out=$(build "$ignored") || fail "the second build with a gitignored data file failed: $out"
expect_restored "$out" "the second build with a gitignored data file"
printf 'fixture-data-two\n' >"$ignored/data/value.txt"
out=$(build "$ignored") || fail "the build after editing the gitignored data file failed: $out"
expect_miss "$out" "the build after editing the gitignored data file"
expect_output "$ignored" debug fixture-value-one "the build after editing the gitignored data file"
expect_line "$ignored" debug 2 "data:fixture-data-two" "the build after editing the gitignored data file"
ok "editing a gitignored file read with include_str! misses and the rebuilt binary prints the new value"

# 9. A build script that watches a directory: adding or editing a file inside it has to miss,
#    and the directory is gitignored so only the recorded tree can see the change.
watched=$work/watched
copy_fixture "$watched"
( cd "$watched" && git init -q . ) || fail "git init failed in the watched-directory fixture"
mkdir -p "$watched/watched"
printf 'one\n' >"$watched/watched/first.txt"
build "$watched" >/dev/null || fail "the build with a watched directory failed"
out=$(build "$watched") || fail "the second build with a watched directory failed: $out"
expect_restored "$out" "the second build with a watched directory"
expect_line "$watched" debug 3 "watched:first.txt" "the second build with a watched directory"
printf 'two\n' >"$watched/watched/second.txt"
out=$(build "$watched") || fail "the build after adding a file to the watched directory failed: $out"
expect_miss "$out" "the build after adding a file to the watched directory"
expect_line "$watched" debug 3 "watched:first.txt,second.txt" "the build after adding a file to the watched directory"
# An empty directory has no file digest of its own, so only a recorded directory entry can see it.
mkdir "$watched/watched/emptydir"
out=$(build "$watched") || fail "the build after adding an empty directory failed: $out"
expect_miss "$out" "the build after adding an empty directory"
expect_line "$watched" debug 3 "watched:emptydir,first.txt,second.txt" "the build after adding an empty directory"
printf 'edited\n' >"$watched/watched/first.txt"
out=$(build "$watched") || fail "the build after editing a file in the watched directory failed: $out"
expect_miss "$out" "the build after editing a file in the watched directory"
expect_line "$watched" debug 3 "watched:emptydir,first.txt,second.txt" "the build after editing a file in the watched directory"
ok "adding a file, an empty directory or an edit in a rerun-if-changed directory misses"

# 10. A read-only tier answers a build without being written: prime a cache, then restore from it
#     with an empty writable directory and BLITZ_READONLY=1, and compare the tier byte for byte.
ro=$work/ro
mkdir -p "$ro/prime" "$ro/empty"
prime=$work/prime
copy_fixture "$prime"
( cd "$prime" && BLITZ_CACHE_DIR="$ro/prime" "$blitz" build --offline ) >/dev/null 2>&1 \
  || fail "priming the read-only cache failed"
before=$( cd "$ro/prime" && find . -type f | sort | xargs shasum | shasum )
reader=$work/reader
copy_fixture "$reader"
out=$( cd "$reader" && BLITZ_CACHE_DIR="$ro/empty" BLITZ_CACHE_RO="$ro/prime" BLITZ_READONLY=1 \
  "$blitz" build --offline 2>&1 ) || fail "the read-only restore failed: $out"
expect_restored "$out" "the read-only restore"
expect_output "$reader" debug fixture-value-one "the read-only restore"
after=$( cd "$ro/prime" && find . -type f | sort | xargs shasum | shasum )
[ "$before" = "$after" ] || fail "the read-only cache directory changed during the restore"
ok "a read-only cache tier restores and stays unchanged"

echo "all e2e checks passed"
