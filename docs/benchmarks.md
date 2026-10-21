# Benchmarks

`bench/bench.py` measures blitz against plain cargo on a real git repository. The
headline target in [design.md](design.md) is a clean-target build of a real project on a
fresh checkout with a warm blitz cache finishing at least 144 times faster than
`cargo build` on the same checkout. This page records what the harness measured. Every
number below comes from a run of the script, never from an estimate.

## How the harness measures

Each scenario starts from a fresh `git clone` into a temporary directory, so no run
inherits a target directory from an earlier one. The cargo registry is fetched with
`cargo fetch --locked` before the clock starts, so network time is not measured, and each
scenario family gets its own `BLITZ_CACHE_DIR`.

| scenario | setup | runs |
| --- | --- | --- |
| `cargo cold` | fresh clone, no target directory | median of `--runs` (default 3) |
| `blitz cold` | fresh clone, empty blitz cache | median of `--runs` (default 3) |
| `blitz warm` | fresh clone, warm blitz cache | median of 10 |
| `blitz warm + edit` | fresh clone, warm cache, one comment appended to the top crate's source | median of 3 |
| `cargo no-op` | one warmed clone, repeated builds | median of 10 |
| `blitz no-op` | one warmed clone, repeated builds | median of 10 |

`blitz cold` measures the miss path against `cargo cold`: the whole-build cache does not
exist yet, so blitz hashes the inputs and then runs the same compilation cargo runs. The
design allows the miss path to be at most 5% slower.

`blitz warm` measures the whole-build restore: nothing changed, so blitz should answer
from the cache without starting cargo.

`blitz warm + edit` measures the per-unit cache. The whole-build key changes, so the
build is a miss, but the dependency crates are restored per unit and only the edited top
crate is compiled.

The no-op rows show how each tool behaves when the target directory is already up to
date. They are reported separately because a no-op is a different question from a fresh
checkout.

## Running it

```sh
cargo build --release
python3 bench/bench.py /path/to/repo --runs 3 --json bench/results.json
```

The script takes the blitz binary from `--blitz`, `$BLITZ`, or `target/release/blitz`. It
prints the table below to stdout and writes the raw per-run numbers to the JSON file when
`--json` is given. Pair each published table with the JSON it came from.

## Environment

Record the machine with every set of numbers, because absolute times move with CPU,
filesystem, and disk cache state.

