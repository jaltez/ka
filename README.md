# ka

A model-agnostic, very-low-footprint coding agent in Rust.

**Status: Phase 0 (skeleton & contracts).** Design: [`research/ka/architecture.md`](research/ka/architecture.md) · Roadmap: [`research/ka/roadmap.md`](research/ka/roadmap.md)

## Footprint contract (CI-enforced target)
Single static binary ≤ 10 MB (musl, stripped) · cold start ≤ 50 ms · idle RSS ≤ 15 MB · zero steady-state network.

## Crates
| Crate | Role |
|---|---|
| `ka-protocol` | `Command`/`Event` wire contract (NDJSON-serializable) |
| `ka-agent` | engine: turn machine, queues, layered strict-TOML config |
| `ka-dialect` | model catalog (`dialects.toml`) + selectors; wires land Phase 1 |
| `ka-strand` | append-only JSONL session records |
| `ka-term` | terminal primitives (raw-mode guard, key decoding); TUI in Phase 3 |
| `ka-cli` | the `ka` binary |

## Develop
```sh
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p ka-cli -- run "hi"     # streams NDJSON events through the real queues
```

Publishing note: `ka` is taken on crates.io; all crates publish under the free `ka-*` names, binary stays `ka`.
