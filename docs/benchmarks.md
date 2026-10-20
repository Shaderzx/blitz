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
| date | TBD |
| machine | TBD |
| CPU / memory | TBD |
| OS / filesystem | TBD |
| rustc | TBD |
| blitz commit | TBD |
| repository and commit | TBD |

## Results

Numbers are filled in from a real run on the machine above. This section is structure
until then.

| scenario | median seconds | speedup vs cargo cold | speedup vs cargo no-op |
| --- | --- | --- | --- |
| cargo cold | TBD | 1.00x | |
| blitz cold | TBD | TBD | |
| blitz warm | TBD | TBD | |
| blitz warm + edit | TBD | TBD | |
| cargo no-op | TBD | TBD | 1.00x |
| blitz no-op | TBD | TBD | TBD |

Binary check: after `blitz warm`, the restored binary must be executable and the same
size class as the one cargo produced (within 25%). TBD.

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
