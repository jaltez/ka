# ka

A model-agnostic, very-low-footprint coding agent in Rust.

**Status: core complete (Phases 0–7 partial).** Design: [`research/ka/architecture.md`](research/ka/architecture.md) · Roadmap: [`research/ka/roadmap.md`](research/ka/roadmap.md) · Survey of 17 agents that informed it: [`research/`](research/README.md)

## Quickstart

```sh
cargo build --release -p ka-cli
alias ka=target/release/ka            # or: cargo install --path crates/ka-cli

cd your-project
ka --model ollama/qwen3.5:9b          # TUI: fresh chat
ka -c                                 # continue this terminal's last session
ka --session 3f9c2a81                 # resume a session by id prefix
ka sessions                           # list session ids for this directory
ka export --session 3f9c2a81           # export a specific session as markdown
ka mcp                                 # probe [[mcp]] servers, list tools
ka providers                          # provider registry + API-key env status
ka run "summarize the build error"    # headless NDJSON
```

Keys: `Enter` send / interject mid-turn · `+text` defer until turn ends · `Esc`/`Ctrl-C` abort · `↑/↓` history · `/model` without arguments opens a picker over the catalog (filter as you type, key/env status; Enter on an unmatched filter sets it as a custom `vendor/model` selector). Interjections and `+deferrals` post a visible ack line while a turn runs. `PgUp`/`PgDn` scroll the transcript (title shows `↑N above`; `Esc` re-pins to the tail). Rendered rows are cached per entry — streaming redraws only the live region. `/session` (alias `/resume`) opens an in-app session picker with type-to-filter; `/new` starts a fresh session; `/settings` edits model/mode/effort (persist with `s`) and shows every provider's API-key env status. The transcript renders markdown (headers, lists, `code`, fenced blocks with syntax coloring) with role blocks: blue for you, amber for tool calls, dark gray for streamed thinking. NO_COLOR is honored.

## Commands

| Surface | What it does |
|---|---|
| `ka` | TUI, fresh session |
| `ka -c` / `ka --session <id>` | continue newest (waypoint-aware per terminal) / resume by id prefix |
| `ka run [-c] [--model M] [--mode guarded\|free\|plan] [--trust] [--dialects f] "prompt"` | one headless turn, NDJSON events on stdout, exit 0/1/2 |
| `ka models [--no-discovery]` | catalog + local Ollama/LM Studio probes |
| `ka rewind [N]` | drop the last N exchanges of the newest strand |
| `ka export [-o out.md]` | strand as readable markdown |
| `ka init` | starter AGENTS.md from repo shape |
| `ka config {schema,print}` | resolved config / JSON schema |

TUI slash commands: `/model <sel>` `/mode <guarded|free|plan>` `/plan <task>` `/build` `/rewind [N]` `/compact [focus]` `/quit` plus custom `/name` from `.ka/commands/*.md` (`$ARGUMENTS` substituted).

## Selectors & models

`vendor/model@effort` — e.g. `anthropic/claude-sonnet-5@high`, `ollama/qwen3.5:9b` (`@` because model ids may contain colons). Two wires built in (anthropic-messages, openai-chat — the latter covers every OpenAI-compatible endpoint incl. Ollama/vLLM/LM Studio/gateways); local endpoints auto-discovered.

## Config

Strict TOML, layered: defaults → `~/.config/ka/ka.toml` → `.ka/ka.toml` (trust-gated: first use prompts or `--trust`; stored in `~/.local/state/ka/trust.json`) → env (`KA_MODEL`, `KA_MODE`) → flags. Unknown keys are hard errors with line numbers.

```toml
model = "ollama/qwen3.5:9b"
mode = "guarded"            # guarded | free | plan

[[rules]]                   # first match wins, before mode logic
tool = "bash"
pattern = "cargo *"         # glob on the call's primary argument
verdict = "allow"           # allow | ask | deny

[[hooks]]                   # exit-2 block contract
event = "pre_tool_use"      # or post_tool_use
tool = "write"              # optional filter
command = "guard.sh"        # {tool, arguments} JSON on stdin
```

