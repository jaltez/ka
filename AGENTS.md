# AGENTS.md

ka: a model-agnostic, low-footprint coding agent in Rust. Single binary, ≤ 10 MB musl, no daemons.

## Commands

```sh
cargo xtask ci        # the CI gate: fmt --check + clippy -D warnings + tests — run before pushing
cargo build -p ka-agent          # debug build
cargo test --workspace --all-features
cargo xtask dev -- models        # rebuild + run dev binary with args
cargo xtask size                 # binary size vs the 10 MB contract
cargo xtask link                 # build release + symlink `kad` → target/release/ka
```

Stable owns the name `ka`; dev is always `kad`. Isolate dev runs with `KA_DATA_DIR=/tmp/ka-dev kad …`.

## Workspace

| Crate | Role |
|---|---|
| `ka-protocol` | Command/Event wire enums (NDJSON-shaped) |
| `ka-engine` | turn machine, tools (Hands) + clearance gate, digests, ledger |
| `ka-dialect` | model catalog + wires (anthropic-messages, openai-chat, openai-responses) + local discovery |
| `ka-strand` | append-only JSONL session store, waypoints |
| `ka-term` | ratatui TUI |
| `ka-agent` | the published `ka` binary |
| `ka-index` / `ka-sandbox` | optional: FTS search (`index` feature), fs sandboxing |
| `xtask` | repo automation |

Dependency direction is one-way: `ka-agent → ka-term/ka-protocol/ka-dialect/ka-strand → ka-engine`. The engine never imports a surface, never does I/O beyond std+serde.

## Hard rules

- `unsafe_code` forbidden; clippy denies `unwrap`/`expect`/`dbg`/`todo`/`unimplemented` — return `Result` instead. CI enforces on all targets/features.
- Footprint contract (CI-gated): ≤ 10 MB stripped musl binary, cold start ≤ 50 ms, idle RSS ≤ 15 MB. Any new dependency or >100 KB feature must justify itself; prefer cargo features or data files over code.
- **Data over code**: model quirks go in `crates/ka-dialect/dialects.toml` / `models-dev.toml`, permission rules and tool catalogs are data — never branches in the engine.
- Strict TOML everywhere: unknown config keys are hard errors with line numbers. Keep it that way.
- Fail toward boring: exact-match edits, append-only logs, fail-closed sandboxes.

## Conventions

- Vocabulary is Ka's own (strand, record, digest, ledger, Hands, Speaker, clearance, hardstops…). Glossary + lineage: `research/ka/architecture.md` §1. Design docs: `research/ka/`; `research/` is a survey, not code.
- Tests are contracts: ~530, including documented-feature contracts — CI fails if a README-documented behavior regresses. When changing documented behavior, update README **and** the contract test together.
- Wire changes need recorded SSE fixture tests (normal, malformed, partial-JSON repair paths). No keys in CI, ever.
- Rust edition 2024, MSRV 1.85. Workspace deps only via `[workspace.dependencies]`.
