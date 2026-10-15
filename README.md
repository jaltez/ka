# ka

A model-agnostic, very-low-footprint coding agent in Rust.

> ⚠️ **Under heavy development.** This project is experimental and moving fast — features, commands, config, and behavior may change or break at any time without notice. Expect rough edges; not ready for production use.

**Status: core complete (Phases 0–7 partial).** Design: [`research/ka-harness/architecture.md`](research/ka-harness/architecture.md) · Roadmap: [`research/ka-harness/roadmap.md`](research/ka-harness/roadmap.md) · Survey of 17 agents that informed it: [`research/`](research/README.md)

## Quickstart

```sh
cargo install ka-harness               # from crates.io — installs the `ka` binary (≤ 0.2.11 shipped as `ka-agent`)
# from a clone:  cargo install --path crates/ka-harness
# dev:           cargo build --release -p ka-harness && alias ka=target/release/ka

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

Keys: `Enter` send / steer mid-turn · `⇧Enter` newline (kitty-protocol terminals; `Ctrl+J` the universal fallback) · `+text` or `Alt+Q` queue the draft until the turn ends (`Ctrl+Q` recalls the last queued item into the draft; slash commands queue too and run through their dispatcher on settle — `/mode free` queues fine, picker-openers like bare `/model` refuse with a reason) · `Esc`/`Ctrl-C` abort · `↑/↓`/`Ctrl+P/N` history (seeded with a resumed session's earlier prompts) · long drafts wrap like a textarea — the box grows to six rows, no horizontal scroll · `/model` without arguments opens a unified picker over the whole catalog: configured providers first, a `─ not configured ─` divider, then the rest (filter as you type, key/env status, and each model's reasoning levels as a `🧠 low/med/high` segment (abbreviated to fit) where the dialect exposes them; Enter on a keyed-but-unset model asks for the API key, then applies the pick; Enter on an unmatched filter sets it as a custom `vendor/model` selector). Steers and queued items post a visible ack line while a turn runs. `PgUp`/`PgDn` scroll the transcript (title shows `↑N above`; `Esc` re-pins to the tail), and the right edge carries a scrollbar rail — drag the thumb to scroll, click the track to page (capture mode). Mouse: capture by default — the strip buttons, tool cards and ▲▼ jump arrows respond to clicks, the wheel scrolls at line granularity, `↑/↓` recall history, and plain left-drag over the transcript selects those rows and copies them to the clipboard on release (OSC 52); ⇧drag still selects natively. `Ctrl+M` (or `[tui] mouse = "native"`) flips to **native**: plain drag selects and pastes with the terminal's own bindings, the wheel scrolls via alternate-scroll, and click-only affordances fall back to their keys (`Ctrl+O`, `Ctrl+↑/↓`); the choice persists. `Esc` closes popups and path completion before it ever aborts a turn, and small actions (mouse toggle, mode/model/thinking changes, `/copy`, key saves, image staging) ack as toasts over the transcript tail instead of transcript rows. Rendered rows are cached per entry — streaming redraws only the live region. `/session` (alias `/resume`) opens an in-app session picker with type-to-filter, and resumed sessions replay their tool calls, results, and thinking; `/new` starts a fresh session; `/settings` edits model/mode/effort (persist with `s`) and shows every provider's API-key env status; `/thinking` opens the reasoning-level picker (Enter applies and persists; `/thinking <level>` is session-scoped). On exit the `ka --session <tag>` resume command is appended to `$HISTFILE` (zsh-aware format; new shells see it on ↑) and written to `<data dir>/last-resume` — with `eval "$(ka shell-integration)"` in `.zshrc`/`.bashrc`, a tiny `ka()` wrapper feeds that command into the live shell's own history after every run, so exiting and pressing **↑ then Enter** reopens the exact session you left. The transcript renders markdown (headers, lists, `code`, fenced blocks with syntax coloring behind a quiet rail). Tool activity is one boxed card per call — `╭─ 🐚 bash ▸ · cmd ─╌╌ 1.2s ✓ ─╮`, state-tinted (violet while running, sage on success, coral on failure, with a failed bash call's `⏎ exit N` on the border), with the thinking-block fold vocabulary: `▸` collapsed / `▾` expanded on any call that has output to reveal; consecutive calls group into one block separated by a quiet `╌` rule, and clicking a row (capture mode) or `Ctrl+O` expands the call's full output in place (spill files pointed at `/spills`). Thinking renders as a borderless pink band carrying a single `🧠` — a `Thinking ▾ · N lines` head plus at most three rows inside a quiet inner margin (the streaming tail while a turn runs, the first three once cached), folding to the full text on click/`Alt+T`; single-line thoughts carry no chevron so a foldable block is recognizable at a glance; `Alt+T` or a click (capture mode) toggles any block open/closed, and a blank spacer row keeps the box off the input. Every turn closes with a single verdict row — `✓ done · 2.1s · 1.2k in · $0.0042` (`◐ aborted`, `◑ stopped at output limit`, `✗ failed … /retry`); model, mode, thinking level, ctx gauge, and cost live in the status bar's right side (the `🧠 ○ off/…` segment shows whenever the model exposes reasoning control). NO_COLOR is honored. The bottom strip carries popup buttons — `📋 todos` (Alt+O), `⚡ skills` (Ctrl+T), `💡 info` (Alt+I), all working mid-turn — with `cwd 🌿 branch` on the right; `/help` teaches the full key table.

## Commands

| Surface | What it does |
|---|---|
| `ka` | TUI, fresh session |
| `ka -c` / `ka --session <id\|name>` | continue newest (waypoint-aware per terminal) / resume by id prefix or memorable name (`/name <word>` sets one; exact, then unique prefix) |
| `ka run [-c] [--model M] [--mode guarded\|free\|plan] [--trust] [--dialects f] "prompt"` | one headless turn, NDJSON events on stdout, exit 0/1/2 |
| `ka models [--no-discovery]` | catalog + local Ollama/LM Studio probes |
| `ka rewind [N]` | drop the last N exchanges of the newest strand |
| `ka export [-o out.md] [--html]` | strand as readable markdown, or a self-contained offline HTML page |
| `ka -i "<prompt>"` | run one prompt, then open the TUI resumed on that strand (works with `-c`/`--session`) |
| `ka skill install <git-url\|path> [--force]` / `ka skill list` / `ka skill remove <name>` | user-scope skill lifecycle (`~/.config/ka/skills`; SKILL.md directories; git URL or local path) |
| `ka install <skill\|agent\|command\|rule> <git-url\|path> [--force]` · `ka install list [kind]` · `ka install remove <kind> <name>` | generalized user-scope customization lifecycle (`ka skill …` stays as the skill alias; no registry, no manifests — installing is the trust act) |
| `ka import claude <settings.json> [--project\|--user] [--dry-run]` | one-shot conversion of Claude Code permission rules into ka `[[rules]]` (printed conversion + skips; strict-validated write; deny → ask → allow order preserves claude precedence) |
| `ka attach [id] [--addr 127.0.0.1:8417] [--token T]` | observe a live `ka serve` session read-only (SSE; `busy · N attached` chips; no write path — prompting stays on the server API) |
| `ka completions <bash\|zsh\|fish>` | shell completions, generated from the live CLI parser |
| `ka init` | starter AGENTS.md at the project root, from repo shape |
| `ka config {schema,print}` | resolved config / JSON schema |
| `ka update [stable\|edge] [--check]` | self-update from the newest GitHub release: ed25519-signed musl tarball (CI sha256 fallback for cargo-installed builds), atomic binary swap; `--check` only reports |

**Updating**: the TUI runs one availability check on startup — rate-limited to a single network hit per 24 h (cached in `~/.local/state/ka/update/`), never installs (`ka update` stays the only install path), and stays quiet offline. When a newer release is out, a `⤴ vX.Y.Z` badge appears in the status bar's right side for the session, plus one transcript line pointing at `ka update`. `[update]` config: `repo = "owner/name"` (default `jaltez/ka-harness`) and `check = "startup"|"off"` (default `startup`).

TUI slash commands: `/model <sel>` `/mode [tier]` (picker: guarded | accept edits | full access | plan) `/thinking [level]` (picker: off | low | medium | high | max — model-dependent; Enter persists, the named form is session-scoped) `/plan <task>` `/build` `/review [base]` `/tasks` `/debug` `/rewind [N]` `/compact [focus]` `/name <word>` (memorable session alias — `ka --session <word>` resumes on it, and `ka sessions` shows `name · title`) `/transfer [focus]` (digest the context into a fresh handoff session — the summary is the new session's entire carried memory, parent link set) `/quit` plus custom commands from `.ka/commands/*.md` (`$ARGUMENTS` substituted) — they appear in the `/` popup (Tab completes the marked suggestion, ↑↓ move the mark) and `/help` like builtins; full list in `/help`. `/skill:<name> [args]` invokes a discovered skill explicitly for one turn: the SKILL.md body rides that turn's system prompt (the args are the prompt), unknown or disabled skills refuse with a note naming the toggle, and `/skill:` completes from the live inventory in the `/` popup — typing `/sk` offers the `/skill` starter, the names appear once you type the colon; bare `/skill` lists the discovered names. Custom commands carry frontmatter: `description`, `argument-hint`, plus `allowed-tools: read, grep` (restricts that turn's toolset) and `model: <selector>` (runs the turn on that model once, never persisting the switch). `/tasks` opens a picker over background tasks/jobs/DAP sessions — ⏎ pages the selected task's full result; `/debug` shows live debug sessions (breakpoints + console tail). Double-Esc on an empty input opens the rewind menu — pick a past message to rewind to, or `e` to edit & resend it. `!cmd` runs a shell command directly (no turn, no gate); its output shows in the transcript and rides the next prompt as context.

Mouse: the default is **capture** — SGR button reporting makes every click affordance work out of the box: tool rows expand in place — each call renders as a state-tinted card with a `▸`/`▾` fold marker (violet while running, sage on success, coral on failure, with a failed bash call's `⏎ exit N` on the border) — the ▲▼ title arrows and strip popups respond to clicks, the wheel scrolls at line granularity, the scrollbar rail on the right edge drags (thumb) and pages (track), and `↑/↓`/`Ctrl+P/N` recall history; with the pointer resting on anything clickable — cards, foldable thinking bands, strip buttons, ▲▼ arrows, the ✕ chip, the rail thumb — a blue hover band / gold glow previews what a click would touch. Plain left-drag over the transcript is ka's own selection: it highlights the picked rows and copies them to your system clipboard on release (OSC 52; a toast confirms), so selecting never needs a modifier. Click outside a popup (or its ✕) closes it — except permission asks, which need an explicit key. ⇧drag still selects natively in your terminal. `Ctrl+M` (or `[tui] mouse = "native"`) switches to **native**: nothing is captured, so plain drag selects and pastes with the terminal's own bindings and the wheel scrolls via alternate-scroll (which translates it to ↑/↓) — click affordances are keyboard-only there (`Ctrl+O`, `Ctrl+↑/↓`); while a popup or modal is open tracking turns on by itself so its ✕/click-outside still work; the choice persists.

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
event = "pre_tool_use"      # or post_tool_use · stop · turn_end ·
tool = "write"              # session_start · session_end ·
command = "guard.sh"        # user_prompt_submit · pre_compact
                            # ({tool, arguments, cwd} JSON on stdin;
                            # lifecycle events carry their own payload)

[guards]
spend_usd = 2.0             # ask before the session crosses $2
context_pct = 90            # ask when the context meter passes 90%
auto_review = true          # fast-role reviewer pre-screens exec asks:
                            # auto-allows only the unmistakably safe,
                            # never denies, always leaves a note. Off by
                            # default; inert in --safe-mode

[pokes]                     # persistence: a turn that settles with
max = 3                     # pending todos continues itself — bounded
                            # rounds, 0 = off (see "Todo confidence")

[[rules]]                   # the doom-loop rule domain: what happens
tool = "loop"               # when a call repeats with identical args
verdict = "ask"             # (allow = never trip · ask at 3 · deny at 3;
                            # no rule = the shipped auto-error at 4)
```

Hooks may also **steer**: on a clean exit, a JSON object on stdout —
`{"mode":"plan","note":"why"}` — switches the permission mode
(persisted to the strand, like `/mode`) and surfaces the note (≤200
chars) as a transcript row. A `pre_tool_use` hook can also patch the
upcoming call — `{"updated_input":{"command":"…"}}` shallow-merges into
the tool arguments (formatters, secret-scrubbers, redirectors) — and
pre-approve it — `{"decision":"allow"}` skips the permission ask, though
never for hardstops or protected paths; every pre-approve lands as a
visible transcript row. On `post_tool_use` the same object may carry
`{"feedback":"…"}` — the string (≤2 KB) is appended to that call's tool
result for the model to read: formatters, linters, and policy scripts
get a generalized [verify]-lint channel over any tool. That is the
whole action language; anything unparsable is ignored. `turn_end` is the claude-compat spelling of
`stop` (same point, same payload, its own event name); `session_start`
/ `session_end` fire once per strand attach/close, `user_prompt_submit`
before each turn, `pre_compact` before every digest.

`ka --safe-mode` disables all customizations (AGENTS.md, MEMORY.md,
skills, agents, commands, hooks, MCP, LSP) keeping built-ins, config,
and auth — the troubleshooting floor. Guarded-mode exec asks append a
rough per-Mtok cost estimate when the active model is priced.

`ka serve` sessions can resume strands on disk: `POST /sessions` with
`{"resume":"latest"}` or `{"resume":"<strand id/prefix>"}` replays the
prior transcript over SSE; `ka acp` `session/load` accepts a strand id
prefix the same way. Any number of concurrent observers may stream a
session (`GET /sessions/{id}/events`): each gets the full ring-buffered
history then live events, `Last-Event-ID` reconnects mid-session, and
presence changes broadcast as `{"type":"presence","busy":…,"attached":…}`
events — `ka attach` renders exactly that (`busy · N attached` in the
footer). `GET /sessions` lists every session with presence + title,
`GET /sessions/{id}` returns one. The write path stays single-owner:
observers have no prompt route; `POST /sessions/{id}/prompt` remains the
only writer API.

## Runtime feature toggles

One grammar decides what the model can use — set at startup (flags,
`[features]`) or flipped mid-session (`/features`, `/sandbox`). Every
change is recorded on the strand and restored on resume, exactly like
model and mode:

```
agents | skills | mcp | hooks | web | lsp | debug          # whole features
mcp:<server> · skill:<name> · tool:<hand> · agent:<name>   # single items
```

- Disabling is fail-closed: the capability vanishes from the model's
  tool list **and** a stray call is rejected naming the toggle — it
  cannot be reached even by guessing the name.
- Enabling restores it. An MCP server disabled at startup spawns on the
  spot; so do the LSP and debug tiers when configured.
- Toggles land between turns. Sent mid-turn, the hide applies to the
  next model round-trip inside that turn and the side effects (spawn,
  strand record, inventory) settle with it.
- `--safe-mode` outranks every toggle — nothing it disables can be
  re-enabled until restart.

```sh
ka --disable agents,mcp:github   # no subagents, one server off
ka --disable tool:bash           # a session that cannot run commands
ka --enable mcp:github           # re-enable over a [features] disable
```

```toml
[features]
disable = ["agents", "skill:pdf", "tool:bash"]
```

In the TUI, `/features` opens the panel (↑↓ choose · ⏎ toggle · `s`
saves the current set as your user-layer default); `/features off
mcp:github` toggles directly. `/sandbox off|fs` switches the bash
sandbox live — the `fs` policy is recomputed from `[sandbox]
allow_write`, so unpersisted session grants drop on off→fs. The status
bar shows `⊘ N` while N specs are disabled. Precedence: flags → resumed
strand → `[features]` → defaults. The `hooks` toggle silences config
`[[hooks]]` and `.ka/hooks` alike; disabling a tier never weakens the
permission tiers — it only removes capabilities.

## Sandbox & LSP

```toml
[sandbox]                  # "off" (default) or "fs"
mode = "fs"                # bash children: read everything, write only
                           # cwd, /tmp, XDG state/cache — enforced by
                           # bwrap, firejail, or in-kernel landlock
allow_write = ["/opt/cache"]   # extra writable dirs; an "always"
                           # expansion grant (below) appends here
```

**Sandbox expansion**: when a sandboxed command deterministically needs
more than the policy allows — a redirection outside the write allowlist,
a network-touching program under a network-denying backend, env
assignments an env-clearing backend would strip — ka computes the exact
missing grants *before* running and asks once: *allow* (this run),
*always* (write paths persist to `[sandbox] allow_write`; network/env
are session-scoped), or *deny* (the sandbox stays unchanged; the command
fails on its own). Grants come from command analysis, never from failure
output, and writes beneath broad dirs (`/`, `$HOME`, `/etc`…) are never
offered — nothing expands without an explicit ask.
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
mouse = "capture"         # default: clickable tool cards, ▲▼ arrows
                           # and strip buttons, line-granularity wheel,
                           # ↑/↓ history; plain drag over the transcript
                           # selects rows and copies them to the
                           # clipboard (OSC 52). Set "native" for plain
                           # drag select/paste with no capture (Ctrl+M
                           # toggles + persists)
```

The model can stage durable notes with the `remember` tool; they land in the project root's `.ka/memory/inbox.md` and nothing reaches `MEMORY.md` until you accept them in `/memory` (⏎ project · u user · d discard).


**Todo confidence** (confidence stepping): the `todo` tool accepts an optional per-item `confidence` 0–100 — rated when an item is assigned, re-rated when it is marked done. An item flipping pending→done with a low→high jump (≤40 → ≥90) is a claim, not a measurement: the tool result carries a re-check advisory and the transcript gets a matching note. Unrated items never trip it; the wire omits `confidence` when absent (backward-compatible).

**Auto-poke** (`[pokes] max`, default 3): a turn that settles while the model's own todo list still has pending items is an early exit, not a finish — the harness continues the work itself with a bounded continuation round (the prompt names the pending items and asks for verification of confidence-spike claims; it rides the strand as a normal user message and is visible on every surface). Pokes stop on non-Done stops (aborts and errors — wire retries have already run for transient failures), in plan mode, when the pending set stops moving (no progress), and at `max` (`0` disables). The same loop drives headless `ka run`, so one prompt keeps iterating until the todo list is actually clear.

**Hill-climbability** (plan mode): when a plan-mode turn finishes, the `fast` role rates the goal 0–100 for hill-climbability — is there a measurable, iterable objective to optimize against? Below 70 the harness pushes back with a note asking for a verifiable objective (a test to pass, a benchmark to move) and suggests wiring `[verify]`. One cheap call per plan turn; no fast role configured or a failed call stays silent, non-plan modes never rate.

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

When `tasks merge` lands an isolated agent's branch, any merged file the main session had **read** is named right in the merge result as a read-file conflict ("re-read before editing") — the ledger would otherwise refuse the next stale edit cold. Files the branch deleted are skipped (nothing to re-read), and the probe matches worktree-root-relative paths, so it works from subdirectory launches. Cross-agent awareness without a coordinator: the merge result is the notification channel.

## Conventions ka reads automatically

`AGENTS.md` (root→cwd, `CLAUDE.md` compat) · `SKILL.md` skills in `.ka/` `.agents/` `.claude/` (name+description listed; body read on demand; `/skill:<name>` injects it for one turn) · hooks · commands. `pathfinder` delegates read-only research to a nested voice and returns a dense summary.

## Safety

Clearances read/write/exec · guarded/free modes with session always-allow · bash decomposition (compound splitting, wrapper stripping, redirection-as-write) · **unbypassable hardstops** (root rm, fork bombs, fetch-and-execute, device writes — ask even in free; headless denies) · read ledger (edits refuse unread or changed files) · one-way secret redaction in every tool result · plan mode read-only except `.ka/plans/`.

**Provider-native web search** (`server_search` dialect flag, opt-in per model): with `[dialects."anthropic/claude-sonnet-5".flags] server_search = true`, the wire injects the provider's own server-side search tool (Anthropic's `web_search_20250305`, OpenAI Responses' `web_search`) and drops ka's client-side `web_search` hand for that model — no `[[search]]` API key needed. Injection honors the `web` feature toggle (disable web → no native search — fail-closed like every toggle); domain rules apply to the client hand only, so restricted setups should keep `[[search]]`. On the anthropic wire the streamed query + sources surface as text (`[web_search: q]`, `Sources: title — url`), failed searches surface `[web_search failed: code]`, a server-side `pause_turn` maps to a visible length stop, and a search deferred alongside client tool calls is named as not-run; on the responses wire the query surfaces (citations ride the model's own text). The `openai_chat` wire refuses the flag instructively at request time (use `[[search]]` there). `web_fetch` is unaffected.

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

**MCP client** (stdio, no new dependencies): configure `[[mcp]]` tables (name/command/args/env) and every server tool appears as a `<name>.<tool>` hand at exec-tier clearance — the gate, rules, and snapshots treat them like any external execution. `ka mcp` probes servers and lists tools. Handshake/tools-list/tools-call only; server noise is ignored; failures are per-server notes. **Tool cache**: after a server's first successful handshake its tool list is remembered under `$XDG_STATE_HOME/ka/mcp-tools.json`, keyed by a hash of the server's config; the next startup registers the tools **from cache instantly** (a slow or dead server never blocks startup, and the tool list stays byte-stable across runs — provider prompt-cache stability) while a background handshake verifies: unchanged = silent, changed = hands swapped + note, dead = the watchdog retries and the cached hands answer "disconnected" until it lands. Any config change invalidates the entry; a corrupt file is a plain miss.

**File snapshots / undo**: `edit` and `write` park the target's current bytes under the data dir before every mutation (a failed snapshot refuses the change) and journal it per session — `/undo` (or `ka undo`) restores the most recent one, creation-undos delete. `/help` lists commands and keys; `ka --version` carries the git hash.

**Live meters**: the engine emits a context meter per model step, so the footer's token/ctx gauge moves while a turn runs (not just at the end), and the reasoning effort always shows beside model/mode (for models that expose reasoning control). `ka run --session <id>` resumes headless by id. Input keying: `⏎` send · `⇧⏎` newline (Ctrl+J fallback) · bracketed paste pastes multi-line as one draft.

**Pricing honesty**: curated dialect rows default to placeholder pricing flagged `priced = false` — surfaces (footer, `ka models`) never display costs from unverified rows; vendor-verified seed rows (e.g. `deepseek/deepseek-flash`) set `priced = true` with published prices. The generated models.dev overlay (`cargo xtask models-sync`) carries real published pricing, subscription plans included as unpriced `plan` rows.

**Model catalog**: 130+ providers seeded from [models.dev](https://models.dev) — including subscription tiers (`zai-coding-plan/glm-5.3`, `zhipuai-coding-plan/…`, `alibaba-coding-plan/…`) alongside pay-per-token endpoints, with context windows, pricing, and key status in the `/model` picker. Picking a model in `/model` persists it as the default for future conversations; `/settings` → `s` still saves the full panel.

**Provider registry**: curated vendors (`openai anthropic google … ollama lmstudio llamacpp vllm`) work selector-only; catalog-derived vendors from models.dev appear in `/settings` and `ka providers` (keyed first, capped, `ka providers` for the full list).

Stable owns the name `ka`; dev is always `kad`. Isolate dev sessions with `KA_DATA_DIR=/tmp/ka-dev kad …` (shares config/rules/hooks, separates strands).

## Ka and the future Ka Agent

`ka` (this repo) is the engine and the terminal experience — one ≤10 MB binary, no daemons. **Ka Agent** is a planned, separate product: an optional persistent agent (long-term memory, messaging channels such as Telegram, web UI, plugins, automations) that will build on ka's stable surfaces — `ka serve`, `ka run --print ndjson`, `ka acp`, the strand format, the crate APIs — or run the `ka` binary as a subprocess. Never inside it, never as its dependency; CI gates the binary's dependency graph so it stays that way. The naming stays Egyptian: ka is the vital essence; **Ba** (the mobile personality) and **Akh** (the effective spirit) are reserved as the product's inner layer names. Decision record: [`research/ka-harness/agent-boundary.md`](research/ka-harness/agent-boundary.md).

## Crates

`ka-protocol` (Command/Event wire contract) · `ka-engine` (engine: turn machine, tools + MCP hands, gate, digests, strands) · `ka-dialect` (catalog + 3 wires + discovery) · `ka-strand` (append-only JSONL sessions) · `ka-term` (ratatui TUI) · `ka-harness` (the binary, published to crates.io).

## Footprint contract
Single binary ≤ 10 MB (currently **6.25 MB** musl, gated in CI on the stripped musl artifact) · cold start ≤ 50 ms · idle RSS ≤ 15 MB · no steady-state network (one rate-limited update check at startup; `[update] check = "off"` disables it). 779 tests (feature contracts included — CI fails if a README-documented behavior regresses), `clippy -D warnings` clean, musl CI build. Full-text session search ships behind the opt-in `index` cargo feature (`cargo install ka-harness --features index`).

**System-prompt budget**: the harness-authored core of the system prompt (identity + todo discipline + the plan-mode block) is contract-capped at **2 KiB ≈ 500 tokens** (`system_prompt_core_stays_within_budget`); project instructions, skills, memory, and digest summaries are session data layered outside it. The core cannot grow without the test failing.
