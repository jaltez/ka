# Ka — Phased Roadmap (FINAL, post-grilling)

Locked decisions: **tokio · ratatui+crossterm · 2 wires (anthropic-messages, openai-chat) · exact-match edits + read-tracking · safety in Phase 2 · local models mandatory Phase 1 · strict TOML · Rust-regex-only search · pathfinder subagent mandatory Phase 6 · pure JSONL core with optional index crate later · keys-only core auth (subscription OAuth = future `ka-passport` crate) · read-only git awareness · SSE fixtures + optional live smoke · char-ratio token estimate with usage true-up · `ka update` from signed GitHub releases.**

Legend: **[M]** mandatory · **[O]** optional/deferrable. Names refer to the architecture doc (`architecture.md`).

## Phase 0 — Skeleton & Contracts
- **[M]** Workspace crates: `ka-agent` (engine), `ka-protocol` (Command/Event wire types), `ka-dialect` (providers), `ka-strand` (sessions), `ka-term` (TUI), `ka-cli` (bin)
- **[M]** Engine↔surface protocol: two mpsc queues; serde enums; NDJSON-serializable (headless reuses it verbatim)
- **[M]** Strict TOML config chain: defaults < `~/.config/ka/ka.toml` < `.ka/ka.toml` < env < flags; unknown keys = hard error with line numbers; generated JSON schema
- **[M]** `dialects.toml` model catalog as data: context window, max output, effort levels, pricing, modalities, dialect flags
- **[M]** CI: clippy `-D warnings`, forbid unwrap/expect in engine crates, release profile (thin-LTO, strip, panic=abort), cross-build musl
- **[O]** Release signing setup (ed25519 minisign-style keypair + CI artifact signing) — prerequisite for `ka update` in Phase 3
- **[O]** Perf baselines in CI (cold-start ms, idle RSS vs committed `baselines.json`) — gemini-cli practice, adopt early
- **[O]** `ka doctor`

**Exit:** `ka run "hi"` streams a canned event sequence through the real protocol.

## Phase 1 — Providers, Selection, Local Models
- **[M]** Wire adapters: `anthropic-messages`, `openai-chat` (SSE; reqwest+rustls, no provider SDKs)
- **[M]** Unified stream events: text/thinking/toolcall deltas, partial-JSON arg accumulator with repairing parser, stop-reason normalization
- **[M]** Dialect flag profile (~12 flags): message shaping, reasoning field, max-tokens field, sampling support, tool-choice downgrades; catalog defaults deep-merged with user overrides
- **[M]** Selectors `vendor/model:effort`; roles `default`/`fast`; persisted model-change records
- **[M]** Auth ladder: env > `.env` chain > keyring (optional dep); `!cmd` secret indirection; no secrets in config; **keys-only by design — subscription OAuth never links into core** (future `ka-passport` crate plugs a TokenSource)
- **[M]** Retry engine: classified errors, backoff+jitter, retry-after, overflow marked (never blind-retried)
- **[M]** Prompt-cache plumbing per wire + cache-hit accounting
- **[M]** **Local models:** discovery probes (Ollama/LM Studio/vLLM `/v1/models`), capability sniff (context, tools, vision), unbounded first-byte timeout, reasoning-replay dialect flag
- **[M]** Token accounting: char-ratio estimate (per-dialect `ratio`, default 4) for meters/digest triggers; every response's usage fields overwrite the estimate (true-up)
- **[M]** Wire test suites: recorded SSE fixtures per wire (incl. malformed-chunk/repair cases) replayed in CI via local socket + wiremock for headers/retries — no keys in CI
- **[O]** Usage/cost footer data (pricing already in catalog)
- **[O]** `cargo xtask live-smoke` — 5-minute real-key checks (anthropic + openai-compatible + ollama) before releases, never in CI
- **[O]** Fallback chains / credential rotation

**Exit:** streaming completion + tool-call round-trip against Anthropic, an OpenAI-compatible cloud provider, and a local Ollama model, all through one interface.

## Phase 2 — Loop, Core Tools, Hard Safety
- **[M]** Turn loop: prompt → stream → tool calls → parallel execution (ordered results) → feed back; stop/length/tools; max-steps cap; abort keeps partial work
- **[M]** Interjection/deferral input queues (mid-turn steering)
- **[M]** Tools: `read` (line/byte selectors, caps), `edit` (exact-match + read-before-edit + changed-since-read via ledger), `write`, `bash` (timeout, output caps + `spill://` files, process-tree kill, auto-background), `glob`, `grep` (Rust regex only, instructive error on unsupported constructs)
- **[M]** Git read-only awareness: engine snapshots repo state (branch, dirty file list) into strand context at turn start; `glob`/`grep` respect gitignore; **no commits, no undo — VCS stays the user's**
- **[M]** Clearance annotations on every tool (read/write/exec + read-only/idempotent)
- **[M]** Tool-result hygiene: truncation, spill pointers, empty-result elision marker
- **[M]** **Safety floor:** `guarded`/`free` modes; session "always allow"; bash compound-segment splitting + wrapper stripping + redirection-as-write; **hardstops** (unbypassable catastrophic list) prompting even in `free`
- **[O]** Loop detection (interaction-signature hash)
- **[O]** `ask` tool (lands with TUI in Phase 3)

