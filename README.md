# blitz

A cargo front-end that never compiles the same thing twice.

```sh
blitz build            # like cargo build; restores the whole result from cache when nothing changed
blitz test             # any cargo subcommand runs with the per-crate cache
blitz stats            # cache hits and misses
```

- **Whole-build cache.** `blitz build` hashes every input of the build up front. When that exact
  build ran before (here, on CI, or in another checkout sharing the cache), it restores the finished
  artifacts without starting cargo.
- **Per-crate cache.** Otherwise cargo runs with blitz as `RUSTC_WRAPPER`, and every dependency that
  was compiled before is restored instead of recompiled.
- **Incremental seeding.** A fresh checkout starts its edited crate from the incremental state an
  earlier compile stored, instead of compiling it from scratch.
- **Cached build scripts.** Build-script binaries are restored like proc-macros, so a clean build
  does not recompile them on every run.

How it works and what it does not do: [docs/design.md](docs/design.md). Measured numbers:
[docs/benchmarks.md](docs/benchmarks.md).
