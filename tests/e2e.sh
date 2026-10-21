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
grep -qE '^hit[[:space:]]+build_script_build' "$log" \
  || fail "no per-unit hit for build_script_build in BLITZ_LOG: $(tr '\n' ' ' <"$log")"
expect_output "$unit" release fixture-value-one "the per-unit build"
ok "the per-unit cache answers the library and the build script after cargo clean and the binary runs"

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

# 11. Deleting the incremental state of a tree that has built keeps the cache, so the next compile of
#     the edited library is seeded from the stored session. The same tree, not a copy: rustc discards
#     a session whose command line differs, so a copy at another path would only prove where the files
#     were put.
seed_cache=$work/seed-cache
seed_tree=$work/seed-tree
copy_fixture "$seed_tree"
mkdir -p "$seed_tree/watched"
printf 'one\n' >"$seed_tree/watched/first.txt"
( cd "$seed_tree" && BLITZ_CACHE_DIR=$seed_cache BLITZ_INCREMENTAL_SNAPSHOT=always \
  RUSTC_WRAPPER=$blitz cargo build --offline ) >/dev/null 2>&1 \
  || fail "the debug build that fills the incremental cache failed"
rm -rf "$seed_tree/target/debug/incremental"
cat >"$seed_tree/lib/src/lib.rs" <<'EOF'
pub fn value() -> &'static str {
    "fixture-value-seeded"
}
EOF
seed_log=$work/seed.log
: >"$seed_log"
( cd "$seed_tree" && BLITZ_CACHE_DIR=$seed_cache BLITZ_LOG=$seed_log RUSTC_WRAPPER=$blitz \
  cargo build --offline ) >/dev/null 2>&1 || fail "the seeded debug build failed"
grep -qE '^seed[[:space:]]+fixture[_-]lib' "$seed_log" \
  || fail "no seed line for fixture_lib in BLITZ_LOG: $(tr '\n' ' ' <"$seed_log")"
expect_output "$seed_tree" debug fixture-value-seeded "the seeded debug build"
ok "deleting the incremental state of a built tree seeds the edited library and prints the new value"

# 12. A build script of a registry crate runs through the cache. The dependency sits in an unpacked
#     registry extraction, the one place cargo treats sources as immutable, which is what the wrapper
#     keys the shim on. The compile leaves a shim in front of the executable and the real binary next
#     to it; after `rm -rf target` the unit cache restores both and the run itself is restored from
#     the run cache (`bsrun-hit`), so cargo sees the same OUT_DIR and the binary prints the same thing
#     as a clean build.
bsrun=$work/bsrun
bsrun_home=$bsrun/cargo-home
bsrun_dep=$bsrun_home/registry/src/test-index/bsrun-dep-1.0.0
bsrun_log=$work/bsrun.log
mkdir -p "$bsrun_dep/src" "$bsrun/app/src"
: >"$bsrun_dep/.cargo-ok"
cat >"$bsrun_dep/Cargo.toml" <<'EOF'
[package]
name = "bsrun-dep"
version = "1.0.0"
edition = "2021"
EOF
cat >"$bsrun_dep/build.rs" <<'EOF'
use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    let out = std::env::var("OUT_DIR").expect("OUT_DIR is set for a build script");
    fs::write(format!("{out}/value.txt"), "build-script-value").expect("write the build script output");
    // An empty directory has no file to carry it back from the cache.
    fs::create_dir(format!("{out}/emptydir")).expect("create the empty directory");
}
EOF
cat >"$bsrun_dep/src/lib.rs" <<'EOF'
pub const VALUE: &str = include_str!(concat!(env!("OUT_DIR"), "/value.txt"));
EOF
cat >"$bsrun/app/Cargo.toml" <<EOF
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
bsrun-dep = { path = "$bsrun_dep" }
EOF
cat >"$bsrun/app/src/main.rs" <<'EOF'
fn main() {
    println!("{}", bsrun_dep::VALUE);
}
EOF
cat >"$bsrun/Cargo.toml" <<'EOF'
[workspace]
members = ["app"]
resolver = "2"
EOF
# The path dependency is a local package, so cargo would compile the build script with `-C
# incremental` and the per-unit cache (which installs the shim) would not see it. Registry crates in
# a real build are not incremental; turn it off so this check exercises that path.
( cd "$bsrun" && CARGO_HOME=$bsrun_home CARGO_INCREMENTAL=0 BLITZ_CACHE_DIR=$work/bsrun-cache \
  RUSTC_WRAPPER=$blitz cargo build --offline ) >/dev/null 2>&1 \
  || fail "the first registry build-script build failed"
