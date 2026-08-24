# ka

A model-agnostic, very-low-footprint coding agent in Rust.

**Status: Phase 7 partial (plan mode, rewind).** Design: [`research/ka/architecture.md`](research/ka/architecture.md) · Roadmap: [`research/ka/roadmap.md`](research/ka/roadmap.md)

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
cargo run -p ka-cli -- run "hi"          # canned path, no model needed (persists a strand)
cargo run -p ka-cli                       # TUI: session picker, meters, ask dialogs
cargo run -p ka-cli -- -c                 # continue the newest strand (waypoint-aware)
cargo run -p ka-cli -- run --model ollama/qwen3.5:9b 'read secret.txt with the read tool'  # live tools
cargo run -p ka-cli -- models            # catalog + local discovery (Ollama, LM Studio)
cargo run -p ka-cli -- run --model ollama/qwen3:32b "hi"   # live local round-trip
```

Selectors are `vendor/model@effort` (`@` because model ids may contain colons).

Conventions: AGENTS.md (root→cwd, CLAUDE.md compat) folds into every system prompt; SKILL.md skills (`.ka/skills/`, `.agents/`, `.claude/`) list name+description+path only — the model reads bodies on demand. Hooks (exit-2 block contract):

```toml
[[hooks]]
event = "pre_tool_use"   # or post_tool_use
tool = "bash"            # optional filter
command = "guard.sh"     # JSON on stdin; exit 2 blocks
```

**Plan mode** (`--mode plan` or TUI `/plan`): read-only except `.ka/plans/` (enforced at the gate); `/build` switches back and starts implementation from the plan file. **Rewind**: `/rewind N` drops the last N exchanges (persisted as a `rewind` record; resume reconstructs the truncated history).

`pathfinder` delegates read-only research (read/glob/grep) to a nested voice and returns a dense summary. Custom slash commands: `.ka/commands/<name>.md` with `$ARGUMENTS`. `ka init` writes a starter AGENTS.md.

Project config (`.ka/ka.toml`) is trust-gated: first use prompts (or pass `--trust`), decisions persist in `~/.local/state/ka/trust.json`. Permission rules are data, evaluated first-match-wins before mode logic:

```toml
[[rules]]
tool = "bash"
pattern = "cargo *"   # glob on the primary argument
verdict = "allow"     # allow | ask | deny
```

Publishing note: `ka` is taken on crates.io; all crates publish under the free `ka-*` names, binary stays `ka`.
