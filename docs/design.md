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
   cargo (inline `key=value` is keyed as a string). Entries carry `ENTRY_VERSION` 4, so entries
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
   real compiler is not a plain `rustc` (incremental units behind a chained wrapper such as
   `clippy-driver` use incremental seeding; non-incremental chained units pass through uncached);
   crate types include `bin`,
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

### Cached build-script runs (`src/buildscript.rs`, namespace `bsrun`)

Cargo runs a build script itself: the compile goes through `RUSTC_WRAPPER`, the run does not. v0.2
cached the compile, so a fresh target restored the executable and still paid for every run, and for a
crate that builds C in its build script that run dominates: on engine's CI build `aws-lc-sys` took
11.9 s of a 24.5 s edit build and nothing of engine started before 13.1 s. v0.3 caches the run too.

1. **Shim.** When the wrapper compiles the build script of an unpacked registry crate
   (`build_script_*`, crate type `bin`, manifest under `$CARGO_HOME/registry/src` with `.cargo-ok`), it
   renames the executable to `<file>.blitz-real` and writes a `/bin/sh` shim in its place:

   ```sh
   #!/bin/sh
   if [ -x '<blitz>' ] && [ "${BLITZ_BUILD_SCRIPT_RUN:-on}" != off ]; then
       BLITZ_BSRUN_MARK='<file>.blitz-real'.blitz-mark.$$
       export BLITZ_BSRUN_MARK
       rm -f "$BLITZ_BSRUN_MARK"
       '<blitz>' build-script-run '<file>.blitz-real' "$@"
       code=$?
       if [ -e "$BLITZ_BSRUN_MARK" ]; then rm -f "$BLITZ_BSRUN_MARK"; exit "$code"; fi
       if [ "$code" -ne 0 ]; then exec '<file>.blitz-real' "$@"; fi
       exit 0
   fi
   exec '<file>.blitz-real' "$@"
   ```

   The paths are absolute and single-quoted. Cargo hard-links the executable to `build-script-build`
   after the compile returns, so whichever name it runs is the shim. The shim still works when blitz
   is gone (a CI run whose blitz download failed reuses the same target volume), and it falls back to
   the real script when blitz exits non-zero without a marker. A non-zero exit alone also describes
   a script that failed, or blitz killed by the kernel. Running a script that already ran a
   second time repeats its side effects and its output. blitz therefore touches
   `$BLITZ_BSRUN_MARK`, `<real>.blitz-mark.$$` beside the real binary, after successfully spawning
   the real script, and never removes it: the shim does. The marker
   lives in cargo's own writable build directory, so a `$TMPDIR` that cannot be written cannot cost
   the fallback its precision. A marker means the script is what produced the code, or is producing
   it. Without a marker after a non-zero exit, the shim assumes nothing ran and execs the script.
   An exit of zero removes the marker and exits 0. If the shim itself is killed, its marker can remain under
   target until cargo cleans it.
   The marker prevents duplicate runs after spawn; it cannot make marking and spawning atomic. If
   writing the marker fails, a non-zero exit can still cause a duplicate run. Path and workspace crates
   keep the plain executable, because cargo recompiles their build script whenever their sources change
   and their runs are not the expensive ones. The per-unit entry stores the real binary (mode 0755)
   under the hashed
   executable name cargo gave the script (`build_script_build-<meta>`), the name it had before the
   shim replaced it and the name v0.2 stored, and never the shim; a restore writes the shim fresh, so
   it names the blitz doing the restoring, and a v0.2 binary can still read a v0.3 entry. Only that
   executable uses the real path and mode 0755. Extra outputs, including split-debuginfo object files,
   keep their own names and mode 0644.

