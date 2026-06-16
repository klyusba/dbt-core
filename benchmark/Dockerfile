# ╔══════════════════════════════════════════════════════════════════════════╗
# ║  dbt benchmark image                                                    ║
# ║                                                                         ║
# ║  Stage 1 (builder) — compiles dbt-sa-cli + dbt-daemon from source      ║
# ║  Stage 2 (benchmark) — runs the synthetic 1 000-model benchmark        ║
# ╚══════════════════════════════════════════════════════════════════════════╝

# ── Stage 1: Rust build ───────────────────────────────────────────────────────
FROM rust:1.91-slim-bookworm AS builder

# System packages needed at compile time
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        ca-certificates \
        git \
        protobuf-compiler \
        libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Copy workspace manifests first so Cargo can fetch & cache registry metadata
# before the (much larger) source tree is copied.
# (Cargo.lock is not committed in this repo, so we only copy what exists.)
COPY Cargo.toml rust-toolchain.toml ./

# Copy all crate sources in one layer (avoids repeated COPY stanzas for each crate)
COPY crates/ crates/

# Build only the two binaries we need.
# --no-default-features skips the web-UI embed step.
# Override LTO + codegen-units to keep peak link memory manageable inside Docker
# (thin-LTO with codegen-units=1 can exhaust RAM on machines with <8 GB Docker limit).
# -j4 caps parallel rustc invocations to reduce memory pressure.
ENV CARGO_PROFILE_RELEASE_LTO=off \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=4 \
    CARGO_PROFILE_RELEASE_OPT_LEVEL=3

RUN --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/usr/local/cargo/registry \
    cargo build --release \
        --no-default-features \
        -p dbt-sa-cli \
        -j4 \
    && cp target/release/dbt /out/dbt


# ── Stage 2: Benchmark runtime ────────────────────────────────────────────────
FROM debian:bookworm-slim AS benchmark

RUN apt-get update && apt-get install -y --no-install-recommends \
        python3 \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Expose the binaries as `dbt` and `dbt-daemon`.
COPY --from=builder /src/target/release/dbt-sa-cli  /usr/local/bin/dbt

# Copy benchmark scripts
COPY benchmark/generate_project.py /benchmark/generate_project.py
COPY benchmark/run_benchmark.py    /benchmark/run_benchmark.py

WORKDIR /benchmark

# The ADBC DuckDB driver is downloaded on first use into ~/.cache (or
# $XDG_CACHE_HOME).  Network access is required at container runtime.
ENV BENCHMARK_PROJECT_DIR=/tmp/benchmark_project
ENV DBT_BIN=dbt

CMD ["python3", "/benchmark/run_benchmark.py"]
