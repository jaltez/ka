# Ka — Original Architecture Design

v0.1 · 2026-08-23 · grounded in `research/` survey; concepts reinvented and renamed (mapping table at the end). General patterns (event queues, append-only logs, SSE clients) are commons; the named mechanisms below are Ka's own.

## 0. Principles
1. **Engine purity**: the engine crate knows nothing about terminals, TOML files, or HTTP servers' vendor quirks. It consumes Commands, emits Events, calls a `Speaker` (provider) trait, and runs `Hands` (tools) behind a `Clearance` gate.
2. **Everything heavy is optional**: every feature that adds >100KB or a dependency beyond the core set lives behind a cargo feature or a separate crate, or doesn't exist.
3. **Data over code**: model quirks, permissions, tool catalogs, agent definitions — files and tables, never branches in the engine.
4. **One process**: children are only user shells (and optional stdio MCP servers / git). No daemons, no sidecars.
5. **Fail toward boring**: strict configs, exact-match edits, append-only logs, fail-closed sandboxes.

**Footprint contract (CI-enforced):** static musl binary ≤ 10MB stripped · cold start ≤ 50ms · idle RSS ≤ 15MB · zero steady-state network.

## 1. Vocabulary (Ka's own names)
| Term | Meaning | Lineage (reinvented from) |
|---|---|---|
| **strand** | one session = one append-only JSONL file; a tree of records with a single live tip | omp/pi session trees, codex rollouts |
| **record** | one line in a strand (message, change, digest, boundary, custom) | session entries |
| **offshoot** | moving the tip to an earlier record (branch in place) | /tree branching |
| **split** | copying a strand's prefix into a new strand file | fork |
| **digest** | a compaction record: summary + kept-tail boundary | compaction entries |
| **interjection** | user input delivered mid-turn, between tool batches | steering |
| **deferral** | user input queued for after the turn settles | follow-up |
| **ledger** | per-strand map of file→(mtime,hash) at last read; edits refuse stale files | read-tracking / filetracker |
| **spill** | oversized tool output parked on disk, referenced `spill://<id>` | artifact spill |
| **dialect** | a named per-model flag profile in `dialects.toml` shaping wire requests | compat flags |
| **Speaker** | the provider trait ("the model speaks through it") | Provider trait |
| **Hands** | the tool registry trait ("the engine acts through them") | tool interfaces |
| **clearance** | tool tier: `read` / `write` / `exec` | approval tiers |
| **hardstops** | unbypassable catastrophic-command list; prompts even in free mode | droid blocklist |
| **guarded / free** | the two permission modes | ask/yolo |
| **waypoints** | tiny per-terminal tokens so `ka -c` continues the right strand per pane | terminal breadcrumbs |
| **pathfinder** | the read-only subagent; summary-only return | scout/explore agents |
| **settling** | post-turn maintenance window (digest checks, ledger sync) | turn_end maintenance |

## 2. Workspace layout
```
ka-agent/       engine: turn machine, queues, Hands/Clearance, digest, ledger   (no I/O deps beyond std+serde)
ka-protocol/   Command + Event enums (shared by TUI, headless, future servers)
ka-dialect/    Speaker trait; anthropic-messages + openai-chat wires; dialects.toml loader; selectors
ka-strand/     strand store: reader, appender, tip/index, split, waypoints
ka-term/       ratatui surface (optional feature from CLI's view)
ka-cli/        the binary: `ka` (TUI), `ka run` (headless NDJSON)
future crates:  ka-index (SQLite/FTS), ka-sandbox (landlock/seatbelt), ka-mcp (rmcp)
```
Dependency direction is one-way: `cli → term/protocol/dialect/strand → engine`. The engine never imports a surface.

## 3. The protocol (`ka-protocol`)
Two queues between surface and engine, both serde enums, both NDJSON-shaped so `ka run` literally serializes the Event side:

```rust
pub enum Command {          // surface → engine
    Prompt { text: String, attachments: Vec<Attachment> },
    Interject { text: String },              // mid-turn steering
    Defer { text: String },                  // queued for settle
    Abort,
    SetModel { selector: String },
    SetEffort { level: Effort },
    SetMode { mode: Mode },                  // guarded | free
    AlwaysAllow { rule: RuleId },
    Resume { strand: StrandId, tip: Option<RecordId> },
    Compact { focus: Option<String> },
    Answer { question: AskId, choice: ... }, // reply to Ask
}

pub enum Event {            // engine → surface (non_exhaustive)
    TurnStarted { context: ContextMeter },
    Delta { kind: DeltaKind },               // Text | Thought | Call{tool, id}
    CallStarted/CallFinished { tool, id, ok, spent },
    Ask { id, questions },                   // surfaces render; headless prints JSON
    TurnFinished { stop: Stop, usage: Usage, cost: f64, cache_hit: f32 },
    DigestStarted/DigestFinished { kept: RecordId },
    ModeChanged, ModelChanged, Error{class, retryable},
}
```

