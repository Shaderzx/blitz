# Decision trail

Why blitz looks the way it does, in the order the decisions were made.

| # | Decision | Why |
|---|---|---|
| 1 | Speed comes from never compiling the same thing twice, not from a faster compiler. | rustc does the work; cargo's own overhead is a fraction of a second. A faster linker or backend gives 2-5x at best. Only reuse of earlier work gives two orders of magnitude, which is how Bazel, Buck and sccache get their wins. |
| 2 | Headline benchmark: clean target, fresh checkout, warm cache, against `cargo build` on the same checkout. Cold-cache and edit-one-file numbers are reported next to it. | That is the case every new CI pull request and every branch switch hits. Reporting the cases blitz does not speed up keeps the number honest. |
| 3 | Two caches over one content-addressed store: whole-build (skip cargo entirely) and per-unit (blitz as `RUSTC_WRAPPER`). | The whole-build hit is what makes a build take milliseconds. The per-unit cache makes the miss path cheap after any change, because dependencies are restored instead of rebuilt. |
| 4 | Restore by `std::fs::copy` into a temp file and rename. | On APFS, btrfs and XFS the copy is a copy-on-write clone, so it costs about the same as a hard link without sharing mutable bytes with the cache. |
| 5 | Per-unit cache skips incremental, `bin`, `dylib` and `cdylib` units. | Incremental units are already cached by rustc itself. Linked outputs pull in system libraries and linkers that blitz cannot hash. sccache draws the same line. |
| 6 | Registry and git dependencies are keyed by their normalized source path, without a dep-info pre-pass. | Their sources cannot change for a given path (crates.io versions and git revisions are immutable, and `Cargo.lock` pins both), so the pre-pass would only cost time. |
| 7 | Read-only cache tiers (`BLITZ_CACHE_RO`). | CI pull requests read main's cache but write only to their own, so a pull request cannot plant artifacts in the cache main's builds use. |
| 8 | The target directory is not part of the whole-build key. | Artifacts are stored relative to it, so a restore into another checkout or `--target-dir` works. Proven by restoring into a second target dir and running the binary. |
| 9 | Proofing found three stale-hit holes, each fixed before merge: relative `rerun-if-changed` paths resolved against the wrong root, `..` paths compared textually, and path dependencies outside the workspace not hashed at all. | A stale hit is the worst bug blitz can have; each hole was reproduced with a purpose-built workspace, fixed, and re-run. |
| 10 | Symlinked or unwalkable directories make `blitz build` fall back to plain cargo. | Guessing which side of a symlink is an input risks a stale hit; plain cargo is always correct. |
| 11 | Lines are 120 columns (`rustfmt.toml`). | The modules were written at that width; one formatter setting keeps `cargo fmt --check` in CI meaningful. |
