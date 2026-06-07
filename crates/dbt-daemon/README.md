# dbt-daemon

A persistent daemon for [dbt Fusion](https://github.com/dbt-labs/dbt-core) that keeps project state in memory between invocations, eliminating cold-start overhead when running dbt thousands of times.

## Why

Every normal `dbt run` pays these startup costs from scratch:

| Phase | Typical cost |
|---|---|
| Filesystem walk + YAML parse | ~200 ms |
| Jinja resolution | ~200 ms |
| Adapter init (TLS, connection pool) | ~200 ms |
| Schema hydration (warehouse round-trip) | ~500 ms+ |
| **Total cold-start overhead** | **~1 s+** |

With `dbt-daemon` the process stays alive. Subsequent invocations skip directly to execution. When source files haven't changed the existing partial-parse fast path makes re-parsing sub-millisecond.

## Architecture

```
┌──────────────┐   Unix socket   ┌────────────────────────────────┐
│  dbt-daemon  │ ──────────────▶ │  dbt-daemon serve              │
│  (client)    │ ◀────────────── │                                │
│  exit code   │   exit code     │  FeatureStack (alive forever)  │
└──────────────┘                 │  CliParser    (alive forever)  │
                                 │  Tokio runtime (alive forever) │
                                 └────────────────────────────────┘
```

The daemon is built entirely on public APIs from `dbt-main`. No existing source files in the workspace are modified — only a single line is added to the workspace `Cargo.toml` members list.

## Building

This crate is part of the [dbt Fusion](https://github.com/dbt-labs/dbt-core) workspace and must be built from within it.

```sh
# Clone the workspace (or your fork)
git clone https://github.com/dbt-labs/dbt-core
cd dbt-core

# Build the daemon binary
cargo build --release -p dbt-daemon

# The binary is at
./target/release/dbt-daemon
```

Optionally install it to `~/.cargo/bin`:

```sh
cargo install --path crates/dbt-daemon
```

## Usage

### 1. Start the daemon

```sh
# Foreground (useful for debugging — output goes to your terminal)
dbt-daemon serve

# Background (recommended for automation)
dbt-daemon serve > ~/.dbt/daemon.log 2>&1 &

# Custom socket path
dbt-daemon serve --socket /tmp/my-dbt.sock
```

The daemon binds a Unix socket at `~/.dbt/daemon.sock` by default and waits for commands.

### 2. Forward commands

```sh
# Every dbt command works — just prefix with dbt-daemon instead of dbt
dbt-daemon run --select my_model --project-dir /path/to/project
dbt-daemon compile --select my_model --project-dir /path/to/project
dbt-daemon test --select my_model --project-dir /path/to/project
```

> **Note:** Because the daemon has its own working directory, always pass
> `--project-dir` explicitly (or set the socket via `DBT_DAEMON_SOCKET` and
> ensure the daemon was started from the project root).

### 3. Set up a transparent alias

```sh
# In ~/.bashrc or ~/.zshrc
alias dbt='dbt-daemon'

# Now this transparently uses the daemon
dbt run --select my_model
```

The client exits with the same exit code as the dbt command, so CI pipelines work without modification.

### 4. Check status / stop

```sh
# Is the daemon running?
dbt-daemon status

# Stop it
kill $(pgrep dbt-daemon)
# or if you saved the PID:
kill $(cat ~/.dbt/daemon.pid)
```

## Configuration

| Mechanism | Effect |
|---|---|
| `DBT_DAEMON_SOCKET` env var | Override the default socket path (`~/.dbt/daemon.sock`) |
| `--socket PATH` flag | Override the socket path for that invocation |

## Protocol

Communication uses a simple length-prefixed JSON protocol over a Unix domain socket:

- **Request** (client → daemon): `{ "args": ["dbt", "run", "--select", "foo"], "cwd": "/path/to/project" }`
- **Response** (daemon → client): `{ "exit_code": 0 }`

The frame format is: 4 bytes big-endian payload length followed by UTF-8 JSON.

## Concurrency

Requests are processed **serially** — one at a time. This is intentional:

- dbt itself is not designed for concurrent warehouse execution from the same project
- It avoids races on process-global state (rustls provider, working directory)
- Clients queue naturally; the daemon processes them in order

## Limitations

- **Unix only** — the Unix domain socket transport does not work on Windows
- **Output goes to daemon's terminal** — the client receives only the exit code; stdout/stderr from dbt appear in the daemon's own output (redirect with `> log 2>&1` when running in background)
- **Single project per daemon** — start one daemon per project directory, using different `--socket` paths if needed
- **No Ctrl+C forwarding** — killing the client does not cancel the in-progress dbt command on the daemon; the daemon continues to completion

## How it differs from `dbt-rpc` (deprecated)

The old Python `dbt-rpc` was a full HTTP server with a JSON-RPC API. `dbt-daemon` is intentionally simpler:

- **No HTTP** — bare Unix socket, minimal overhead
- **No JSON-RPC** — one request, one response, connection closed
- **No state management API** — the daemon manages state automatically
- **No authentication** — socket permissions (`0600`) restrict access to the current user

## Source layout

```
crates/dbt-daemon/
├── Cargo.toml
├── README.md          ← you are here
└── src/
    ├── main.rs        entry point, server/client dispatch
    ├── protocol.rs    wire protocol (frame codec, request/response types)
    ├── server.rs      Unix socket server loop
    ├── client.rs      thin client (connect, send, receive exit code)
    └── state.rs       socket path resolution
```

## Contributing

This crate lives in a fork of the upstream dbt Fusion workspace. To contribute:

1. Fork [https://github.com/dbt-labs/dbt-core](https://github.com/dbt-labs/dbt-core)
2. Add `"crates/dbt-daemon"` to the `[workspace.members]` list in the root `Cargo.toml`
3. Drop the `crates/dbt-daemon/` directory into the workspace
4. `cargo check -p dbt-daemon`
