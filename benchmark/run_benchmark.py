#!/usr/bin/env python3
"""Benchmark dbt-sa-cli (dbt) against a 1000-model chain project.

Steps
─────
1. Generate the synthetic dbt project (1 000 models, ~32 kB SQL each).
2. Driver warm-up: run model_0001 once so the ADBC DuckDB driver is downloaded
   and cached before timing begins.
3. Full materialisation: dbt run (all models) so every upstream table exists.
4. Benchmark  dbt run --select  — SAMPLE_MODELS models, one dbt invocation each,
                                   report mean + stdev.
5. Benchmark  dbt run (stdin loop) — same models piped via stdin to a single
                                   long-lived process; parse overhead paid once.
6. Print a comparison table.
"""
import os
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import List, Optional, Tuple

# ── tunables ──────────────────────────────────────────────────────────────────
PROJECT_DIR = os.environ.get("BENCHMARK_PROJECT_DIR", "/tmp/benchmark_project")
DBT_BIN     = os.environ.get("DBT_BIN", "dbt")

# Model indices to sample for both benchmark modes (spread across chain).
SAMPLE_INDICES = list(range(1, 100, 5))

COMMON_FLAGS = [
    "--project-dir", PROJECT_DIR,
    "--profiles-dir", PROJECT_DIR,
    # "--log-level", "off",
    "--debug",
    "--no-write-json",
]


# ── helpers ───────────────────────────────────────────────────────────────────

def run(
    cmd: List[str],
    check: bool = True,
    env: Optional[dict] = None,
    input: Optional[str] = None,
) -> Tuple[float, int]:
    """Run *cmd*, return (wall_seconds, returncode)."""
    t0 = time.perf_counter()
    if isinstance(input, str):
        input = input.encode()
    result = subprocess.run(
        cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env, input=input
    )
    elapsed = time.perf_counter() - t0

    stdout = result.stdout.decode(errors="replace")
    print(stdout[-2000:])

    if check and result.returncode != 0:
        stderr = result.stderr.decode(errors="replace")
        stdout = result.stdout.decode(errors="replace")
        raise RuntimeError(
            f"Command failed (rc={result.returncode}):\n"
            f"  {' '.join(cmd)}\n"
            f"--- stdout ---\n{stdout[-2000:]}\n"
            f"--- stderr ---\n{stderr[-2000:]}"
        )
    return elapsed, result.returncode


def model_name(idx: int) -> str:
    return f"model_{idx:04d}"


def fmt_ms(seconds: float) -> str:
    return f"{seconds * 1000:.1f} ms"


def fmt_stats(times: List[float]) -> str:
    if not times:
        return "n/a"
    mean = statistics.mean(times)
    if len(times) > 1:
        stdev = statistics.stdev(times)
        return f"{fmt_ms(mean)} ± {fmt_ms(stdev)}"
    return fmt_ms(mean)


# ── project generation ────────────────────────────────────────────────────────

def generate_project() -> None:
    script = Path(__file__).parent / "generate_project.py"
    if not script.exists():
        raise FileNotFoundError(f"generate_project.py not found at {script}")
    run([sys.executable, str(script), PROJECT_DIR], check=True)


# ── driver / full-run warm-up ─────────────────────────────────────────────────

def warmup_driver() -> None:
    """Materialise model_0000 so the DuckDB ADBC driver is downloaded+cached."""
    print("  Warming up ADBC driver (first run downloads the DuckDB driver) …",
          flush=True)
    run([DBT_BIN, "run"] + COMMON_FLAGS, input="model_0000")


def full_run() -> float:
    """Run all models once so every upstream table exists. Returns wall time."""
    print("  Running all 1 000 models to populate the database …", flush=True)
    elapsed, _ = run([DBT_BIN, "run"] + COMMON_FLAGS, input="*")
    return elapsed


# ── dbt run --select X benchmark (one process per model) ─────────────────────