## 4. Engine turn machine
States: `Receiving → Speaking → Acting → Settling`.
- **Speaking**: stream from the active Speaker; deltas forwarded as received; interjections checked between deltas; Esc → Abort keeps partial transcript as a record.
- **Acting**: calls whose clearance is granted run in parallel (`FuturesUnordered`, results re-ordered to call order); exec-tier waits for `AlwaysAllow`/Ask when guarded; per-tool timeouts; output through the caps+spill pipe.
- **Settling**: ledger sync, digest threshold check (maybe trigger), deferral queue drain, strand flush.
A strand is **the** state: restarting the process and replaying records to the tip reproduces everything (models, effort, mode, pending defer). No other persistence exists in core.

## 5. Dialect layer (`ka-dialect`)
```toml
# dialects.toml (excerpt) — data, not code
[dialect."anthropic/claude-sonnet-5"]
context = 200000
efforts = ["low","medium","high","max"]
input   = ["text","image"]
cache   = "control"            # control | key | none
ratio  = 3.6                    # chars-per-token estimate for meters/digest triggers
[dialect."anthropic/claude-sonnet-5.flags"]
reasoning_field   = "thinking"
developer_role    = true
requires_tool_result_name = false

[dialect."ollama/qwen3:32b"]
wire = "openai-chat"
discovery = "ollama"
first_byte_timeout = 0         # unbounded prefill
[dialect."ollama/qwen3:32b.flags"]
replay_reasoning  = true       # byte-exact <think> re-emit for KV reuse
tool_choice       = "pin-or-auto"
```
- Speaker implementations: `AnthropicMessages`, `OpenAIChat`. Both vendored (reqwest+rustls+hand-rolled SSE), no SDK crates. Speakers consume a **TokenSource** trait: keys/env/`!cmd` now; a future `ka-passport` crate may plug subscription OAuth in without touching core.
- Selector grammar: `vendor/model:effort`, `:effort` inherits vendor, roles `@default` `@fast` (+`@plan` later) resolved through `[roles]` in config.
- Local discovery probes convert found endpoints into ephemeral dialect rows; explicit config wins.
- The partial-JSON tool-arg accumulator lives here once, shared by both wires, with a repairing fallback parser.

## 6. Hands (tools) & clearance
Core seven, each a struct implementing `Hand`:
| tool | clearance | notes |
|---|---|---|
| `read` | read | line/range/byte selectors; caps; mints ledger entries |
| `edit` | write | exact old→new, must be unique; ledger check (changed-since-read = refuse) |
| `write` | write | refuses overwrite-without-read; shebang ⇒ +x |
| `bash` | exec | timeout, caps, spill, process-tree kill, auto-background past 60s |
| `glob` | read | gitignore-aware, bounded results |
| `grep` | read | Rust regex only; errors teach rewrites (no backrefs) |
| `ask` | — | surfaces render; headless emits Ask event and waits |
Plus `pathfinder` (Phase 6) as a tool that spawns an engine instance on an isolated strand with a read-only Hands set.
Every Hand carries `annotations { read_only, idempotent }` consumed by permissions and output filtering.

## 7. Safety
- Modes: `guarded` (exec + non-allowlisted write → Ask; "always allow" persists per session as a Rule) and `free` (auto; hardstops still Ask).
- **Bash decomposition** (pure Rust, shlex-based): split compounds on `&& ; || | &`; strip wrappers `timeout nice nohup stdbuf env` and bare `xargs`; redirections count as writes; readonly allowlist (ls, cat, git status/diff/log…) auto-passes only when no compound operators.
- **Hardstops** (flat, unbypassable list, checked on the decomposed program set): recursive root/home deletion, fork bombs, fetch-and-execute pipes, writes to /etc, `/` or top-level-dir rm, `dd` to devices. Resolution follows PATH aliases → resolves the actual executable before matching.
- Phase 5+: rules as data (`[[rule]] tool="bash" pattern="git push*" verdict="ask"`), project trust gate, optional `ka-sandbox` crate (landlock+seccomp / seatbelt), one-way secret redaction.

## 8. Strand store (`ka-strand`)
```
~/.local/share/ka/strands/<encoded-cwd>/<ts>_<id>.jsonl
spills: ~/.local/share/ka/spills/<id>            (content-addressed)
waypoints: $TMPDIR/ka/<tty-hash>                 (strand id + tip)
```
Record kinds: `header` (first line: id, ts, cwd, versions, **repo snapshot: branch + dirty list**) · `message` · `change` (model/effort/mode, with role) · `digest` (summary, kept-from record id) · `boundary` (clear) · `custom` (namespaced `x.<vendor>.<name>`; engine reserves `x.ka.*`).
- Appends only; offshoot = tip move; split = new file, copy prefix, link `parent`.
- Resume: stream-parse, replay changes to restore settings, synthesize `aborted` stop on dangling turns.
- No database in core. `ka-index` (later) is a rebuildable cache, never a source of truth.