2. **Key** (domain `blitz-bsrun-v3`, so entries from before tool, environment and replay fixes are not reused):
   - the BLAKE3 digest of the real executable (a recompile, a new compiler or a new linker changes it);
   - `OUT_DIR` as a literal path (CI spells it identically on every run; another path is another key);
   - every environment variable cargo sets for a build script (`CARGO_*`, plus `TARGET`, `HOST`,
     `PROFILE`, `OPT_LEVEL`, `DEBUG`, `RUSTC`, `RUSTC_LINKER`), except `CARGO_MAKEFLAGS` (a jobserver
     token that changes on every build) and `NUM_JOBS` (parallelism, not output);
   - every `DEP_*` name and value Cargo supplies from dependencies with `links`, even without an
     explicit `rerun-if-env-changed` directive;
   - the C tooling variables a build script reads: `CC`, `CXX`, `AR`, `CFLAGS`, `CXXFLAGS`,
     `LDFLAGS`, `CPPFLAGS` in their plain, `<name>_<target>`, `TARGET_<name>` and `HOST_<name>`
     forms, plus anything starting with `PKG_CONFIG` or `CMAKE`;
   - every variable named after the crate (`AWS_LC_SYS_*`, `RING_*`): `CARGO_PKG_NAME` upper-cased
     with `-` as `_`, followed by `_`;
   - the identity (real path, size, mtime) of `cc`, `c++`, `cmake`, `ar`, `ld`, `perl`, `pkg-config`,
     `make`, `python3`, `protoc`, `nasm`, `clang` and `git` on `PATH`, plus the effective
     target-prefixed and host-prefixed names selected from `TARGET` and `HOST`, or the fact that one
     is absent. The raw `PATH` also enters the key. Each path is canonicalised before it is recorded,
     and the resolver requires execute permission, so a symlink or replacement tool changes the key.
     Explicit executable overrides, including `CC`, `CXX`, `AR`, `PKG_CONFIG`, `RUSTC` and
     `RUSTC_LINKER` forms, contribute their resolved path and content digest. An unresolved selection,
     command string, or non-UTF-8 selection bypasses the run cache.

3. **Direct mode.** The stdout of a stored run is parsed for `cargo:rerun-if-env-changed=NAME` and
   `cargo:rerun-if-changed=PATH`. Each name is stored with the value the run saw, and each path
   outside `CARGO_MANIFEST_DIR` and outside `OUT_DIR` with the digest of the file it held, or with
   "absent". A lookup compares every one and misses on any difference, so a table-driven build script
   that switches on `AWS_LC_SYS_*` cannot answer with the wrong table. A `rerun-if-changed` path that
   resolves to a directory or a special file makes the run uncacheable rather than stale. Paths inside
   the crate are left out because the shim is only installed for an unpacked registry crate: its
   extraction is immutable and checksum-verified, so nothing inside it can change while the key stays
   the same. A watch is omitted only when both its original spelling and resolved target are inside
   the crate or `OUT_DIR`, with no `..` in its spelling. External watches retain their original path
   even when their current target is internal. Retargeting an external link therefore rechecks its
   current bytes. An unresolved path is watched, not ignored. Only `NotFound` records or matches
   absence. Directories, special files and other metadata or read errors reject caching. Instruction
   parsing trims the complete line and its value, matching Cargo's whitespace handling.

4. **Hit.** Only into an empty or missing `OUT_DIR`; anything already there belongs to this run, so the
   real script runs instead. Cargo directives replay before output restoration. If directive replay
   fails after stdout starts, blitz returns an error and never runs the real script a second time.
   Files are materialized with their recorded modes, directories are recreated from entries of their
   own (an empty one has no file to carry it), and symlinks are recreated with their targets: a link
   that resolves to a file is stored with the digest of that target and verified on every lookup, and
   a link to a directory or a special file makes the run uncacheable. Regular files and directories
   are restored before links. All links are created before their target digests are checked, so forward
   references and link chains work. An explicitly recorded dangling link must still return `NotFound`;
   a target appearing later rejects the hit. Before any writes, validation rejects non-normal names,
   normalized duplicate names, conflicting types and any non-directory ancestor of another entry.
   Leaf symlinks may target external files, but restoration never writes through them. Directory modes
   are applied deepest-first after the tree is populated and links are checked. A restore that fails
   removes and recreates `OUT_DIR` before the caller can run the real script.