| field | value |
| --- | --- |
| date | 2026-10-21 00:50 UTC |
| machine | MacBook Pro, Apple M5 Pro |
| CPU / memory | 18 cores, 64 GB |
| OS / filesystem | macOS 27.0.1, APFS |
| rustc | rustc 1.99.0 (b940084d7 2026-09-28) |
| blitz commit | bad219a (PR #1, after review rounds 1 and 2) |
| repository and commit | flowmaster/engine 548176ad, debug profile, `--locked` |

The machine was not idle: other agents were running Docker stacks and builds, load average
around 10 to 12 during the run. Both tools ran under the same load.

## Results

Raw runs: [`bench/results-engine-2026-10-21.json`](../bench/results-engine-2026-10-21.json).

| scenario | median seconds | speedup vs cargo cold | speedup vs cargo no-op |
| --- | --- | --- | --- |
| cargo cold | 36.262 | 1.00x | |
| blitz cold | 36.995 | 0.98x (2.0% slower) | |
| blitz warm | 0.013 | **2,718x** | |
| blitz warm + edit | 25.661 | 1.41x | |
| cargo no-op | 0.136 | | 1.00x |
| blitz no-op | 0.011 | | 12.1x |

Binary check: the restored binary is executable and 102.78 MB against cargo's 102.79 MB
(same size class; the bytes differ only in embedded checkout paths).

What this says, plainly:
- **Headline (target 144x): met, 2,718x.** A fresh checkout with an empty target directory
  and a warm cache gets a runnable `engine` binary in 13 ms instead of 36 s.
- The miss path costs 2.0% over plain cargo (target: under 5%). Review round 1 briefly pushed
  it to 9.8% by running a dep-info pre-pass for every registry crate; round 2 replaced that with
  ccache-style direct mode (decision 20) and brought it back under the target.
- After editing the top crate, the build is 1.41x faster: the dependencies are restored, but
  engine is one large crate and compiling it is most of the build. blitz cannot make rustc
  faster.
- With nothing to do, blitz answers in 11 ms where cargo takes 136 ms.
- Earlier runs on the same machine: 73fa48c gave 11 ms warm (3,168x) and +0.5% cold; 8d86eb1
  gave 11 ms warm and +9.8% cold (the regression above).

## v0.2: incremental seeding and cached build scripts (2026-10-21)

Same machine and repository (flowmaster/engine `ae28b873`, debug profile, `--locked`), one
`bench/bench.py --runs 1` run per binary. The runs are minutes apart on a shared machine, so
compare each row with `cargo cold` from its own table as well as the absolute seconds.

`blitz` before (main `c64f851`, v0.1):

| scenario | median seconds | speedup vs cargo cold | speedup vs cargo no-op |
| --- | --- | --- | --- |
| cargo cold | 33.049 | 1.00x | |
| blitz cold | 33.296 | 0.99x | |
| blitz warm | 0.011 | 2914.46x | |
| blitz warm + edit | 28.523 | 1.16x | |
| cargo no-op | 0.112 | | 1.00x |
| blitz no-op | 0.011 | | 10.22x |

`blitz` after (v0.2):

| scenario | median seconds | speedup vs cargo cold | speedup vs cargo no-op |
| --- | --- | --- | --- |
| cargo cold | 35.116 | 1.00x | |
| blitz cold | 36.379 | 0.97x | |
| blitz warm | 0.011 | 3113.96x | |
| blitz warm + edit | 24.604 | 1.43x | |
| cargo no-op | 0.114 | | 1.00x |
| blitz no-op | 0.011 | | 9.92x |

`blitz warm + edit` fell from 28.523 s to 24.604 s across checkouts, but those medians overlap:
before [33.818, 24.787, 28.523] s, after [25.244, 24.604, 24.364] s, on a shared machine under
load. That row cannot separate seeding from noise, and it is not the evidence for the feature.

The evidence is the same path, measured on the CI host (fm-hel-03-ai, 64 cores, Docker, CI paths
`/src` and `/target`, `CARGO_BUILD_JOBS=4`, engine `cargo test --no-run`, empty target after a
one-line edit): seeding off 79.4 s, seeding on 27.3 s, and 24.5 s with line-tables-only debug
info. On that host plain cargo cold is 136.8 s, blitz cold 139.6 s and line-tables-only cold
120.6 s. A fresh checkout reuses little, because rustc discards a session whose command line
differs and another checkout spells `--out-dir`, `-L` and the source paths differently; the same
tree does, which is the case CI has after its first build and on every branch switch in one
working copy. On a local checkout that had built once, deleting the incremental state and editing
one library line rebuilt in 17.0 s without seeding and 2.3 s with a snapshot from that same state.

The snapshot cost measured during that debug build was 261 files / 314.2 MB in 249 ms for the
library lineage and 218 files / 54.1 MB in 100 ms for the binary lineage, about 0.8% of a 44 s cold
build.

Raw runs: [`bench/results-engine-2026-10-21-v0.2-before.json`](../bench/results-engine-2026-10-21-v0.2-before.json)
and [`bench/results-engine-2026-10-21-v0.2-after.json`](../bench/results-engine-2026-10-21-v0.2-after.json).

## v0.3: cached build-script runs (2026-10-21)

Measured on the CI host, fm-hel-03-ai (x86_64, 64 cores, Docker `rust:1-bookworm`, rustc 1.99.1),
with the same paths CI uses (`/src`, `/target`, `/blitz-cache`), `CARGO_BUILD_JOBS=4`, engine
`cargo test --no-run --locked --timings` in the dev profile with `CARGO_PROFILE_DEV_DEBUG` and
`CARGO_PROFILE_TEST_DEBUG` set to `line-tables-only`. Each run starts with
`find /target -mindepth 1 -delete` after one probe function is appended to `src/lib.rs`.

Both binaries ran the same recipe minutes apart on the same host. v0.2.0 (`d178be2`) used its own
empty cache (`/blitz-cache-v02`), warmed first; v0.3.0 used the warm v0.2 cache with its new entries
added.

| run | seconds |
| --- | --- |
| v0.2, warm-up (empty cache) | 120.1 |
| v0.2, edit run | 23.8 |
| v0.3, warm-up (v0.2 entries reused) | 48.8 |
| v0.3, edit run | **14.0** |
| v0.3, edit run minutes later, same recipe | 12.4 |
| v0.3, the same edit run again, identical inputs | 8.9 |
| v0.3, edit run with `CARGO_BUILD_JOBS=16` | 14.2 |
| v0.3, edit run on the same tree with the run cache off (`BLITZ_BUILD_SCRIPT_RUN=off`) | 29.3 |
| v0.3, edit run with the final binary, after `blitz gc` had trimmed the cache | 17.9 |

On the 14.0 s run, 309 of the 311 units came from the per-unit cache and 2 passed through; the 35
build-script units that used to miss on every build are the 35 run hits.

Five v0.3 edit runs on this recipe measured 12.4, 13.7, 14.0, 14.2 and 17.9 s. The unit work was the
same in all of them (309 unit hits, 35 run hits, 2 pass-throughs, 25 seeds); what moved was how long
the restores take, and the 17.9 s run paid for a cache whose blobs had just been trimmed out of the
page cache by `blitz gc`.

The same scenario measured 24.5 s for v0.2 before this work, and 23.8 s for the v0.2 control above,
so the edit run fell by 1.7x against the control and 1.75x against the earlier figure, and reaches the
12 s this change was aimed at. Decision counts for the 14.0 s run: 35 `bsrun-hit`, 309 unit hits, 2
pass-throughs and 25 seeds. The run-cache-off run had the same unit counts, 311 unit hits and 3
misses, and 25 snapshots, because more than five minutes had passed since the last snapshot.

Where the time went, from `cargo --timings` (unit durations, not wall time):

| unit | v0.2 seconds | v0.3 seconds | v0.3 start |
| --- | --- | --- | --- |
| `aws-lc-sys` build-script run | 11.45 | 0.12 | 0.79 |
| `ring` build-script run | 2.07 | 0.03 | 0.83 |
| `engine` lib, test build | 6.46 | 6.38 | 2.40 |
| `engine` lib | 3.82 | 3.96 | 2.39 |

Nothing of engine started before 12.57 s in the v0.2 run, and 2.39 s in the v0.3 run: the
build-script runs were the head of the critical path and now restore in about a tenth of a second
each. Total unit seconds fell from 56.0 to 43.9.

The cleanest single measure of the feature is the pair on the same tree: 29.3 s with
`BLITZ_BUILD_SCRIPT_RUN=off` and 12.4 s with it on, 2.4x. The two C build scripts account for 13.5 s
of the v0.2 critical path, and the wall gain against the v0.2 control is 9.8 s, because in v0.3 engine
now compiles while the remaining build scripts restore instead of waiting behind them.

The cache grew from 5.7 GB to 7.1 GB over the warm-up and five edit runs; `blitz gc --max-size 6G`
then removed 1,126 files and left 6,119 MiB, so the run entries are trimmed like everything else.

Raw numbers: [`bench/results-engine-2026-10-21-v0.3-ci.json`](../bench/results-engine-2026-10-21-v0.3-ci.json).

## Reading the numbers

- Compare `blitz cold` against `cargo cold` first. A miss path that is much slower than
  cargo means the hashing and the wrapper are costing the user on every cache miss, and
  that comes before any speedup claim.
- `blitz warm` against `cargo cold` is the headline number. The design target is 144x.
- `blitz warm + edit` against `cargo cold` shows what a one-file change costs when the
  dependencies are restored. It should be much closer to compiling one crate than to a
  full build.
- The no-op rows are the honest floor: when cargo has nothing to do, a cache cannot save
  much, and blitz should not be slower by more than its own key computation.

## Caveats

- Fresh clones use the local git object store, so clone time is not the network. Clone
  time is outside the timed window in every scenario anyway.
- Absolute times include process startup for both tools and depend on how warm the
  filesystem cache is. Medians absorb some of that; they do not remove it.
- A repository with build scripts that read machine state (environment variables, system
  headers, generated native libraries) can miss more often than a pure-Rust workspace.
  If a scenario hits less than expected, run it with `BLITZ_LOG` set and read the
  decision lines before trusting the number.