first_value=$("$bsrun/target/debug/app") || fail "the registry build-script binary did not run"
[ "$first_value" = "build-script-value" ] \
  || fail "the clean build printed '$first_value', expected 'build-script-value'"
shim=$(find "$bsrun/target/debug/build" -name 'build_script_*' ! -name '*.d' ! -name '*.blitz-real' | head -1)
[ -n "$shim" ] || fail "no build script executable under target/debug/build"
[ "$(head -1 "$shim")" = '#!/bin/sh' ] \
  || fail "the build script executable is not the blitz shim: $(head -3 "$shim")"
[ -x "$shim.blitz-real" ] || fail "the real build script was not kept next to the shim"
rm -rf "$bsrun/target"
: >"$bsrun_log"
( cd "$bsrun" && CARGO_HOME=$bsrun_home CARGO_INCREMENTAL=0 BLITZ_CACHE_DIR=$work/bsrun-cache \
  BLITZ_LOG=$bsrun_log RUSTC_WRAPPER=$blitz cargo build --offline ) >/dev/null 2>&1 \
  || fail "the second registry build-script build failed"
grep -qE '^bsrun-hit[[:space:]]' "$bsrun_log" \
  || fail "no bsrun-hit line in BLITZ_LOG: $(tr '\n' ' ' <"$bsrun_log")"
find "$bsrun/target/debug/build" -type d -name emptydir | grep -q . \
  || fail "the restored OUT_DIR is missing the empty directory the build script created"
second_value=$("$bsrun/target/debug/app") || fail "the restored registry build-script binary did not run"
[ "$second_value" = "$first_value" ] \
  || fail "the restored build printed '$second_value', expected '$first_value'"
ok "a registry build script is shimmed and its run is restored after a deleted target"

# 13. A modern build script prints the double-colon spellings cargo accepts since 1.77:
#     `cargo::rerun-if-env-changed` and `cargo::rustc-env`, with a trailing space on the name because
#     cargo trims it. Flipping that variable between two builds with no target directory must miss on
#     the recorded value; reading only the single-colon spelling recorded nothing, and an untrimmed
#     name names a variable nothing can set, so either way the shim replayed the first run's stdout
#     and baked the old value in.
probe=$work/probe
probe_home=$probe/cargo-home
probe_dep=$probe_home/registry/src/test-index/probe-dep-1.0.0
probe_log=$work/probe.log
mkdir -p "$probe_dep/src" "$probe/app/src"
: >"$probe_dep/.cargo-ok"
cat >"$probe_dep/Cargo.toml" <<'EOF'
[package]
name = "probe-dep"
version = "1.0.0"
edition = "2021"
EOF
cat >"$probe_dep/build.rs" <<'EOF'
fn main() {
    println!("cargo::rerun-if-env-changed=BLITZ_PROBE ");
    let value = std::env::var("BLITZ_PROBE").unwrap_or_default();
    println!("cargo::rustc-env=BLITZ_PROBE={value}");
}
EOF
cat >"$probe_dep/src/lib.rs" <<'EOF'
pub const PROBE: &str = env!("BLITZ_PROBE");
EOF
cat >"$probe/app/Cargo.toml" <<EOF
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
probe-dep = { path = "$probe_dep" }
EOF
cat >"$probe/app/src/main.rs" <<'EOF'
fn main() {
    println!("{}", probe_dep::PROBE);
}
EOF
cat >"$probe/Cargo.toml" <<'EOF'
[workspace]
members = ["app"]
resolver = "2"
EOF
probe_build() {
  ( cd "$probe" && CARGO_HOME=$probe_home CARGO_INCREMENTAL=0 BLITZ_CACHE_DIR=$work/probe-cache \
    BLITZ_LOG=$probe_log RUSTC_WRAPPER=$blitz BLITZ_PROBE=$1 cargo build --offline ) >/dev/null 2>&1
}
: >"$probe_log"
probe_build a || fail "the first double-colon build-script build failed"
value=$("$probe/target/debug/app") || fail "the first double-colon build-script binary did not run"
[ "$value" = "a" ] || fail "the first double-colon build printed '$value', expected 'a'"
rm -rf "$probe/target"
: >"$probe_log"
probe_build b || fail "the second double-colon build-script build failed"
grep -qE '^bsrun-miss[[:space:]].*env-changed' "$probe_log" \
  || fail "no bsrun-miss env-changed line in BLITZ_LOG: $(tr '\n' ' ' <"$probe_log")"
value=$("$probe/target/debug/app") || fail "the rebuilt double-colon build-script binary did not run"
[ "$value" = "b" ] || fail "the rebuilt double-colon build printed '$value', expected 'b'"
ok "cargo:: rerun-if-env-changed is recorded and flipping the value misses and rebuilds"

echo "all e2e checks passed"
