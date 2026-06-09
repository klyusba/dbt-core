#!/usr/bin/env python3
"""Generate a synthetic dbt project with NUM_MODELS models in a linear chain.

Each model (except the root) contains a WHERE clause comparing two large random
string literals (STRING_LEN characters each) to make individual model files
large enough to stress the parser.

Usage:
    python3 generate_project.py <output_dir>
"""
import os
import random
import string
import sys

NUM_MODELS = 1000
STRING_LEN = 16_384  # characters per literal; two per model → ~32 kB SQL per file


def _rand_str(length: int) -> str:
    alphabet = string.ascii_lowercase + string.digits
    return "".join(random.choices(alphabet, k=length))


def generate(project_dir: str) -> None:
    models_dir = os.path.join(project_dir, "models")
    os.makedirs(models_dir, exist_ok=True)

    # ── dbt_project.yml ──────────────────────────────────────────────────────
    with open(os.path.join(project_dir, "dbt_project.yml"), "w") as f:
        f.write(
            "name: 'benchmark'\n"
            "version: '1.0.0'\n"
            "config-version: 2\n"
            "profile: 'benchmark'\n"
            "model-paths: [\"models\"]\n"
            "models:\n"
            "  benchmark:\n"
            "    +materialized: table\n"
        )

    # ── profiles.yml ─────────────────────────────────────────────────────────
    with open(os.path.join(project_dir, "profiles.yml"), "w") as f:
        f.write(
            "benchmark:\n"
            "  target: dev\n"
            "  outputs:\n"
            "    dev:\n"
            "      type: duckdb\n"
            f"      path: {project_dir}/benchmark.duckdb\n"
            "      schema: main\n"
        )

    # ── models ───────────────────────────────────────────────────────────────
    print(f"Generating {NUM_MODELS} models in {models_dir} …", flush=True)
    for i in range(NUM_MODELS):
        name = f"model_{i:04d}"
        path = os.path.join(models_dir, f"{name}.sql")

        if i == 0:
            sql = "select 1 as id\n"
        else:
            prev = f"model_{i - 1:04d}"
            a = _rand_str(STRING_LEN)
            b = _rand_str(STRING_LEN)
            sql = (
                f"select 1\n"
                f"from {{{{ ref('{prev}') }}}}\n"
                f"where '{a}' = '{b}'\n"
            )

        with open(path, "w") as f:
            f.write(sql)

        if (i + 1) % 100 == 0:
            print(f"  {i + 1}/{NUM_MODELS}", flush=True)

    print("Project generation complete.", flush=True)


if __name__ == "__main__":
    out = sys.argv[1] if len(sys.argv) > 1 else "/tmp/benchmark_project"
    generate(out)
