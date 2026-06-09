#!/usr/bin/env python3
"""Benchmark dbt-sa-cli (dbt) and dbt-daemon against a 1000-model chain project.

Steps
─────
1. Generate the synthetic dbt project (1 000 models, ~32 kB SQL each).
2. Driver warm-up: run model_0001 once so the ADBC DuckDB driver is downloaded
   and cached before timing begins.
3. Full materialisation: dbt run (all models) so every upstream table exists.
4. Benchmark  dbt parse         — N_PARSE runs, report median + stdev.
5. Benchmark  dbt run --select  — SAMPLE_MODELS models, one dbt invocation each,
                                   report mean + stdev.
6. Start dbt-daemon, warm it up, then repeat step 5 via dbt-daemon.
7. Print a results table.
"""
import os
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import List, Optional, Tuple

# ── tunables ──────────────────────────────────────────────────────────────────
PROJECT_DIR = os.environ.get("BENCHMARK_PROJECT_DIR", "/tmp/benchmark_project")
DBT_BIN     = os.environ.get("DBT_BIN",    "dbt")
DAEMON_BIN  = os.environ.get("DAEMON_BIN", "dbt-daemon")
SOCKET_PATH = os.environ.get("DAEMON_SOCKET", "/tmp/dbt-benchmark-daemon.sock")

# Model indices to sample for `dbt run --select X` (spread across chain)
SAMPLE_INDICES = list(range(1, 200))
DAEMON_WARMUP_MODELS = 2  # dummy runs before daemon timing starts

COMMON_FLAGS = [
    "--project-dir", PROJECT_DIR,
    "--profiles-dir", PROJECT_DIR,
    "--log-level", "off",
    "--no-write-json",
]


# ── helpers ───────────────────────────────────────────────────────────────────

def run(
    cmd: List[str],
    check: bool = True,
    env: Optional[dict] = None,
) -> Tuple[float, int]:
    """Run *cmd*, return (wall_seconds, returncode)."""
    t0 = time.perf_counter()
    result = subprocess.run(
        cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env
    )
    elapsed = time.perf_counter() - t0
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
    run([DBT_BIN, "run", "--select", "model_0000"] + COMMON_FLAGS)


def full_run() -> float:
    """Run all models once so every upstream table exists. Returns wall time."""
    print("  Running all 1 000 models to populate the database …", flush=True)
    elapsed, _ = run([DBT_BIN, "run"] + COMMON_FLAGS)
    return elapsed


# ── dbt run --select X benchmark ─────────────────────────────────────────────

def bench_run_single(indices: List[int]) -> List[float]:
    times = []
    for idx in indices:
        m = model_name(idx)
        elapsed, _ = run([DBT_BIN, "run", "--select", m] + COMMON_FLAGS)
        times.append(elapsed)
        print(f"    {m}: {fmt_ms(elapsed)}", flush=True)
    return times


# ── daemon helpers ────────────────────────────────────────────────────────────