Dialect overlays add any OpenAI-compatible provider: `ka run --dialects my.toml --model myhost/model "hi"`.

## Conventions ka reads automatically

`AGENTS.md` (root→cwd, `CLAUDE.md` compat) · `SKILL.md` skills in `.ka/` `.agents/` `.claude/` (name+description listed; body read on demand) · hooks · commands. `pathfinder` delegates read-only research to a nested voice and returns a dense summary.

## Safety

Clearances read/write/exec · guarded/free modes with session always-allow · bash decomposition (compound splitting, wrapper stripping, redirection-as-write) · **unbypassable hardstops** (root rm, fork bombs, fetch-and-execute, device writes — ask even in free; headless denies) · read ledger (edits refuse unread or changed files) · one-way secret redaction in every tool result · plan mode read-only except `.ka/plans/`.

## Development (cargo xtask)

Repo automation follows the [cargo-xtask](https://github.com/matklad/cargo-xtask) convention — `cargo xtask <task>`:

```sh
cargo xtask install   # stable: cargo install --locked → ~/.cargo/bin/ka
cargo xtask link      # dev: builds release + symlinks kad → repo target/release/ka
cargo xtask dev -- models          # rebuild + run dev binary with args
cargo xtask ci        # fmt --check + clippy -D warnings + tests (the CI gate)
cargo xtask size      # binary size vs the 10 MB contract
cargo xtask unlink    # remove the kad symlink
```

**Three wires**: `anthropic_messages`, `openai_chat` (plus every OpenAI-compatible endpoint), and `openai_responses` — the Responses API for reasoning models (o-series seeded: `openai/o3`, `openai/o4-mini`) with item-based history (`function_call`/`function_call_output`), flat tool definitions, `reasoning.effort`, and streamed reasoning summaries (visible as thinking).

**MCP client** (stdio, no new dependencies): configure `[[mcp]]` tables (name/command/args/env) and every server tool appears as a `<name>.<tool>` hand at exec-tier clearance — the gate, rules, and snapshots treat them like any external execution. `ka mcp` probes servers and lists tools. Handshake/tools-list/tools-call only; server noise is ignored; failures are per-server notes.

**File snapshots / undo**: `edit` and `write` park the target's current bytes under the data dir before every mutation (a failed snapshot refuses the change) and journal it per session — `/undo` (or `ka undo`) restores the most recent one, creation-undos delete. `/help` lists commands and keys; `ka --version` carries the git hash.

**Pricing honesty**: seeded dialect prices are placeholders and are flagged `priced = false` — surfaces (footer, `ka models`) never display costs from unverified rows. Flip the flag when real numbers land.

**Provider registry**: `openai anthropic google mistral groq cerebras deepseek qwen moonshot xai zhipu nvidia openrouter together fireworks ollama lmstudio llamacpp vllm` — any `vendor/model` selector works against these (no catalog row needed; context/pricing unknown until seeded).

Stable owns the name `ka`; dev is always `kad`. Isolate dev sessions with `KA_DATA_DIR=/tmp/ka-dev kad …` (shares config/rules/hooks, separates strands).

## Crates

`ka-protocol` (Command/Event wire contract) · `ka-agent` (engine: turn machine, tools + MCP hands, gate, digests, strands) · `ka-dialect` (catalog + 3 wires + discovery) · `ka-strand` (append-only JSONL sessions) · `ka-term` (ratatui TUI) · `ka-cli` (the binary).

## Footprint contract

Single binary ≤ 10 MB (currently **6.35 MB**) · cold start ≤ 50 ms · idle RSS ≤ 15 MB · zero steady-state network. 113 tests, `clippy -D warnings` clean, musl CI build.
