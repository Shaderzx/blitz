# blitz design

blitz is a cargo front-end. It does not replace rustc, and it cannot make the compiler itself
faster. What it does is make sure the same compile never runs twice: on any machine that shares a
cache directory, a build whose inputs were built before is restored instead of compiled.

## Goal and the honest scope of "144x"

The headline target: **a clean-target build of a real project (flowmaster/engine) on a fresh
checkout, with a warm blitz cache, finishes at least 144 times faster than `cargo build` on the same
checkout.** That is the CI case (every new pull request starts with an empty target directory) and
the branch-switch case.

What blitz does not claim, and the benchmark reports anyway:
- A cold cache (nothing ever built) is no faster than cargo. The miss path must stay within 5% of
  plain cargo.
- Editing one file in the top crate recompiles that crate, like cargo does. The gain there comes from
  dependencies being restored instead of rebuilt on a fresh checkout.

## Two caches, one store

`src/store.rs` (done) holds content-addressed blobs (`cas/`) and JSON action entries (`ac/<ns>/`)
over one writable tier and any number of read-only tiers. Writes are staged in `tmp/` and renamed
into place, so concurrent builds never see partial files. Restores go through
`Store::materialize`, which copies with `std::fs::copy` (a copy-on-write clone on APFS, btrfs and
XFS, so a 50 MB binary restores in about a millisecond), sets the mode and a fresh mtime, and renames
over the destination.

`src/hash.rs` (done): `Digest` (BLAKE3) and `KeyBuilder`, which length-prefixes every labelled part.

`src/toolchain.rs` (done): `rustc_identity(path, cache_root)` returns `rustc -vV` cached by the
binary's path, size, mtime and inode; `resolve_rustc(dir)` finds the real compiler cargo would use
without spawning rustup in the common case.

### Whole-build cache (`src/whole.rs`, namespace `whole`)

`blitz build [cargo build args]`:

1. Fall back to plain `cargo build <args>` (with the per-unit wrapper) when the arguments contain
   something blitz does not model: `--message-format`, `--build-plan`, `--unit-graph`, `-Z`,
   `--manifest-path` pointing outside the current workspace, `--out-dir`/`--artifact-dir`,
   `--timings`, or `BLITZ_DISABLE=1`.
2. Find the workspace root: the nearest ancestor `Cargo.toml` containing `[workspace]`, else the
   nearest `Cargo.toml` (respecting `--manifest-path`).
3. Compute the key (domain `blitz-whole-v1`) from:
   - every input file under the workspace root: walk with the `ignore` crate (respects `.gitignore`,
     includes hidden files such as `.cargo/config.toml`, skips `.git/` and the target directory).
     Each file contributes its path relative to the root and its BLAKE3 digest. A stat cache
     (`<target>/blitz/statcache.json`: relative path -> size, mtime ns, inode, digest) avoids
     re-hashing unchanged files; any metadata change means re-hash.
   - `~/.cargo/config.toml` (or `$CARGO_HOME/config.toml`) contents if present.
   - toolchain: `rustc_identity(resolve_rustc(root))`, plus size and mtime of the `cargo` binary next
     to that rustc.
   - the cargo arguments, minus pure output flags (`-q`, `--quiet`, `-v`, `-vv`, `--verbose`,
     `--color <x>`, `--color=<x>`, `-j N`, `--jobs N`, `--keep-going`).
   - environment: every variable whose name starts with `CARGO_` (except `CARGO_HOME`,
     `CARGO_TARGET_DIR`, `CARGO_MAKEFLAGS`, `CARGO_TERM_*`, `CARGO_BUILD_JOBS`), `RUSTFLAGS`,
     `RUSTDOCFLAGS`, `RUSTC`, `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER`, `RUSTC_BOOTSTRAP`,
     `RUSTUP_TOOLCHAIN`, `CC`, `CXX`, `AR`, `CFLAGS`, `CXXFLAGS`, `LDFLAGS`, `PKG_CONFIG_PATH`,
     `SDKROOT`, `MACOSX_DEPLOYMENT_TARGET`, and any names listed in `BLITZ_KEY_ENV` (comma separated).
   - OS and architecture of blitz itself (`std::env::consts`).
