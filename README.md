# ka

A model-agnostic, very-low-footprint coding agent in Rust.

> ⚠️ **Under heavy development.** This project is experimental and moving fast — features, commands, config, and behavior may change or break at any time without notice. Expect rough edges; not ready for production use.

**Status: core complete (Phases 0–7 partial).** Design: [`research/ka/architecture.md`](research/ka/architecture.md) · Roadmap: [`research/ka/roadmap.md`](research/ka/roadmap.md) · Survey of 17 agents that informed it: [`research/`](research/README.md)

## Quickstart

```sh
cargo install ka-agent               # from crates.io — installs the `ka` binary
# from a clone:  cargo install --path crates/ka-agent
# dev:           cargo build --release -p ka-agent && alias ka=target/release/ka

cd your-project
ka --model ollama/qwen3.5:9b          # TUI: fresh chat
ka -c                                 # continue this terminal's last session
ka --session 3f9c2a81                 # resume a session by id prefix
ka sessions                           # list session ids for this directory
ka export --session 3f9c2a81           # export a specific session as markdown
ka mcp                                 # probe [[mcp]] servers, list tools
ka providers                          # provider registry + API-key env status
ka run "summarize the build error"    # headless NDJSON
ka run --review                       # read-only review of the working tree
```

Keys: `Enter` send / interject mid-turn · `+text` defer until turn ends · `Esc`/`Ctrl-C` abort · `↑/↓` history (seeded with a resumed session's earlier prompts) · long drafts wrap like a textarea — the box grows to six rows, no horizontal scroll · `/model` without arguments opens a unified picker over the whole catalog: configured providers first, a `─ not configured ─` divider, then the rest (filter as you type, key/env status; Enter on a keyed-but-unset model asks for the API key, then applies the pick; Enter on an unmatched filter sets it as a custom `vendor/model` selector). Interjections and `+deferrals` post a visible ack line while a turn runs. `PgUp`/`PgDn` scroll the transcript (title shows `↑N above`; `Esc` re-pins to the tail). Mouse: native is the default — plain drag selects and pastes with the terminal's own bindings, the wheel scrolls the chat (alternate-scroll sends it as `↑/↓`), and `↑/↓` or `Ctrl+P/N` recall history. `Ctrl+M` (or `[tui] mouse = "capture"`) flips to **capture**: the wheel scrolls at line granularity, the strip buttons and tool rows respond to clicks, ⇧drag still selects natively; the choice persists. `Esc` closes popups and path completion before it ever aborts a turn, and small actions (mouse toggle, mode/model changes, `/copy`, key saves, image staging) ack as toasts over the transcript tail instead of transcript rows. Rendered rows are cached per entry — streaming redraws only the live region. `/session` (alias `/resume`) opens an in-app session picker with type-to-filter, and resumed sessions replay their tool calls, results, and thinking; `/new` starts a fresh session; `/settings` edits model/mode/effort (persist with `s`) and shows every provider's API-key env status. On exit the `ka --session <tag>` resume command is appended to `$HISTFILE` (zsh-aware format; new shells see it on ↑). The transcript renders markdown (headers, lists, `code`, fenced blocks with syntax coloring behind a quiet rail). Tool activity is one compact row per call — `│ ▸ → tool · args …note` with a `✓`/`✗` verdict and duration right-aligned; consecutive calls group into one block, and clicking a row (capture mode) or `Ctrl+O` expands the call's full output in place (collapsed with `▸`/`▾`, spill files pointed at `/spills`). Multi-line thinking collapses to its first line plus a count — click a block (capture mode) or `Alt+T` to expand/collapse; per-block clicks and the global toggle compose. Every turn closes with a single verdict row — `✓ done · 2.1s · 1.2k in · $0.0042` (`◐ aborted`, `◑ stopped at output limit`, `✗ failed … /retry`); model, mode, ctx gauge, and cost live in the status bar's right side. NO_COLOR is honored. The bottom strip carries popup buttons — `todos` (Alt+O), `skills` (Ctrl+T), `info` (Alt+I), all working mid-turn — with `cwd:branch` on the right; `/help` teaches the full key table.

## Commands

| Surface | What it does |
|---|---|
| `ka` | TUI, fresh session |
| `ka -c` / `ka --session <id>` | continue newest (waypoint-aware per terminal) / resume by id prefix |
| `ka run [-c] [--model M] [--mode guarded\|free\|plan] [--trust] [--dialects f] "prompt"` | one headless turn, NDJSON events on stdout, exit 0/1/2 |
| `ka models [--no-discovery]` | catalog + local Ollama/LM Studio probes |
| `ka rewind [N]` | drop the last N exchanges of the newest strand |
| `ka export [-o out.md] [--html]` | strand as readable markdown, or a self-contained offline HTML page |
| `ka skill install <git-url\|path> [--force]` / `ka skill list` / `ka skill remove <name>` | user-scope skill lifecycle (`~/.config/ka/skills`; SKILL.md directories; git URL or local path) |
| `ka init` | starter AGENTS.md at the project root, from repo shape |
| `ka config {schema,print}` | resolved config / JSON schema |

TUI slash commands: `/model <sel>` `/mode [tier]` (picker: needs-approval | accept-edits | full-access | plan) `/plan <task>` `/build` `/review [base]` `/tasks` `/debug` `/rewind [N]` `/compact [focus]` `/quit` plus custom `/name` from `.ka/commands/*.md` (`$ARGUMENTS` substituted) — they appear in the `/` popup and `/help` like builtins; full list in `/help`. `/tasks` opens a picker over background tasks/jobs/DAP sessions — ⏎ pages the selected task's full result; `/debug` shows live debug sessions (breakpoints + console tail). Double-Esc on an empty input opens the rewind menu — pick a past message to rewind to, or `e` to edit & resend it. `!cmd` runs a shell command directly (no turn, no gate); its output shows in the transcript and rides the next prompt as context.

Mouse: the default is **native** — nothing is captured, so plain drag selects and pastes with the terminal's own bindings, the wheel scrolls the chat (alternate-scroll translates it to ↑/↓), ↑/↓ scroll and Ctrl+P/N recall history, and a whole-row drag selects chat text only (nothing shares its rows). The bottom strip carries popups: `todos` (Alt+O), `skills` (Ctrl+T), `info` (Alt+I) — shortcuts work everywhere, and in capture mode the buttons are clickable. `Ctrl+M` (or `[tui] mouse = "capture"`) switches to **capture**: SGR button reporting — the wheel scrolls, the strip buttons respond to clicks, ⇧drag selects (every major terminal bypasses capture on Shift); the choice persists. kitty implements no alternate-scroll: keep `capture` there for the wheel.

## Selectors & models

`vendor/model@effort` — e.g. `anthropic/claude-sonnet-5@high`, `ollama/qwen3.5:9b` (`@` because model ids may contain colons). Two wires built in (anthropic-messages, openai-chat — the latter covers every OpenAI-compatible endpoint incl. Ollama/vLLM/LM Studio/gateways); local endpoints auto-discovered.

## Config

Strict TOML, layered: defaults → `~/.config/ka/ka.toml` → `.ka/ka.toml` (trust-gated: first use prompts or `--trust`; stored in `~/.local/state/ka/trust.json`) → env (`KA_MODEL`, `KA_MODE`) → flags. Unknown keys are hard errors with line numbers. The **project root** — the nearest `.git` ancestor of the launch dir, stopping at `$HOME` and the filesystem root (else the launch dir itself) — hosts the whole project-scope `.ka/` layer (config, skills, rules, agents, commands, hooks) and everything ka generates into it (plans, staged memories, merge patches, always-allow saves), so sessions started in a subdirectory share one `.ka`.

```toml
model = "ollama/qwen3.5:9b"
mode = "accept_edits"       # guarded | accept_edits | free | plan

[[rules]]                   # first match wins, before mode logic
tool = "bash"
pattern = "cargo *"         # glob on the call's primary argument
verdict = "allow"           # allow | ask | deny

[[hooks]]                   # exit-2 block contract
event = "pre_tool_use"      # or post_tool_use
tool = "write"              # optional filter
command = "guard.sh"        # {tool, arguments} JSON on stdin
```

Hooks may also **steer**: on a clean exit, a JSON object on stdout —
`{"mode":"plan","note":"why"}` — switches the permission mode
(persisted to the strand, like `/mode`) and surfaces the note (≤200
chars) as a transcript row. That is the whole action language; anything
unparsable is ignored.

`ka --safe-mode` disables all customizations (AGENTS.md, MEMORY.md,
skills, agents, commands, hooks, MCP, LSP) keeping built-ins, config,
and auth — the troubleshooting floor. Guarded-mode exec asks append a
rough per-Mtok cost estimate when the active model is priced.

`ka serve` sessions can resume strands on disk: `POST /sessions` with
`{"resume":"latest"}` or `{"resume":"<strand id/prefix>"}` replays the
prior transcript over SSE; `ka acp` `session/load` accepts a strand id
prefix the same way.


## Sandbox & LSP

```toml
[sandbox]                  # "off" (default) or "fs"
mode = "fs"                # bash children: read everything, write only
                           # cwd, /tmp, XDG state/cache — enforced by
                           # bwrap, firejail, or in-kernel landlock

[lsp]                      # opt-in diagnostics feedback
enable = true
write_through = true       # act through the server (below), not just read
[lsp.commands]             # language → stdio server; only configured
rust = "rust-analyzer"     # languages spawn (hand-rolled client, no
python = "pyright-langserver --stdio"  # new deps)
```

`[lsp]` appends the server's latest diagnostics to successful `edit`/`write` results as informational context (never tool errors), capped at 20 lines. With LSP enabled, four navigation hands join the registry: `symbols` (workspace symbol search — the budget-safe repo map), `definition` / `references` (IDE-style navigation), and `diagnostics` (pull project-wide or per-file findings). Servers start eagerly so navigation works before the first edit. `ka doctor` reports whether configured server commands exist on PATH.

With `write_through = true`, three Write-tier hands join: `lsp_rename`, `lsp_actions`, and `lsp_format`. `lsp_rename` renames a symbol through `textDocument/rename` — every reference updates in one call; with `kind = "file"` it moves a file, applying each server's ripple edits first (`workspace/willRenameFiles`: imports, re-exports, barrels) and notifying `didRenameFiles` after. `lsp_actions` lists the server's code actions at a position (Read tier) and runs one — `mode = "run"`, pick by number or title — applying its `WorkspaceEdit`, executing its command, and honoring `workspace/applyEdit` requests the server sends mid-command. `lsp_format` formats a whole file through `textDocument/formatting`; its edits ride the same ledger path as every server-proposed change. Server-proposed edits ride the ka write path, never around it: files you have read refuse to change since the read (same ledger as `edit`), untouched ripple files are snapshotted (`/undo` covers them) and ledger-minted on apply, everything is capped (50 files, 64 edits per file, 256 KB of inserted text), stays inside the working directory, and never touches protected paths. Write-through is inert in `--safe-mode` like every customization tier.

## Debugging (DAP, opt-in)

```toml
[debug]                    # the probe: spawn a debug adapter, drive it
enable = true              # via one `debug` hand
[debug.adapters]           # name → stdio launch command; merged over
codelldb = "/opt/codelldb/adapter"  # the embedded seed (gdb, lldb-dap,
                           # debugpy, dlv, netcoredbg)
```

The `debug` hand launches or attaches a program, sets breakpoints, steps, and inspects the stack, variables, and expressions. Control-flow actions (start/break/clear/continue/next/step_in/step_out/pause/disconnect) run at Exec clearance; inspection (sessions/breaks/threads/stack/vars/eval/output) reads. Blocking actions wait — bounded — for the next stop and report where it landed; adapter console output lands in a bounded ring; `runInTerminal` is refused (the debug console is external). Sessions are capped and reaped when idle or dead. Inert in `--safe-mode`.

## Verify loop, notifications, memory inbox

```toml
[verify]                   # aider-style auto-lint / auto-test
test = "cargo test"        # runs once after a turn that edited files;
                           # a failure feeds the output back to the model
                           # for ONE automatic fix round
[[verify.lints]]
pattern = "*.rs"           # glob on the edited path (or basename)
command = "rustfmt --check {file}"

[tui]
bell = true                # ring the bell on turn completion + asks
notify = "notify-send ka \"turn done\""   # JSON {event, stop} on stdin
mouse = "native"          # default: plain drag selects, the wheel
                           # scrolls via alternate-scroll (xterm/VTE/
                           # alacritty/WezTerm/foot/Windows Terminal/
                           # iTerm2; kitty has no 1007 — set "capture"
                           # there; Ctrl+M toggles + persists)
```

The model can stage durable notes with the `remember` tool; they land in the project root's `.ka/memory/inbox.md` and nothing reaches `MEMORY.md` until you accept them in `/memory` (⏎ project · u user · d discard).

## Scoped rules & protected paths

`.ka/rules/*.md` (trust-gated; also `.agents/rules`, `.claude/rules`, `~/.config/ka/rules`) carry optional `paths:` frontmatter — a rule activates once a matching file has been read this session (TS conventions load only when TS files are touched). Rules on the web tools match hosts with domain semantics (`example.com` covers subdomains). Protected paths — `.git/hooks|config|modules`, `~/.ssh`, shell rc files, `.gitconfig`, ka's own config/credentials/trust store — always prompt, even in free mode; bash redirections into them hardstop.

## Agent frontmatter & background delegates

```
---
name: explorer
model: ollama/qwen3.5:9b   # per-agent model (or effort: low alone)
tools: read, grep, glob    # restrict the nested voice's hands
---
You explore code and report where things live.
```

`delegate {"background": true}` starts an agent detached and returns immediately; the `tasks` hand lists/reads/cancels background work, and `/tasks` in the TUI opens the picker (⏎ pages a task's full result).

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

**Markdown agents**: `.ka/agents/*.md` (and `.agents/`, `.claude/agents/`, `~/.config/ka/agents/`) define subagents — frontmatter (`name`, `description`, `max-steps`) plus a body that becomes the agent system prompt. The model delegates self-contained subtasks through the `delegate` tool (read-only nested voice, dense summary back — pathfinder generalized). `ka agents` lists them; `/agents` in the TUI.

**MCP client** (stdio, no new dependencies): configure `[[mcp]]` tables (name/command/args/env) and every server tool appears as a `<name>.<tool>` hand at exec-tier clearance — the gate, rules, and snapshots treat them like any external execution. `ka mcp` probes servers and lists tools. Handshake/tools-list/tools-call only; server noise is ignored; failures are per-server notes.

**File snapshots / undo**: `edit` and `write` park the target's current bytes under the data dir before every mutation (a failed snapshot refuses the change) and journal it per session — `/undo` (or `ka undo`) restores the most recent one, creation-undos delete. `/help` lists commands and keys; `ka --version` carries the git hash.

**Live meters**: the engine emits a context meter per model step, so the footer's token/ctx gauge moves while a turn runs (not just at the end), and the reasoning effort shows once set. `ka run --session <id>` resumes headless by id. Input keying: `⏎` send · `⇧⏎` newline (Ctrl+J fallback) · bracketed paste pastes multi-line as one draft.

**Pricing honesty**: curated dialect rows default to placeholder pricing flagged `priced = false` — surfaces (footer, `ka models`) never display costs from unverified rows; vendor-verified seed rows (e.g. `deepseek/deepseek-flash`) set `priced = true` with published prices. The generated models.dev overlay (`cargo xtask models-sync`) carries real published pricing, subscription plans included as unpriced `plan` rows.

**Model catalog**: 130+ providers seeded from [models.dev](https://models.dev) — including subscription tiers (`zai-coding-plan/glm-5.3`, `zhipuai-coding-plan/…`, `alibaba-coding-plan/…`) alongside pay-per-token endpoints, with context windows, pricing, and key status in the `/model` picker. Picking a model in `/model` persists it as the default for future conversations; `/settings` → `s` still saves the full panel.

**Provider registry**: curated vendors (`openai anthropic google … ollama lmstudio llamacpp vllm`) work selector-only; catalog-derived vendors from models.dev appear in `/settings` and `ka providers` (keyed first, capped, `ka providers` for the full list).

Stable owns the name `ka`; dev is always `kad`. Isolate dev sessions with `KA_DATA_DIR=/tmp/ka-dev kad …` (shares config/rules/hooks, separates strands).

## Crates

`ka-protocol` (Command/Event wire contract) · `ka-engine` (engine: turn machine, tools + MCP hands, gate, digests, strands) · `ka-dialect` (catalog + 3 wires + discovery) · `ka-strand` (append-only JSONL sessions) · `ka-term` (ratatui TUI) · `ka-agent` (the binary, published to crates.io).

## Footprint contract

Single binary ≤ 10 MB (currently **6.0 MB**, gated in CI on the musl artifact) · cold start ≤ 50 ms · idle RSS ≤ 15 MB · zero steady-state network. 530 tests (feature contracts included — CI fails if a documented behavior regresses), `clippy -D warnings` clean, musl CI build. Full-text session search ships behind the opt-in `index` cargo feature (`cargo install ka-agent --features index`).
