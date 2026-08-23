# Ka — Implications for a Very-Low-Footprint Rust Agent

Synthesis of the ecosystem survey into design guidance. Constraint: **model-agnostic, very low footprint, Rust**. Precedents: codex (Rust, ~130 crates — heavy), goose (Rust — heavy), pi (TS — tiny-core philosophy), crush (Go — single process). Nobody currently ships a *small* Rust agent; the gap is real.

## 1. What "low-footprint" should mean (measured, not vibes)
- Single static binary (musl), <10MB, no runtime downloads at steady state; cold start <50ms; idle CPU ~0; RSS <20MB baseline.
- Adopt gemini-cli's practice: committed perf/memory baselines + CI regression gates (cold start, idle CPU, long session RSS).
- No embedded V8, no tree-sitter in core, no SQLite in core, no OTel, no tokenizer heuristics beyond a cheap estimator. All optional features behind cargo features.
- One process; children only: user shell commands, stdio MCP servers, LSP (optional), git (optional).

## 2. Workspace blueprint (steal from codex/pi/goose layering)
```
ka/            core engine: session, loop, tools, permissions  (no I/O deps beyond std+serde)
ka-protocol/   Op/EventMsg enums (SQ/EQ mpsc queues) + stream-json wire types   [codex]
ka-providers/  Provider trait; openai-chat + anthropic-messages + openai-responses wires; catalog as data
ka-session/    append-only JSONL store (+ optional index)
ka-sandbox/    bwrap/landlock/seatbelt adapters (feature-gated)
ka-cli/        one surface: interactive (ratatui? or line-diff TUI à la pi) + `ka run` headless
ka-mcp/        rmcp client (feature-gated)
```
- **Engine decoupled from UI via two mpsc queues; events are serde enums** (codex protocol crate — directly copyable).
- Minimal core set (codex's own conclusion): protocol + core + config + model-provider + rollout + sandbox + one surface.

## 3. Tool catalog — small core, everything else is data
Start with pi's 7 and omp's lessons (read/write/edit/bash/glob/grep + ask):
- `read` — line selectors (`:N`, `:N-M`, raw), structural summaries for code (footer elision), archives optional.
- `edit` — string-replace with read-before-edit + staleness detection (crush filetracker); optional anchored/hashline mode later.
- `write` / `bash` (timeout, output caps + spill file, process-tree kill, auto-background) / `glob` / `grep` (vendored regex; ripgrep subprocess optional) / `ask`.
- Everything else = MCP tools, skills, or feature-gated modules. Goose's `McpClientTrait` pattern is key: **built-in tools implement the same trait as MCP tools** — zero extra abstraction, tools-as-data.
- ToolAnnotations (readOnly/destructive/idempotent/openWorld, priority) on every tool — drives both permissions and output filtering (goose/rmcp).
- Tool-result hygiene from day 1: truncation caps + artifact spill URI (`artifact://<id>` recovery), `[Uneventful result elided]` useless-flag (omp).

## 4. Provider layer — the moat for "model-agnostic"
- 3 wire protocols cover ~everything: `anthropic-messages`, `openai-chat`, `openai-responses`. (pi does 4 incl. google-generative-ai; codex is responses-only; omp does 10 — overkill for v1.)
- **Compat as data, not branches**: per-model flag struct (omp's ~60-flag catalog is the reference; start with ~12: supportsDeveloperRole, requiresToolResultName, requiresAssistantAfterToolResult, thinkingFormat, maxTokensField, supportsSamplingParams, toolChoiceDowngrades, reasoningContentReplay...). Deep-merge overrides from a models.toml. `whenThinking` pre-built variant swap.
- Model catalog = static TOML/JSON (context window, pricing, efforts, modalities) + runtime discovery for Ollama/LM Studio/vLLM (`/v1/models` + capability probing). Fingerprinted cache like omp.
- Roles from day 1: `default / fast / plan` (minimum viable set; omp/pi/plandex/amp all converge on this). Model selectors `provider/model:effort`.
- Tool-call dialects for no-native-tools models: defer; but design the message model so history can be re-encoded (omp's AgentMessage→convertToLlm boundary and pi's transformContext→convertToLlm two-stage pipeline are the correct seam).
- Local models: unbounded first-event timeout, KV-cache-preserving reasoning replay (byte-exact `<think>` re-emit).
- Caching: per-wire cache_control/prompt_cache_key; per-request date/cwd reminder instead of system-prompt mutation (omp #7404); cache-hit-rate surfaced.

## 5. Session model
- Append-only JSONL per session; first line = meta; **tree via id/parentId + leaf pointer** (omp/pi — branching is pointer movement, zero rewrites); entry types: message, model_change, compaction, branch_summary, reset_boundary, custom (namespaced).
- Reverse scanner for cheap tails; zstd cold files optional; **no DB in core** (index is rebuildable — codex's rule).
- Resume w/ interrupted-turn synthesis (omp session_exit → synthetic aborted message).
- Fork = new file from entry id. Terminal breadcrumbs (omp) are a cheap killer feature for per-pane `ka -c`.

## 6. Context management ladder (in build order)
1. Tool-output truncation + spill + pruning with protected recent window (40k/20komp defaults).
2. LLM summary compaction w/ kept tail, never cut at toolResult, split-turn handling, same-model option (roo lesson: avoids format mismatch).
3. Cheap local elision ("shake": regex-level replacement of old tool outputs with artifact refs) — zero-LLM, trivial in Rust.
4. Model promotion on overflow before compaction (omp).
5. Later: provider-native compaction, speculation, snapcompact (omp's bitmap trick is uniquely suited to a lean binary: PNG encode + public-domain fonts, no model call).

## 7. Permissions & safety (lean but credible)
- 3 tiers read/write/exec + modes ask-edits/yolo (omp) or untrusted/on-request/never (codex) — pick two vocabularies max.
- Rule engine as data: per-tool allow/ask/deny with **bash compound-segment splitting + wrapper stripping (timeout/nice/nohup/xargs/env)** and redirection-as-write (claude's mini shell-semantics engine — the single highest-value safety component; implement with shlex + rules, no tree-sitter needed for v1).
- **Unbypassable blocklist + resolve-the-actual-program (droid)** for a handful of catastrophic patterns (rm -rf /, fork bombs, fetch-and-execute) — force-prompt even in yolo.
- Sandbox optional module: landlock+seccomp (pure Rust, no binaries — unlike bwrap) + seatbelt strings on macOS; fail-closed.
- Secrets: env-scan + regex list with reversible HMAC placeholders restored at tool boundary (omp) or one-way redaction (amp) — pick one-way for v1.
- Project trust dialog gating project-local skills/hooks/config (pi/claude/codex convergent).

## 8. Extensibility without weight
- **No in-process plugin runtime in v1.** Rust has no safe dyn-loading story for logic; use:
  - Hooks: shell commands w/ JSON stdin/stdout, exit-2-block contract — Claude-Code-compatible semantics is a free ecosystem win (openhands adopted it verbatim; codex too).
  - Skills: agentskills.io SKILL.md standard, progressive disclosure (~100 tokens metadata; body on demand) — everyone reads everyone's dirs.
  - MCP: rmcp behind a feature flag; 250ms startup gate + deferred tools + late registration (omp).
  - Custom agents/commands: markdown + YAML frontmatter, role-routed models (universal standard).
- Later: WASM (wasmtime — but that's MBs) or subprocess RPC (pi's `--mode rpc` shows the seam).

## 9. Surfaces (in build order)
1. `ka run` headless w/ stream-json (Claude-Code-compatible event schema — amp proved compat wins; cline NDJSON is the minimal variant) + `--output-schema` structured final answer (codex) + exit codes as API.
2. Line-based interactive TUI (pi's differential-render approach; ratatui if budget allows — it's cheap enough).
3. ACP server mode (stdio) → free Zed/JetBrains/VS Code-via-extension integration (goose/kilo/opencode/gemini converge on ACP).
4. Later: HTTP+SSE server (opencode model) if embedding takes off.

## 10. Deliberately out of scope for v1 (the weight everyone else carries)
Browser automation, computer use, eval kernels, image gen, TTS/voice, semantic/vector search (roo needed Qdrant; cline's new core dropped it for ripgrep — proof), GUI canvases, team/enterprise tiers, remote VM orchestration, realtime voice, embedded tiny models. All reachable later as MCP servers or feature crates.

## 11. Cheapest high-leverage differentiators to steal
1. **Per-TTY terminal breadcrumbs** (omp) — trivial, beloved.
2. **Cache-hit-rate in the footer** (pi) — trivial, drives trust.
3. **Loop-detection via interaction-signature hashing** (crush) — ~50 LOC.
4. **Snapshot edits against read-time content hashes** (omp hashline, simpler variant) — kills silent-clobber edits.
5. **Read-only scout subagent by default** (claude Explore/omp scout pattern) — child session w/ reduced tools, summary-only return; the single best context-protection pattern across the survey.
6. **Tool-context economy**: tool defs cost tokens — budget the listing, defer MCP discovery (claude/amp/codex convergent).
7. **Self-describing config**: `ka doctor`, `ka config` schema-generated docs (crush crush_info, codex config.schema.json).

## 12. Risks / cautions from the survey
- Compat-flag sprawl is the real cost of model-agnosticism (omp: ~60 flags, 11 dialects, 1.7k-line quirks doc). Budget for a quirks test-suite per provider from week 1; start with 3 wires and grow on demand.
- "Everything is an extension" (pi) pushes complexity to users; "everything is core" (omp/opencode) explodes footprint. Ka's line: **core = loop + 7 tools + JSONL sessions + 2 permission modes; everything else = data files, hooks, MCP, or feature crates.**
- Stream-json compat de-facto standard is Claude Code's shape — match it early or risk ecosystem isolation.
- Sandboxing on Linux without bwrap: landlock is in-kernel and pure-syscall — but read codex's `linux-sandbox` README first; they vendored bwrap for a reason (nested deny/reopen semantics).
