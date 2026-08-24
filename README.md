# ka

A model-agnostic, very-low-footprint coding agent in Rust.

**Status: Phase 1 (real wires).** Design: [`research/ka/architecture.md`](research/ka/architecture.md) · Roadmap: [`research/ka/roadmap.md`](research/ka/roadmap.md)

## Footprint contract (CI-enforced target)
Single static binary ≤ 10 MB (musl, stripped) · cold start ≤ 50 ms · idle RSS ≤ 15 MB · zero steady-state network.

## Crates
| Crate | Role |
|---|---|
| `ka-protocol` | `Command`/`Event` wire contract (NDJSON-serializable) |
| `ka-agent` | engine: turn machine, queues, layered strict-TOML config |
| `ka-dialect` | dialect catalog + **wires**: openai-chat & anthropic-messages Speakers, SSE client, retry/classify, JSON repair, token ladder, local discovery |
| `ka-agent` engine | live voice path (real models) + canned fallback; cost from usage |
| `ka-strand` | append-only JSONL session records |
| `ka-term` | terminal primitives (raw-mode guard, key decoding); TUI in Phase 3 |
| `ka-cli` | the `ka` binary |

## Develop
```sh
cargo fmt
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p ka-cli -- run "hi"          # canned path, no model needed
cargo run -p ka-cli -- models            # catalog + local discovery (Ollama, LM Studio)
cargo run -p ka-cli -- run --model ollama/qwen3:32b "hi"   # live local round-trip
```

Selectors are `vendor/model@effort` (`@` because model ids may contain colons).

Publishing note: `ka` is taken on crates.io; all crates publish under the free `ka-*` names, binary stays `ka`.