4. Look up `whole/<key>`. The entry records: the artifacts (path relative to the target directory,
   blob digest, executable bit), and the extra inputs found after the miss build (see 6). A hit
   requires every blob present and every extra input unchanged. On a hit, materialize every
   artifact, write the stat cache, print to stderr in cargo's style:
   `    Finished <profile> [restored by blitz] in 4ms` and exit 0 without running cargo.
5. On a miss, run `cargo build <args> --message-format=json-render-diagnostics` with
   `RUSTC_WRAPPER=<blitz>` (unless already set or `BLITZ_NO_WRAPPER=1`). stderr is inherited (the
   user sees cargo's normal progress and diagnostics); stdout is read line by line as JSON. Collect
   `compiler-artifact` messages for packages whose `manifest_path` is inside the workspace root:
   their `filenames` and `executable`. Also forward `"reason":"build-finished"` success.
   If cargo fails, return its exit code and store nothing.
6. After a successful miss build, record extra inputs that the walk did not cover. These are read
   from every dep-info file and build script output in the profile directories, not only the ones
   this build wrote, because cargo can skip a build script whose watched variable still matters.
   Extra inputs are validated on lookup, not part of the key, so a toggling external input costs
   one miss each way. Path dependencies outside the workspace root are walked and keyed like the
   workspace itself (labelled by the manifest's path value, so no absolute path enters the key).
   Any dep-info prerequisite or `rerun-if-changed` path the key does not already hold is an extra
   input, including git-ignored files under the root; those are stored relative to the root so a
   second checkout validates its own copy. A watched directory is recorded as its sorted tree
   (relative path and digest per file). `NotFound` is the only "absent"; any other read error
   means no entry is stored. `Cargo.lock` and every manifest reachable through `members` and
   `path` dependencies are keyed even when git-ignored. File-valued `--config` falls back to plain
   cargo (inline `key=value` is keyed as a string). Entries carry `ENTRY_VERSION` 2, so entries
   from before these rules are never reused.
   On a hit, every artifact path must be relative with only normal components; anything else is
   treated as a miss before a single file is written.
   - files listed in the workspace crates' dep-info (`<target>/<profile>/deps/*.d`; parse the first
     rule's prerequisites, paths canonicalised first) that are outside the workspace root and outside
     `$CARGO_HOME/registry` and `$CARGO_HOME/git` (registry and git sources are pinned by
     `Cargo.lock`), with their digests;
   - for every build script output (`<target>/<profile>/build/*/output` written in this build):
     `cargo:rerun-if-env-changed=NAME` (record NAME and its current value, or absent) and
     `cargo:rerun-if-changed=PATH` outside the workspace (record path and digest).
   Store the artifacts as blobs, then the entry, then write the stat cache.
7. The target directory: `--target-dir` argument, else `CARGO_TARGET_DIR`, else `build.target-dir`
   in `<root>/.cargo/config.toml`, else `<root>/target`. Artifact paths are recorded relative to it,
   so a restore into a different checkout or target directory works.

Never print secrets. Keep the hit path free of process spawns: a hit on flowmaster/engine must take
single-digit milliseconds.

### Per-unit cache (`src/wrapper.rs`, namespace `unit`)

Cargo runs `blitz <rustc> <args>` for every compiler invocation. `wrapper::main` must never fail a
build because of the cache: any internal error runs the real compiler with the original arguments.

1. Pass straight through (no caching) when: no `--crate-name`; any input is `-` (stdin); `-C
   incremental=...` is present; the compiler is `clippy-driver` or anything other than a plain
   rustc; crate types include `bin`, `dylib` or `cdylib` (linking pulls in system state); `--emit`
   lacks both `link` and `metadata`; `BLITZ_DISABLE=1`.
   Cacheable crate types: `lib`, `rlib`, `staticlib`, `proc-macro`.
2. Key (domain `blitz-unit-v1`):
   - `rustc_identity(<rustc>)` (argv[1] is the real toolchain binary; cache by its metadata).
   - The arguments in order, each normalized: replace the out-dir's target root (the directory two
     levels above `--out-dir` when it ends in `<profile>/deps`; the build script dirs live under the
     same root), the current directory, `CARGO_MANIFEST_DIR`, and `$CARGO_HOME` with placeholders
     (`$TARGET`, `$CWD`, `$MANIFEST`, `$CARGO_HOME`). Longest prefix first.
   - For every `--extern name=path`: the digest of the file's contents. Memoize digests in
     `<target root>/blitz/digests/<file name>` (size + mtime ns + digest) so each rlib is hashed once.
   - Environment: every `CARGO_*` variable cargo set for this invocation except `CARGO_MAKEFLAGS`
     and `CARGO_HOME`, normalized the same way; plus `RUSTC_BOOTSTRAP`, `OUT_DIR` (normalized),
     `SDKROOT`, `MACOSX_DEPLOYMENT_TARGET`.
   - Sources:
     - When `CARGO_MANIFEST_DIR` is inside `$CARGO_HOME/registry/src` or `$CARGO_HOME/git/checkouts`,
       the sources are immutable for a given normalized path (crates.io and git revisions do not
       change), so skip the dep-info pre-pass and key on the normalized manifest dir alone.
     - Otherwise run a dep-info pre-pass: the same arguments with `--emit` replaced by
       `--emit=dep-info`, `-o`/`--out-dir` pointed at a temp directory, and diagnostics discarded.
       Parse the `.d` file: hash every prerequisite file (path normalized in the key) and record
       `# env-dep:NAME=VALUE` lines (key on NAME and the current value).
     - When `OUT_DIR` is set (the crate has a build script), hash every file under `OUT_DIR`
       (relative paths). Generated sources and bundled native libraries live there.
3. Hit: entry `unit/<key>` lists output files (file name, digest), the normalized stdout and
   stderr, and the exit code (always 0; failures are not cached). Materialize every output into
   `--out-dir` (mode 0644), de-normalizing `.d` files on the way out (the stored `.d` has
   placeholders), replay stdout and stderr de-normalized, update the digest memo for the restored
   files, record `unit-hit` in stats, exit 0.
4. Miss: run the real compiler with stdout and stderr piped. Forward every stderr line to our stderr
   as it arrives (cargo reads the JSON artifact notifications on stderr for pipelining, so they must
   not be held back), and keep a copy. On exit 0, collect the outputs: files in `--out-dir` whose
   names start with `lib<crate_name><extra_filename>.` or `<crate_name><extra_filename>.` (from
   `-C extra-filename=`) and whose mtime is at or after the invocation start. Store them as blobs (the
   `.d` file normalized first), store the entry, update the digest memo, record `unit-miss`.
5. Log one line per decision to `$BLITZ_LOG` when set: `hit|miss|pass <crate> <reason>`.

## Correctness rules

- A stale hit is the worst possible bug. When in doubt, miss.
- Keys never include absolute paths that differ between checkouts, and always include everything
  that changes output bytes.
- Failed compiles are never cached.
- Read-only tiers are never written (enforced in `Store`).

## Proof

- `tests/e2e.sh`: drives the real `blitz` binary on a small real workspace (a binary crate plus a
  path-dependency library) in a temp dir: miss, clean, hit, edit invalidates, env change
  invalidates, per-unit hits on a release build after `cargo clean`. One script, no unit tests.
- `bench/bench.py <repo path>`: fresh clones, measured scenarios, a markdown table. See
  `docs/benchmarks.md` for the recorded numbers.