5. **Miss.** The real binary runs with the same arguments, environment, working directory and stdin;
   its stdout and stderr are forwarded as they arrive and recorded; its exit code is returned
   unchanged. The run is stored only when it exited 0, `OUT_DIR` was empty or missing before it
   started, both streams were read and forwarded completely, and all output names are UTF-8.
   Interrupted reads retry; other read or forwarding errors prevent storing the run. A forwarding
   error does not stop draining the other output or change the script's exit code.
   Every internal error (no key, unreadable entry,
   missing blob, disk full, a name that cannot go into the shim) falls back to running the real
   binary, so a broken cache can cost a run and never a build.

6. Log `bsrun-hit`, `bsrun-miss` and `bsrun-store` to `$BLITZ_LOG` in the usual format and count them
   in `blitz stats`. Each real script run counts one `bsrun-miss`, even when storing fails.
   `BLITZ_BUILD_SCRIPT_RUN=off` and `BLITZ_DISABLE=1` take the real binary even when blitz is there.
   Runs live in `ac/bsrun/` with their blobs in `cas/`, so `blitz gc` trims them with everything else.

What still misses: a registry crate whose build script reads something outside the key (the machine's
`/usr/include`, the time of day, a network response) produces the same key with different bytes;
`rerun-if-changed` is the only way such a script can tell blitz, and cargo has the same blindness. A run
that writes outside `OUT_DIR`, into its own registry extraction or into `/tmp`, is not restored. Git
checkouts and path crates are not shimmed at all.

What the tool identities do not cover: system libraries and headers a script probes, through
`pkg-config` or by scanning `/usr/include`, are not in the key. A changed system library under an
unchanged toolchain can therefore hit. The CI images pin their toolchain, so this is a limit of a
hand-managed machine, not of CI; `BLITZ_BUILD_SCRIPT_RUN=off` disables the run cache entirely.

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
   files plus its lock are stored. Under concurrent lib and test compiles the newest changed session
   can belong to the sibling instead: that only costs a seed, because rustc discards a session whose
   command line differs, and it never produces wrong output. Two sessions with one timestamp cannot
   be attributed at all, so the snapshot is skipped (`nosnap ambiguous`). The blobs are written
   from the digest memo, so unchanged object files are not re-hashed. A lineage snapshotted less
   than five minutes ago is left alone unless `BLITZ_INCREMENTAL_SNAPSHOT=always`, which adopting CI
   sets (the FlowMaster repos do), so every run leaves the newest state for the next pull request.
   Log the file count, byte count and duration, and log `nosnap <reason>` when a snapshot is
   skipped. Staging directories a killed build left behind (`.blitz-seed-*` older than an hour) are
   deleted when the next seed runs.
4. `BLITZ_INCREMENTAL=off` disables seeding and snapshots.

## Correctness rules

- A stale hit is the worst possible bug. When in doubt, miss.
- Keys never include absolute paths that differ between checkouts, and always include everything
  that changes output bytes.
- Failed compiles and failed build-script runs are never cached.
- A cached build-script run is restored only into an empty `OUT_DIR`, and every `rerun-if-*` request
  the script printed is compared on the way in.
- Read-only tiers are never written (enforced in `Store`).

## Proof

- `tests/e2e.sh`: drives the real `blitz` binary on a small real workspace (a binary crate plus a
  path-dependency library) in a temp dir: miss, clean, hit, edit invalidates, env change
  invalidates, per-unit hits on a release build after `cargo clean`, a registry crate's build script
  shimmed and its run restored into a fresh target. One script, no unit tests.
- `bench/bench.py <repo path>`: fresh clones, measured scenarios, a markdown table. See
  `docs/benchmarks.md` for the recorded numbers.