**Exit:** dogfood on a real repo in `guarded` mode without hand-editing files.

## Phase 3 — Strands & Initial TUI
- **[M]** Strand store: append-only JSONL, header record, record kinds (message, model/effort change, digest, boundary, custom namespaced), id/parent tree + leaf pointer, split-to-new-file
- **[M]** Resume + interrupted-turn synthesis (dangling turns marked aborted)
- **[M]** ratatui TUI: streaming transcript, input editor + history, abort key, footer (tokens, cost, cache-hit %, context %), `ask` dialogs
- **[M]** Session picker (`ka`, `ka -c`)
- **[M]** `ka update`: fetch from GitHub releases, ed25519 signature verification before swap, opt-in channel tag (stable/edge); no auto-update, no background checks
- **[O]** Waypoints (per-terminal continue tokens)
- **[O]** Titles via `fast` role
- **[O]** Markdown export

**Exit:** daily driver for small tasks; resume after crash is clean.

## Phase 4 — Context Survival
- **[M]** Tool-output pruning (protected recent window, minimum-savings threshold)
- **[M]** Digest compaction: kept-tail budget, never cut inside a tool pair, split-turn handling, same-model option, `/compact [focus]`
- **[M]** Reserve-based trigger (max(16k, 15% window)) + overflow → digest-and-retry
- **[O]** Zero-LLM elision pass (old outputs → `spill://` pointers)
- **[O]** Context promotion (larger-window sibling switch)
- **[O]** Speculative digest (pre-threshold arm)
- **[O]** Full display history with digest dividers

**Exit:** multi-hour session with no manual context surgery.

## Phase 5 — Permissions as Data & Trust
- **[M]** Per-tool allow/ask/deny rules in config (compound bash patterns)
- **[M]** Project trust gate for `.ka/` local config/skills/hooks
- **[O]** One-way secret redaction (env scan + regex set) in tool results
- **[O]** `ka-sandbox` feature crate: landlock+seccomp (Linux) / seatbelt strings (macOS), fail-closed
- **[O]** Rule import from deny-list templates

**Exit:** `free` mode usable on trusted repos with a defensible floor.

## Phase 6 — Conventions, Extension, Pathfinder
- **[M]** AGENTS.md hierarchy (root→cwd, lazy per-directory) + `ka init`
- **[M]** Skills: SKILL.md standard, progressive disclosure
- **[M]** Hooks: shell commands, JSON envelope, exit-2 block (ecosystem-compatible contract), PreToolUse/PostToolUse first
- **[M]** **Pathfinder:** read-only subagent (child session, reduced toolset, summary-only return); spawn seam = the same engine, isolated strand
- **[O]** MCP client behind cargo feature (startup-gated, deferred tool discovery)
- **[O]** Custom slash commands (markdown, `$ARGUMENTS`)
- **[O]** Markdown-defined agents (model/tools frontmatter)
- **[O]** Claude-Code-compatible `--print` stream-json

**Exit:** ecosystem interop (reads others' skills/rules) + context protection via delegation.

## Phase 7 — Power Features (optional, ordered by pull)
1. Plan mode (read-only toolset, plan file, approval handoff, `plan` role)
2. Strand tree navigation + offshoot summaries
3. Snapshots/rewind (write-tree objects + patch records)
4. Background jobs (detached bash + job tools)
5. Worktree isolation for subagents
6. `ka-index` crate (SQLite/FTS cross-session search)
7. openai-responses third wire; provider-native digest hooks
8. ACP server (stdio) for editor embedding
9. HTTP/SSE server mode
10. Memory tiers (MEMORY.md)
11. Web search + URL reader tools
12. Structured-output mode (schema-constrained replies)

## Non-goals (explicit, permanent)
No in-process plugin runtime · no vector DB / semantic indexing · no browser or computer control · no image gen / TTS / voice · no enterprise/MDM/team/cloud tier · no telemetry beyond optional local logs · no eval kernels · **no subscription OAuth in core** (`ka-passport` crate may add it later) · **no git mutation** (read-only awareness only).

## Footprint budget (enforced from Phase 0)
Single static binary ≤ 10MB (musl, stripped) · cold start ≤ 50ms · idle RSS ≤ 15MB · zero network at steady state · children only: user shell, stdio MCP (optional), git (optional).