def bench_run_single(indices: List[int]) -> List[float]:
    """Spawn a fresh dbt process for each model selector. Returns per-run times."""
    times = []
    for idx in indices:
        m = model_name(idx)
        elapsed, _ = run([DBT_BIN, "run"] + COMMON_FLAGS, input=m)
        times.append(elapsed)
        print(f"    {m}: {fmt_ms(elapsed)}", flush=True)
    return times


# ── dbt run stdin-loop benchmark (one process, N selectors via stdin) ─────────

def bench_run_stdin(indices: List[int]) -> Tuple[float, int]:
    """Pipe all model selectors into a single dbt run process via stdin.

    The compilation.rs stdin loop reads one selector per line, builds a fresh
    schedule for it, and executes the tasks — parse overhead is paid only once.
    An empty line (EOF) terminates the loop.

    Returns (total_elapsed_seconds, number_of_selectors).
    """
    selectors = "\n".join(model_name(idx) for idx in indices) + "\n"
    elapsed, _ = run([DBT_BIN, "run"] + COMMON_FLAGS, input=selectors)
    return elapsed, len(indices)

# ── results printer ───────────────────────────────────────────────────────────

def print_results(
    run_times: List[float],
    stdin_total: float,
    full_run_time: float,
) -> None:
    n = len(run_times)
    single_total = sum(run_times)
    stdin_avg = stdin_total / n if n else 0.0
    speedup = single_total / stdin_total if stdin_total > 0 else float("inf")

    sep = "─" * 70
    print()
    print("╔══════════════════════════════════════════════════════════════════╗")
    print("║                  dbt benchmark results (1 000 models)            ║")
    print("╠══════════════════════════════════════════════════════════════════╣")

    def row(label: str, value: str) -> None:
        print(f"║  {label:<40}  {value:>20}  ║")

    row("Full dbt run (1 000 models, setup)", fmt_ms(full_run_time))
    print(f"║  {sep[:66]}  ║")

    row(f"dbt run --select X  (n={n}, separate processes)", "")
    row("  mean ± stdev (per invocation)", fmt_stats(run_times))
    row("  min / max", f"{fmt_ms(min(run_times))} / {fmt_ms(max(run_times))}")
    row("  total", fmt_ms(single_total))
    print(f"║  {sep[:66]}  ║")

    row(f"dbt run stdin loop  (n={n}, single process)", "")
    row("  total", fmt_ms(stdin_total))
    row("  avg per selector", fmt_ms(stdin_avg))
    print(f"║  {sep[:66]}  ║")

    row("Speedup  (stdin loop vs separate processes)", f"{speedup:.2f}×")
    print("╚══════════════════════════════════════════════════════════════════╝")
    print()


# ── main ──────────────────────────────────────────────────────────────────────

def main() -> None:
    print("═" * 70)
    print("  dbt benchmark — 1 000-model linear chain, DuckDB adapter")
    print("═" * 70)

    # Step 1 — generate project
    print("\n[1/4] Generating dbt project …", flush=True)
    generate_project()

    # Step 2 — driver warm-up (downloads ADBC DuckDB driver if absent)
    print("\n[2/4] Driver warm-up …", flush=True)
    warmup_driver()

    # Step 3 — full run to populate every model's table
    print("\n[3/4] Full dbt run (initial materialisation) …", flush=True)
    full_run_time = full_run()
    print(f"  Completed in {fmt_ms(full_run_time)}", flush=True)

    # Step 4a — separate-process baseline: one dbt invocation per model
    print(
        f"\n[4/4] Benchmarking {len(SAMPLE_INDICES)} models …",
        flush=True,
    )
    print(
        f"  4a) separate processes ({len(SAMPLE_INDICES)} invocations) …",
        flush=True,
    )
    run_times = bench_run_single(SAMPLE_INDICES)

    # Step 4b — stdin-loop: single dbt process, all selectors piped via stdin
    print(
        f"\n  4b) stdin loop (single process, {len(SAMPLE_INDICES)} selectors) …",
        flush=True,
    )
    stdin_total, _ = bench_run_stdin(SAMPLE_INDICES)
    print(f"    total: {fmt_ms(stdin_total)}", flush=True)

    # Print comparison table
    print_results(run_times, stdin_total, full_run_time)


if __name__ == "__main__":
    main()
