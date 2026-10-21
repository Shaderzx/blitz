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

`blitz warm + edit` fell from 28.523 s to 24.604 s, and from 86% of that run's cold build to
70%. The gain is bounded by rustc: it discards an incremental session whose command line
differs, and a fresh checkout spells `--out-dir`, `-L` and the source paths differently, so
little of another checkout's session is reused. Seeding the same tree is where the feature
pays: on a checkout that had built once, deleting the incremental state and editing one
library line rebuilt in 17.0 s without seeding and 2.3 s with a snapshot taken from that
same compiled state.

Build scripts: on a fresh checkout of the same revision, 313 of 318 units came from the cache
(35 build-script units used to miss every time). The snapshot cost measured during that
debug build was 261 files / 314.2 MB in 249 ms for the library lineage and 218 files / 54.1 MB
in 100 ms for the binary lineage, about 0.8% of a 44 s cold build.

Raw runs: [`bench/results-engine-2026-10-21-v0.2-before.json`](../bench/results-engine-2026-10-21-v0.2-before.json)
and [`bench/results-engine-2026-10-21-v0.2-after.json`](../bench/results-engine-2026-10-21-v0.2-after.json).

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