## 9. Context survival
1. Caps + spill at every tool result (default 32KB tail / 8KB head, line caps) — before any model involvement.
2. **Prune**: blank tool results older than the protected window (40k) when savings ≥ 20k; elision markers.
3. **Digest**: same-model summary (avoids dialect mismatch), kept tail 20k adaptive, never cut between a call and its result, split-turn double summary; triggers at reserve max(16k, 15%); overflow → drop failing turn → digest → retry once.
4. Later: zero-LLM elision (old results → `spill://` pointers), context promotion, speculative digest.

## 10. Surfaces
- **TUI** (`ka-term`, ratatui): transcript pane (streaming deltas, collapsed tool rows), editor with history/interjection keys (Enter=interject while speaking, queued deferral via prefix `+`), footer meters (context %, cost, **cache-hit %**), Ask dialogs, model/effort quick-cycle keys.
- **Headless** (`ka run`): stdin/prompt arg; prints the Event stream as NDJSON (Phase 6 option: Claude-Code-compatible event shapes for interop); `--schema` structured final answer (Phase 7); exit codes: 0 ok · 1 error · 2 aborted · 42 input-rejected.
- **Updater** (`ka update`): fetches GitHub release artifact, verifies an ed25519 minisign-style signature before swapping the binary, opt-in channel tag (stable/edge); never runs in background, no auto-update. Supply chain: pinned deps, no build scripts in dependency tree where avoidable (cargo auditable).

## 11. Config
```
~/.config/ka/ka.toml      # user
.ka/ka.toml               # project (trusted after gate)
.ka/skills/…, .ka/agents/ # Phase 6 ecosystem dirs (reads .claude/.agents skills too)
```
Strict TOML: unknown keys = error with position; `ka config schema` emits JSON schema for editors. Secrets only via env names or `!cmd` indirection.

## 12. Inspiration → reinvention map (explicit)
| Borrowed idea (source) | Ka reinvention |
|---|---|
| omp hashline `[PATH#TAG]` anchored edits | ledger mtime+hash read-tracking; anchors deferred (Phase 7 candidate "clamps") |
| omp compaction methodOrder + snapcompact | digest ladder (caps → prune → digest; elision/promotion later); bitmap trick NOT adopted (scope) |
| omp steering/follow-up queues | interjection / deferral, `+`-prefix input |
| omp terminal breadcrumbs | waypoints |
| omp compat-flag system (~60 flags) | dialects.toml, ~12 flags, vendor-merged |
| omp artifact spill | `spill://` store |
| codex Op/EventMsg queues | Command/Event protocol (NDJSON-first) |
| codex rollouts + zstd | strand records (plain JSONL; zstd later) |
| codex sandbox matrix | `ka-sandbox` optional crate, landlock-first |
| goose McpClientTrait platform tools | Hands trait (same seam for pathfinder/MCP later) |
| goose state-machine ops | Receiving/Speaking/Acting/Settling machine |
| claude bash subcommand/wrapper analysis | bash decomposition (shlex-based, no tree-sitter) |
| droid unbypassable blocklist + program resolution | hardstops |
| cline `requires_approval` | clearance annotations fixed per tool (no per-call model flagging in v1) |
| crush filetracker | ledger |
| pi tiny-core philosophy | seven Hands, zero mandated subsystems |
| gemini perf baselines | footprint contract in CI |
| scout/explore subagents (claude/omp) | pathfinder |
| aider auto-commit + undo | read-only git awareness (snapshot in header; VCS stays the user's) |
| tiktoken/usage accounting (pi, codex) | char-ratio estimate + usage true-up, tokenizer-free |
| claude/amp self-updaters | `ka update`, signature-gated, manual-only |

## 13. Deliberately kept ecosystem-standard (not renamed — interop wins)
SKILL.md skills format · AGENTS.md · markdown agent/command frontmatter · hooks exit-2-block JSON contract · `mcp__server__tool` naming (Phase 6) · Claude-Code-compatible stream-json option (Phase 6) · MCP stdio/http transports (optional crate).

## 14. Verification strategy
- **Wire fixtures (mandatory, CI)**: recorded SSE transcripts per wire — normal streams, malformed chunks, partial-JSON repair cases, retry/overflow paths — replayed through a local socket; wiremock covers headers/backoff. No keys in CI, ever.
- **Live smoke (optional, pre-release)**: `cargo xtask live-smoke` runs one real round-trip each against Anthropic, an OpenAI-compatible endpoint, and a local Ollama; failures block release, never CI.
- **Footprint contract**: cold-start ms and idle RSS asserted against committed `baselines.json` in CI (Phase 0 opt-in, mandatory from Phase 3).
- **Engine property tests**: strand replay determinism (records → tip → identical engine state), ledger refusal on stale writes, bash decomposition table tests, hardstop matrix incl. wrapper-obfuscation cases.
