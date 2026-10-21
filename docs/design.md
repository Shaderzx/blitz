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
     includes hidden files such as `.cargo/config.toml`, skips `.git/`, the target directory and
     every cache store root, which may sit inside the workspace).
     Each file contributes its path relative to the root and its BLAKE3 digest. A stat cache
     (`<target>/blitz/statcache.json`: relative path -> size, mtime ns, inode, digest) avoids
     re-hashing unchanged files; any metadata change means re-hash, and a file whose mtime is within
     2 s of now is re-hashed anyway (the racy-clean rule: a rewrite inside one clock tick moves no
     stat field).
   - a second walk with the ignore rules switched off adds every `Cargo.toml` and `Cargo.lock`
     under the root, so a member an ignore rule hides (for example `members = ["vendor/*"]` with
     `vendor/` in `.gitignore`) is still keyed. It skips `.git`, the target directory and the cache
     store roots, collects only those two file names, and skips a manifest it cannot read rather
     than failing the key.
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
   second checkout validates its own copy. A watched directory is recorded as its sorted tree, one
   digest per file and a digest-free entry per directory, so adding, removing or renaming an empty
   directory changes the value. The target directory and every cache store root are left out of the
   tree, and a watch that resolves exactly at one of them stores no entry at all: its tree changes
   with every build. `NotFound` is the only "absent"; any other read error
   means no entry is stored. Every `Cargo.toml` and `Cargo.lock` under the root is keyed even when
   git-ignored. File-valued `--config` falls back to plain
   cargo (inline `key=value` is keyed as a string). Entries carry `ENTRY_VERSION` 3, so entries
   from before these rules are never reused. A build that starts without a `Cargo.lock` stores no
   entry: cargo writes the lock during that build, so the recorded key could never be looked up.
   On a hit, every artifact path must be relative with only normal components; anything else is
   treated as a miss before a single file is written.
   The sources of extra inputs:
   - every prerequisite in the workspace crates' dep-info (`<target>/<profile>/deps/*.d`, paths
     canonicalised first) that the key does not already hold, except files under
     `$CARGO_HOME/registry` and `$CARGO_HOME/git` (pinned by `Cargo.lock`);
   - every build script output (`<target>/<profile>/build/*/output`): `cargo:rerun-if-env-changed=NAME`
     (NAME and its current value, or absent) and `cargo:rerun-if-changed=PATH` (file digest, or the
     directory's tree) when the key does not already hold it.
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
   incremental=...` is present (those invocations are seeded instead, see the next section); the
   real compiler is not a plain `rustc` (`clippy-driver` reaches it as a chained wrapper, and the
   units behind it are cached like any other); crate types include `bin`,
   `dylib` or `cdylib` (linking pulls in system state); `--emit` lacks both `link` and `metadata`;
   `BLITZ_DISABLE=1`.
   Cacheable crate types: `lib`, `rlib`, `proc-macro`, and `bin` for a build script
   (`build_script_*`). A proc-macro and a build script are linked artifacts, so their keys also
   cover the linker binary and the SDK variables (decision 13 in `docs/decision-trail.md`, and
   decision 26 for build scripts).
2. Key (domain `blitz-unit-v2`):
   - `rustc_identity(<rustc>)` (argv[1] is the real toolchain binary; cache by its metadata).
   - The arguments in order, each normalized: replace the out-dir's target root (the directory two
     levels above `--out-dir` when it ends in `<profile>/deps`, and three levels above a build
     script's `<target>/<profile>/build/<pkg>-<hash>` so the `-L dependency=<target>/<profile>/deps`
     path on its compile line normalizes too), the current directory, `CARGO_MANIFEST_DIR`, and
     `$CARGO_HOME` with placeholders (`$TARGET`, `$CWD`, `$MANIFEST`, `$CARGO_HOME`). Longest prefix
     first.
   - For every `--extern name=path`: the digest of the file's contents. Memoize digests in
     `<target root>/blitz/digests/<file name>` (size + mtime ns + digest) so each rlib is hashed once.
   - Environment: every `CARGO_*` variable cargo set for this invocation except `CARGO_MAKEFLAGS`
     and `CARGO_HOME`, normalized the same way; plus `RUSTC_BOOTSTRAP`, `OUT_DIR` (normalized),
     `SDKROOT`, `DEVELOPER_DIR`, `MACOSX_DEPLOYMENT_TARGET`, and every `DEP_*` variable cargo set for
     the unit (the `links` metadata of its dependencies).
   - Sources:
     - When `CARGO_MANIFEST_DIR` is an unpacked registry crate (under `$CARGO_HOME/registry/src`
       with a `.cargo-ok` file next to its `Cargo.toml`), the key stops at the normalized manifest
       path: the extraction cannot change and `Cargo.lock` pins the version. No dep-info pre-pass
       runs, so a cold registry unit costs one compiler process instead of two. The `# env-dep:`
       lines the real compile writes into its `.d` are stored in the entry as name plus value (or
       unset) and compared on every lookup; any difference is a miss, and the recompile overwrites
       the entry with the values it saw.
     - A git checkout is not treated that way. It, and every other unit, runs the dep-info
       pre-pass, which hashes every prerequisite file (path normalized in the key) and keys the
       `# env-dep:NAME=VALUE` lines by name and current value, so a changed source or variable
       changes the key before anything can be restored.
     - When `OUT_DIR` is set (the crate has a build script), hash every file under `OUT_DIR`
       (relative paths). Generated sources and bundled native libraries live there.
3. Hit: entry `unit/<key>` lists output files (file name, digest, mode), the normalized stdout and
   stderr, and the environment reads recorded for a registry unit. Materialize every output into
   `--out-dir` with the recorded mode (0755 for a build script's executable, 0644 otherwise),
   de-normalizing `.d` files on the way out (the stored `.d` has placeholders), replay stdout and
   stderr de-normalized, update the digest memo for the restored files (for a `.d`, the digest of
   the de-normalized bytes written), record `unit-hit` in stats, exit 0. A recorded environment
   value that no longer matches the current environment makes the entry a miss.
4. Miss: run the real compiler with stdout and stderr piped. Forward every stderr line to our stderr
   as it arrives (cargo reads the JSON artifact notifications on stderr for pipelining, so they must
   not be held back), and keep a copy. On exit 0, collect the outputs: files in `--out-dir` whose
   names start with `lib<crate_name><extra_filename>.` or `<crate_name><extra_filename>.` (from
   `-C extra-filename=`) and whose mtime is at or after the invocation start. Store them as blobs (the
   `.d` file normalized first), store the entry, update the digest memo, record `unit-miss`. For a
   registry unit, read the `# env-dep:` lines out of that `.d` and store them in the entry; if the
   compile wrote no `.d`, do not store the entry, because a hit would then have nothing to compare
   the environment against.
5. Log one line per decision to `$BLITZ_LOG` when set: `hit|miss|pass <crate> <reason>`; the
   incremental path adds `seed <crate> <files>`, `noseed <crate> <reason>` and
   `snapshot <crate> files=<n> bytes=<n> ms=<n>`.

### Incremental seeding (`src/incremental.rs`, namespace `incr`)

Cargo compiles workspace members in the dev profile with `-C incremental=<dir>`. v0.1 passed those
invocations straight through, so on a fresh checkout the crate a user just edited had no state to
resume from and compiled from scratch: `blitz warm + edit` on flowmaster/engine was 1.41x. rustc's
incremental compilation is built to reuse its query cache and object files from an earlier compile
of the same crate, and it checks every reused result itself (fingerprints; a session from another
command line or compiler is discarded). blitz only has to put the state where rustc looks.

Every compiler invocation that carries `-C incremental=<dir>` and `--crate-name <name>`, direct or
behind a workspace wrapper such as `clippy-driver`, takes this path:

1. **Lineage key** (domain `blitz-incr-v1`): the real compiler's `rustc -vV` text
   (`toolchain::rustc_identity`), the chained driver's path, size and mtime when there is one, the
   crate name, crate types, whether `--test` is present, `--target`, the profile directory name
   (`debug`, `release`), and the normalized `CARGO_MANIFEST_DIR`, the package name and version, and
   the manifest's path relative to the workspace root, so two packages with one crate name (or one
   package's library and its bench) never share a lineage. The sources, the `--extern` digests and
   `-C metadata` are deliberately left out: they change between commits, and reusing
   older state across them is the point.
2. **Seed** before the compiler runs: look up `incr/<lineage>`. The entry records the crate's
   incremental subdirectory (`<crate_name>-<id>`, one plain path component, rejected otherwise)
   and its files (relative path with normal components only, digest, mode). If
   `<dir>/<subdir>` does not exist, materialize the files into it through a staged directory and a
   rename. If it exists, do nothing: local state is newer than any snapshot. The files include the
   session's lock, whose name is the session name without its trailing hash component. rustc does
   not load a session whose lock is missing, so an entry without one is not used. Log `seed` or
   `noseed`; any error skips seeding and never fails the build.
3. **Snapshot** after the compiler exits 0: every `<crate_name>-*` directory and its newest finalized
   `s-*` session (not `-working`) were recorded before the compile, so the one directory whose
   session changed during it is the crate directory this invocation wrote; that newest session's
   files plus its lock are stored. Two directories changing at once cannot be attributed, so the
   snapshot is skipped (`nosnap ambiguous`). The blobs are written from the digest
   memo, so unchanged object files are not re-hashed. A lineage snapshotted less than five minutes
   ago is left alone unless `BLITZ_INCREMENTAL_SNAPSHOT=always`, which adopting CI sets (the
   FlowMaster repos do), so every run leaves the newest state for the next pull request. Log the file
   count, byte count and duration, and log `nosnap <reason>` when a snapshot is skipped. Staging
   directories a killed build left behind (`.blitz-seed-*` older than an hour) are deleted when the
   next seed runs.
4. `BLITZ_INCREMENTAL=off` disables seeding and snapshots.

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
