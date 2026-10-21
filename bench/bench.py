#!/usr/bin/env python3
"""Benchmark blitz against cargo on a real git repository.

Every scenario runs in a fresh clone under a temporary directory, so no run inherits a
built target directory. The cargo registry is fetched before timing, so network time is
not part of the numbers.

    bench/bench.py <git repo path> [--runs N] [--json results.json]

Scenarios: cargo cold, blitz cold (empty cache), blitz warm (warm cache), blitz warm
with one edited top-crate source file, and the no-op rebuild for both tools. The table
prints medians in seconds and speedups against cargo cold; no-op rows also show the
speedup against cargo's own no-op.
"""

import argparse
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

WARM_RUNS = 10
EDIT_RUNS = 3
NOOP_RUNS = 10


def unset_inherited(env):
    for name in (
        "BLITZ_CACHE_DIR",
        "BLITZ_CACHE_RO",
        "BLITZ_LOG",
        "BLITZ_DISABLE",
        "BLITZ_NO_WRAPPER",
        "BLITZ_READONLY",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_TARGET_DIR",
    ):
        env.pop(name, None)
    env["CARGO_TERM_COLOR"] = "never"
    return env


def run_timed(command, cwd, env):
    """Run a command, return elapsed seconds. Any failure aborts the benchmark."""
    started = time.perf_counter()
    proc = subprocess.run(
        command,
        cwd=str(cwd),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    elapsed = time.perf_counter() - started
    if proc.returncode != 0:
        raise SystemExit(
            "command failed with exit code %d: %s\n%s"
            % (proc.returncode, " ".join(command), proc.stdout)
        )
    return elapsed


def run_untimed(command, cwd, env, check=True):
    proc = subprocess.run(
        command,
        cwd=str(cwd),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    if check and proc.returncode != 0:
        raise SystemExit(
            "preparation failed with exit code %d: %s\n%s"
            % (proc.returncode, " ".join(command), proc.stdout)
        )
    return proc


def clone_repo(repo, workdir, name):
    dest = workdir / name
    if dest.exists():
        shutil.rmtree(dest)
    run_untimed(["git", "clone", "--quiet", "--", str(repo), str(dest)], workdir, os.environ.copy())
    return dest


def prefetch_registry(checkout, env):
    """Fetch every locked dependency once, outside the timed window."""
    proc = run_untimed(["cargo", "fetch", "--locked"], checkout, env, check=False)
    if proc.returncode != 0:
        proc = run_untimed(["cargo", "fetch"], checkout, env, check=False)
    if proc.returncode != 0:
        raise SystemExit("cargo fetch failed in %s\n%s" % (checkout, proc.stdout))


def find_binary(checkout):
    """The largest executable directly in the target directory: the workspace's binary."""
    target = checkout / "target"
    for profile in ("debug", "release"):
        directory = target / profile
        if not directory.is_dir():
            continue
        best = None
        for path in directory.iterdir():
            if not path.is_file() or not os.access(path, os.X_OK):
                continue
            if path.suffix in (".d", ".rlib", ".rmeta", ".so", ".dylib"):
                continue
            size = path.stat().st_size
            if best is None or size > best[1]:
                best = (path, size)
        if best is not None:
            # The checkout is deleted after the scenario, so record what the binary is,
            # not a path that no longer exists.
            return {"name": best[0].name, "profile": profile, "size": best[1]}
    return None


def top_crate_source(checkout):
    """The source file of the top crate: the root package, else the first member."""
    for relative in ("src/main.rs", "src/lib.rs"):
        candidate = checkout / relative
        if candidate.is_file():
            return candidate
    candidates = sorted(checkout.glob("*/src/lib.rs")) + sorted(checkout.glob("*/src/main.rs"))
    candidates = [p for p in candidates if "target" not in p.parts]
    if not candidates:
        raise SystemExit("cannot find a top-crate source file under %s" % checkout)
    return candidates[0]


def median(values):
    return statistics.median(values)


def run_cold_cargo(repo, workdir, env, runs):
    times = []
    binary = None
    for index in range(runs):
        checkout = clone_repo(repo, workdir, "cargo-cold-%d" % index)
        prefetch_registry(checkout, env)
        times.append(run_timed(["cargo", "build", "--locked"], checkout, env))
        if binary is None:
            binary = find_binary(checkout)
        shutil.rmtree(checkout)
    return times, binary


def run_cold_blitz(repo, workdir, env, blitz, runs):
    times = []
    for index in range(runs):
        checkout = clone_repo(repo, workdir, "blitz-cold-%d" % index)
        cache = workdir / ("cache-cold-%d" % index)
        if cache.exists():
            shutil.rmtree(cache)
        cold_env = dict(env)
        cold_env["BLITZ_CACHE_DIR"] = str(cache)
        prefetch_registry(checkout, cold_env)
        times.append(run_timed([blitz, "build", "--locked"], checkout, cold_env))
        shutil.rmtree(checkout)
        shutil.rmtree(cache, ignore_errors=True)
    return times


def prime_cache(repo, workdir, env, blitz, cache):
    """Build the repo once with blitz so the whole-build and per-unit entries exist."""
    if cache.exists():
        shutil.rmtree(cache)
    checkout = clone_repo(repo, workdir, "prime")
    warm_env = dict(env)
    warm_env["BLITZ_CACHE_DIR"] = str(cache)
    prefetch_registry(checkout, warm_env)
    run_untimed([blitz, "build", "--locked"], checkout, warm_env)
    shutil.rmtree(checkout)


def run_warm_blitz(repo, workdir, env, blitz, cache):
    times = []
    binary = None
    for index in range(WARM_RUNS):
        checkout = clone_repo(repo, workdir, "blitz-warm-%d" % index)
        warm_env = dict(env)
        warm_env["BLITZ_CACHE_DIR"] = str(cache)
        prefetch_registry(checkout, warm_env)
        times.append(run_timed([blitz, "build", "--locked"], checkout, warm_env))
        if binary is None:
            binary = find_binary(checkout)
        shutil.rmtree(checkout)
    return times, binary


def run_warm_edit_blitz(repo, workdir, env, blitz, cache):
    times = []
    edited = None
    for index in range(EDIT_RUNS):
        checkout = clone_repo(repo, workdir, "blitz-edit-%d" % index)
        warm_env = dict(env)
        warm_env["BLITZ_CACHE_DIR"] = str(cache)
        prefetch_registry(checkout, warm_env)
        source = top_crate_source(checkout)
        with source.open("a", encoding="utf-8") as handle:
            handle.write("\n// blitz bench: one edited line at %d\n" % time.time())
        if edited is None:
            edited = str(source.relative_to(checkout))
        times.append(run_timed([blitz, "build", "--locked"], checkout, warm_env))
        shutil.rmtree(checkout)
    return times, edited


def run_noop_cargo(repo, workdir, env):
    checkout = clone_repo(repo, workdir, "cargo-noop")
    prefetch_registry(checkout, env)
    run_untimed(["cargo", "build", "--locked"], checkout, env)
    times = [run_timed(["cargo", "build", "--locked"], checkout, env) for _ in range(NOOP_RUNS)]
    shutil.rmtree(checkout)
    return times


def run_noop_blitz(repo, workdir, env, blitz, cache):
    checkout = clone_repo(repo, workdir, "blitz-noop")
    warm_env = dict(env)
    warm_env["BLITZ_CACHE_DIR"] = str(cache)
    prefetch_registry(checkout, warm_env)
    run_untimed([blitz, "build", "--locked"], checkout, warm_env)
    times = [
        run_timed([blitz, "build", "--locked"], checkout, warm_env) for _ in range(NOOP_RUNS)
    ]
    shutil.rmtree(checkout)
    return times


def speedup(reference, value):
    return reference / value if value else float("inf")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("repo", help="path to a git repository to benchmark")
    parser.add_argument("--runs", type=int, default=3, help="cold runs per tool (default 3)")
    parser.add_argument(
        "--blitz",
        default=os.environ.get(
            "BLITZ", str(Path(__file__).resolve().parent.parent / "target" / "release" / "blitz")
        ),
        help="path to the blitz binary (default target/release/blitz, or $BLITZ)",
    )
    parser.add_argument("--json", help="write the results as JSON to this file")
    args = parser.parse_args()

    runs = max(1, args.runs)
    repo = Path(args.repo).resolve()
    if not (repo / ".git").exists():
        raise SystemExit("%s is not a git repository" % repo)
    blitz = str(Path(args.blitz).resolve())
    if not os.access(blitz, os.X_OK):
        raise SystemExit("no executable blitz binary at %s" % blitz)

    env = unset_inherited(os.environ.copy())
    workdir = Path(tempfile.mkdtemp(prefix="blitz-bench-"))
    results = {
        "repo": str(repo),
        "commit": run_untimed(["git", "rev-parse", "HEAD"], repo, env).stdout.strip(),
        "blitz": blitz,
        "rustc": run_untimed(["rustc", "-vV"], repo, env).stdout.splitlines()[0],
        "runs": runs,
        "started_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "scenarios": {},
        "binary": {},
    }
    warm_cache = workdir / "cache-warm"

    try:
        print(
            "benchmarking %s (%s) with %s" % (repo, results["commit"][:12], blitz),
            file=sys.stderr,
        )
        print("preparing the warm cache", file=sys.stderr)
        prime_cache(repo, workdir, env, blitz, warm_cache)

        cargo_cold, cargo_binary = run_cold_cargo(repo, workdir, env, runs)
        blitz_cold = run_cold_blitz(repo, workdir, env, blitz, runs)
        blitz_warm, blitz_binary = run_warm_blitz(repo, workdir, env, blitz, warm_cache)
        blitz_edit, edited = run_warm_edit_blitz(repo, workdir, env, blitz, warm_cache)
        cargo_noop = run_noop_cargo(repo, workdir, env)
        blitz_noop = run_noop_blitz(repo, workdir, env, blitz, warm_cache)
    finally:
        shutil.rmtree(workdir, ignore_errors=True)

    scenarios = {
        "cargo cold": cargo_cold,
        "blitz cold": blitz_cold,
        "blitz warm": blitz_warm,
        "blitz warm + edit": blitz_edit,
        "cargo no-op": cargo_noop,
        "blitz no-op": blitz_noop,
    }
    for name, times in scenarios.items():
        results["scenarios"][name] = {
            "median_seconds": round(median(times), 3),
            "runs_seconds": [round(t, 3) for t in times],
        }
    results["scenarios"]["blitz warm + edit"]["edited_file"] = edited
    results["binary"] = {"cargo": cargo_binary, "blitz": blitz_binary}

    baseline = median(cargo_cold)
    noop_baseline = median(cargo_noop)

    print()
    print("| scenario | median seconds | speedup vs cargo cold | speedup vs cargo no-op |")
    print("| --- | --- | --- | --- |")
    for name in scenarios:
        seconds = median(scenarios[name])
        against_cold = "%.2fx" % speedup(baseline, seconds)
        if name.endswith("no-op"):
            against_noop = "%.2fx" % speedup(noop_baseline, seconds)
        else:
            against_noop = ""
        print("| %s | %.3f | %s | %s |" % (name, seconds, against_cold, against_noop))

    if blitz_binary and cargo_binary:
        blitz_size = blitz_binary["size"]
        cargo_size = cargo_binary["size"]
        ratio = blitz_size / cargo_size if cargo_size else 0.0
        same_class = 0.75 <= ratio <= 1.25
        results["binary"]["size_ratio"] = round(ratio, 3)
        results["binary"]["same_size_class"] = same_class
        print()
        print(
            "binary check: the restored binary is executable and %.2f MB against cargo's %.2f MB "
            "(%.3fx) - %s"
            % (
                blitz_size / 1e6,
                cargo_size / 1e6,
                ratio,
                "same size class" if same_class else "SIZE MISMATCH",
            )
        )
        if not same_class:
            raise SystemExit("the restored binary is not the same size class as cargo's")
    else:
        print()
        print("binary check: skipped, no binary was produced in the target directory")
        results["binary"]["same_size_class"] = None

    if args.json:
        Path(args.json).write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
        print("\nwrote %s" % args.json)


if __name__ == "__main__":
    main()
