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
  # expect_output <dir> <profile> <expected> <label>
  local out
  out=$("$1/target/$2/app") || fail "$4: the built binary did not run"
  [ "$out" = "$3" ] || fail "$4: the binary printed '$out', expected '$3'"
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

echo "all e2e checks passed"