def wait_for_socket(path: str, timeout: float = 60.0) -> bool:
    """Block until the Unix socket at *path* is connectable or timeout."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            s.settimeout(1.0)
            s.connect(path)
            s.close()
            return True
        except (OSError, ConnectionRefusedError):
            time.sleep(0.2)
    return False


def start_daemon() -> subprocess.Popen:
    if Path(SOCKET_PATH).exists():
        Path(SOCKET_PATH).unlink(missing_ok=True)
    log = open("/tmp/dbt-daemon.log", "w")
    # Start the daemon with an explicit socket path.
    proc = subprocess.Popen(
        [DAEMON_BIN, "serve", "--socket", SOCKET_PATH],
        stdout=log,
        stderr=log,
    )
    print(f"  Daemon PID {proc.pid}, socket {SOCKET_PATH}", flush=True)
    return proc


def stop_daemon(proc: subprocess.Popen) -> None:
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
    if Path(SOCKET_PATH).exists():
        Path(SOCKET_PATH).unlink(missing_ok=True)


def daemon_run(model: str) -> float:
    """Send one `dbt run --select <model>` to the running daemon.

    The socket path is communicated via DBT_DAEMON_SOCKET so the --socket flag
    is NOT included in the forwarded args (the daemon server re-parses args with
    the dbt CLI parser which does not know --socket).
    """
    env = os.environ.copy()
    env["DBT_DAEMON_SOCKET"] = SOCKET_PATH
    cmd = [DAEMON_BIN, "run", "--select", model] + COMMON_FLAGS
    elapsed, _ = run(cmd, env=env)
    return elapsed


def bench_daemon(indices: List[int]) -> List[float]:
    proc = start_daemon()
    try:
        print("  Waiting for daemon socket …", flush=True)
        if not wait_for_socket(SOCKET_PATH, timeout=120):
            raise RuntimeError("Daemon did not become ready within 120 s")
        print("  Daemon ready.", flush=True)

        # warm-up: let the daemon parse + materialise a couple of models
        print(f"  Daemon warm-up ({DAEMON_WARMUP_MODELS} invocations) …",
              flush=True)
        for i in range(DAEMON_WARMUP_MODELS):
            t = daemon_run(model_name(SAMPLE_INDICES[i]))
            print(f"    warm-up {i + 1}: {fmt_ms(t)}", flush=True)

        # timed runs
        times = []
        for idx in indices:
            m = model_name(idx)
            elapsed = daemon_run(m)
            times.append(elapsed)
            print(f"    {m}: {fmt_ms(elapsed)}", flush=True)
        return times
    finally:
        stop_daemon(proc)


# ── results printer ───────────────────────────────────────────────────────────

def print_results(
    parse_times:  List[float],
    run_times:    List[float],
    daemon_times: List[float],
    full_run_time: float,
) -> None:
    sep = "─" * 70
    print()
    print("╔══════════════════════════════════════════════════════════════════╗")
    print("║                  dbt benchmark results (1 000 models)           ║")
    print("╠══════════════════════════════════════════════════════════════════╣")

    def row(label: str, value: str) -> None:
        print(f"║  {label:<40}  {value:>20}  ║")

    row("Full dbt run (1 000 models, setup)", fmt_ms(full_run_time))
    print(f"║  {sep[:66]}  ║")

    row(f"dbt parse  (n={N_PARSE}, median)",
        fmt_ms(statistics.median(parse_times)))
    row("  mean ± stdev", fmt_stats(parse_times))
    print(f"║  {sep[:66]}  ║")

    row(f"dbt run --select X  (n={len(run_times)} models)",
        "")
    row("  mean ± stdev (per invocation)", fmt_stats(run_times))
    row("  min", fmt_ms(min(run_times)))
    row("  max", fmt_ms(max(run_times)))
    print(f"║  {sep[:66]}  ║")

    if daemon_times:
        row(f"dbt-daemon run --select X  (n={len(daemon_times)} models)",
            "")
        row("  mean ± stdev (per invocation)", fmt_stats(daemon_times))
        row("  min", fmt_ms(min(daemon_times)))
        row("  max", fmt_ms(max(daemon_times)))

        speedup = statistics.mean(run_times) / statistics.mean(daemon_times)
        row("  speedup vs plain dbt run", f"{speedup:.2f}×")
    else:
        row("dbt-daemon", "SKIPPED")

    print("╚══════════════════════════════════════════════════════════════════╝")
    print()


# ── main ──────────────────────────────────────────────────────────────────────

def main() -> None:
    print("═" * 70)
    print("  dbt benchmark — 1 000-model linear chain, DuckDB adapter")
    print("═" * 70)

    # Step 1 — generate project
    print("\n[1/5] Generating dbt project …", flush=True)
    generate_project()

    # Step 2 — driver warm-up (downloads ADBC DuckDB driver if absent)
    print("\n[2/5] Driver warm-up …", flush=True)
    warmup_driver()

    # Step 3 — full run to populate every model's table
    print("\n[3/5] Full dbt run (initial materialisation) …", flush=True)
    full_run_time = full_run()
    print(f"  Completed in {fmt_ms(full_run_time)}", flush=True)

    # Step 4 — dbt run --select X (one model per process)
    print(
        f"\n[4/5] Benchmarking dbt run --select X "
        f"({len(SAMPLE_INDICES)} models, one per invocation) …",
        flush=True,
    )
    run_times = bench_run_single(SAMPLE_INDICES)

    # Step 6 — dbt-daemon run --select X
    daemon_times: List[float] = []
    if shutil.which(DAEMON_BIN):
        print(
            f"\n[5/5] Benchmarking dbt-daemon run --select X "
            f"({len(SAMPLE_INDICES)} models) …",
            flush=True,
        )
        try:
            daemon_times = bench_daemon(SAMPLE_INDICES)
        except Exception as exc:
            print(f"  WARNING: daemon benchmark failed — {exc}", flush=True)
    else:
        print(f"\n[5/5] {DAEMON_BIN} not found — skipping daemon benchmark.",
              flush=True)

    # Step 7 — print results
    print_results(parse_times, run_times, daemon_times, full_run_time)


if __name__ == "__main__":
    main()
