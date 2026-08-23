# omp (tools & surfaces) — Feature Extraction

> Source: omp internal docs (all 28 tools/*.md + TUI/settings/approval/env/CLI/runtime docs). Scout: OmpToolsUi.

## Identity
- Repo: oh-my-pi ("omp"); monorepo `packages/*` (TypeScript on Bun) + `crates/*` (Rust NAPI addons). Binaries: `omp` CLI; npm packages `@oh-my-pi/pi-coding-agent`, `pi-natives` (+per-platform leaves), `pi-tui`, `omptype`, `snapcompact`, `pi-mnemopi`, `omp-stats`, python `robomp`.
- Architecture: TS orchestration (session, tools, TUI, extensibility) over Rust N-API addon (`pi-natives`: grep/glob/ast/shell/pty/isolation/desktop/highlight/diff/tokens/snapcompact) + crates (`pi-walker`, `pi-ast`, `pi-iso`). Process-global registries (agent registry, IRC bus, job manager, launch broker); daemon brokers per project (browser, LSP mux, launch, relay).
- Footprint: lazy tool loading (`loadMode: essential|discoverable`), lazy native subpath imports, bounded in-memory output (50KB tail window) with artifact spill to disk, content-addressed blob store, FS scan cache (TTL 1s, 16 entries), persistent shells/kernels, per-platform addon with modern(AVX2)/baseline CPU variants.

## Core loop & orchestration
- Turn loop: user message → model stream (text/thinking/toolcall deltas monitored by TTSR) → parallel tool batch (per-tool `concurrency: shared|exclusive`) → results injected → steering/queued follow-ups → `turn_end` (rewind apply, todo sync, advisor delta queue, mid-turn compaction checks).
- Steering: interrupt (`immediate|wait`) + queued steering messages delivered mid-run; double-Escape → branch/tree navigation.
- Plan mode: read-only tool subset, plan proposals via `xd://propose`, plan-review UI / ACP elicitation / PlanYolo auto-approve then model switch to executor.
- Prewalk: start on strong model, hand off to cheaper model at first edit/write once todo exists.
- Vibe mode (`/vibe`): director session reduced to `read` + parent `todo` + 5 vibe tools, persistent keep-alive worker subagents in two tiers (`fast`=sonic/@smol, `good`=task/@task).
- Subagent lifecycle: running → idle (TTL 7min) → parked (disposed, revivable by `hub` message) → revived. "Main" never parked.
- Advisor subsystem: second model reviewing transcript deltas, `nit|concern|blocker` severities, WATCHDOG.md/yml rosters, quarantines unsafe advisor output.
- TTSR: regex/ast-grep rules match streaming text/thinking/tool-arg deltas; abort generation, inject `<system-interrupt>` reminder, retry (contextMode discard/keep).
- Async job manager: background bash/task/vibe jobs, auto-delivery of results, smart poll ladder [5s..5m], maxJobs 1..100, job retention 5 min.

## Tool catalog (actual names)
1. **read** — universal reader: files (line selectors `:N`, `:A-B`, `:A+C`, `:raw`, `:conflicts`), structural code summaries w/ elision footers, directories, archives (`:inner/path`), SQLite (`:table`, `?q=SELECT`), internal URLs (`skill:// agent:// artifact:// history:// issue:// pr:// local:// mcp:// memory:// omp:// rule:// security:// ssh:// vault:// xd://`), images, documents (pdf/docx/pptx/xls/rtf/epub), notebooks, URLs (reader-mode), profiler reports. Hashline snapshots.
2. **write** — files, archive entries, SQLite rows, `conflict://` merge-resolution (`@ours/@theirs/@base/@both`), `xd://<device>` dispatch; strips pasted hashlines; shebang → chmod +x; plan-mode guard; generated-file guard; LSP format/diagnostics writethrough.
3. **edit** — hashline patch language: `[PATH#TAG]`, `PUT N.=M:`/`PUT N*:`/`PUT <N:`/`PUT >N:`/`CUT … @reg`/register paste/`REM`/`MV`; stale-tag snapshot recovery; modes `hashline|apply_patch|patch|replace`; no-op loop guard.
4. **bash** — `command/env/timeout/cwd/pty/async`; interceptor routing to read/grep/glob/edit/write/hub; direnv preflight; persistent native Shell sessions; PTY overlay; auto-background after 60s; internal-URL expansion; bundled jq (vendored jaq) + uutils builtins.
5. **grep** — Rust regex → PCRE2 fallback → literal recovery; per-file caps 20/200, file pages w/ `skip`, 512-char line cap, 50KB byte cap, archive members, internal URLs, 4MiB per-file skip, 30s budget, hashline anchors.
6. **glob** — limit 200, mtime sort, gitignore default on, multi-root `;`, 5s timeout returning partials, prefix-folded tree output.
7. **ast_grep** — native ast-grep (60+ languages), metavariables `$NAME/$_/$$$NAME`, disabled by default.
8. **ast_edit** — structural rewrite preview → staged proposal, apply/discard via `write xd://resolve|xd://reject`; overlap detection.
9. **eval** — persistent kernels: `py` (NDJSON subprocess, IPython-style magics, matplotlib Agg→PNG), `js` (Bun worker VM), `rb`/`jl` opt-in; prelude helpers `display/read/write/env/output/tool.<name>/completion/agent/parallel/pipeline/log/phase/budget`.
10. **task** — subagent spawner: batch `{context, tasks[]}`; per-item `name/agent/task/effort/outputSchema/isolated`; isolation modes none/auto/apfs/btrfs/zfs/reflink/overlayfs/projfs/block-clone/rcopy; hidden `yield` tool; outputs `agent://<id>`, transcripts `history://<id>`; session semaphore; recursion depth gate; soft request budget 200.
11. **hub** — merged coordination: messaging (send/await/inbox/list/wait), jobs (wait/cancel/jobs), processes (start w/ ready{log,port}, ps, logs w/ grep/follow/cursor, stop, restart, describe, send stdin/keys/signals, wait).
12. **todo** — phase/task ops init/start/done/drop/block/unblock/rm/append/view; single-active-task normalization; parent-owned.
13. **web_search** — 23-provider chain; Google-style directives, lenient post-filtering, Public Web fan-out consensus ranking, browser-backed scrapers escalate to shared headless Chromium.
14. **browser** — named tabs; kinds: headless (project-shared broker), spawned app, connected cdp, Browser Relay (user's Chrome via MV3 extension), cmux (WKWebView); `run` executes JS in worker; ARIA snapshots `[ref=eN]`; screenshots ≤1024px/150KB.
15. **computer** — host desktop runtime: windows/displays/capabilities/clipboard/elementAt/focusedWindow; AX trees with ref generations; `read_only` flag; macOS/X11/Wayland-portal/Windows.
16. **debug** — DAP session driver: launch/attach (14 adapters), breakpoints (source/function/instruction/data), stepping, evaluate, scopes/variables/disassemble/memory, custom_request; recursive child sessions.
17. **lsp** — 13 actions (diagnostics incl. workspace builds, definition/implementation/references, hover, symbols, rename, rename_file, code_actions, status, reload, capabilities, raw request); broker-shared per-project mux; custom linter clients; `lsp.json`.
18. **ask** — interactive multi-question picker/editor; options w/ preview/header, multi-select, recommended, timeout auto-select; vocalizer + terminal notification.
19. **inspect_image** — vision-model image inspection; auto-registered when active model lacks native image input; auto-resize ladder.
20. **generate_image** — multi-provider image gen/edit w/ credentialed fallback.
21. **tts** — local Kokoro-82M ONNX or xAI Grok Voice.
22. **checkpoint / rewind** — conversation boundary → investigate → `rewind(report)` prunes exploratory branch, persists branch_summary; conversation-only (no git/file restore).
23. **security_scan** — preflight (plan fingerprinting) → start (background restricted scan session w/ security-reviewer workers) → status/validate; cloud actions; immutable `security://` URI namespace (scan.json/findings.json/report.md/SARIF/provenance).
24. **learn / manage_skill** — autolearn: lessons (hindsight/mnemopi/learned.md) + managed SKILL.md CRUD.
25. **recall / reflect / retain / memory_edit** — memory tools; bank scoping global/per-project/tagged.
26. Hidden: **yield** (subagent terminal), **advise**, **security_publish**, **vibe_spawn/send/wait/kill/list**, xd:// devices **resolve/reject/propose**.

## Context management (tools-side)
- Read-side: structural summaries (`read.summarize`, ≤2MiB/20k lines, elision footer), hashline snapshot store (256 paths × 4 versions, 4MiB cap), URL cache w/ artifact-backed pagination.
- Output truncation: `OutputSink` 50KB UTF-8-safe tail + 20KB head w/ middle elision + 768B/line cap + artifact mirroring (`artifact://<id>`); subagent outputs capped 500KB/5000 lines w/ full artifact.
- FS scan cache (Rust `pi-walker`): keyed by canonical root + WalkOptions; TTL 1000ms, 16 entries, empty-result recheck 200ms, mutation-driven invalidation.

## Extensibility
- Discovery `.omp > .claude > .codex > .gemini` via capability providers: skills, slash commands, rules, prompts, hooks, custom tools, extensions, MCP, agents, settings, context files.
- MCP tools as `mcp__server__tool`; child subagents proxy parent connections.
- Plugins/marketplace: `omp plugin install/link`, `--plugin-dir`; Claude marketplace plugin agents honored.
- Themes (JSON ~66 color tokens, symbol presets), keybindings.yml, custom status-line segments, DAP/LSP json configs.

## Safety & permissions
- Three tiers `read|write|exec`; default-deny to exec for undeclared. Modes `always-ask|write|yolo` (default yolo).
- Resolution: tool `approval(args)` → tool deny → user deny → yolo exceptions → override gating → explicit tool policy → user policy → mode tier.
- `tools.approval.<tool>: allow|deny|prompt`; `bash.patterns` ordered glob rules (allow must match whole command; compound-segment splitting); critical-pattern safety overrides (rm -rf /, fork bombs, fetch-and-execute, /etc/passwd, shutdown) force prompts even under allow.
- `bashInterceptor.patterns`: regex → dedicated-tool routing (cat→read, rg→grep...); best-effort, not security boundary.
- Known gap: `eval` can spawn shells bypassing `bash.patterns`; pair with `tools.approval.eval`.
- Computer use `read_only: true` → read tier; approval body shows 2,000-char code; fail closed headless.
- Subagents run forced `tools.approvalMode: yolo`; parent `task` call is the authorization boundary.
- ACP: `session/request_permission` for bash/edit/delete/move, form elicitation.
- Secrets: obfuscation + redaction before provider requests; auth broker/gateway vault.
- Isolation for subagents: copy-on-write workspaces (APFS/btrfs/Zfs clones, overlayfs, ProjFS, reflink, block-clone, rcopy) with baseline capture, patch/branch merge.

## Model/provider abstraction (tools-side)
- `modelRoles`: default, smol, slow, vision, plan, designer, commit, tiny, task, advisor + custom; `:thinking` suffixes; `cycleOrder` for Ctrl+P cycling.
- Thinking ladder minimal→max w/ per-level budgets; `auto` classifier; `ultrathink` magic.
- Retry/fallback: `retry.modelFallback` chains keyed by role / `provider/model` / wildcards; cooldown-expiry revert.
- Tool wire formats `tools.format`: auto/native/glm/hermes/kimi/xml/anthropic/deepseek/harmony/qwen3/gemini/gemma/minimax; Codex Code Mode collapses tools into eval/ask/todo + `tool.*()` bridge.
- Sampling: temperature/topP/topK/minP/penalties, textVerbosity, service tiers per family.
- Prompt caching: `providers.cacheRetention` auto/short/long/none, `--prompt-cache-key`, `--provider-session-id`; Anthropic idle keep-alive refreshes.

## Surfaces
- **TUI**: differential renderer (TerminalFrameProvider, monotonic history batches, viewport diffing), alt-screen resize w/ DSR/CPR anchor recovery, synchronized output detection, Kitty graphics w/ ImageBudget demotion, OSC 5522 enhanced paste, ToolExecutionComponent row-collapse, markdown+syntax, Agent Hub (Alt+A), status line presets, `/hotkeys`.
- **Headless**: `-p/--print`, `--mode json` event stream.
- **RPC**: `--mode rpc|rpc-ui` JSON-RPC over stdio (bash/abort_bash; client terminal bridge).
- **ACP**: `omp acp` Agent Client Protocol (IDE clients).
- **SDK**: embeddable package (non-CLI hosts get process-local browser/LSP fallbacks).
- **Web**: collab-web guest UI, omp-stats dashboard, robomp dashboard, metaharness benchmark dashboard.
- **CLI subcommands**: launch, acp, auth-broker, auth-gateway, agents, bench, browser-relay, cleanse, commit, completions, compress, config, dry-balance, gc, grep, gallery, grievances, install, join, models, plugin, ps, say, share, setup, shell, read, ssh, stats, update, usage, tiny-models, token, ttsr, worktree/wt, search/q.

## Session & collaboration (tools-side)
- Sessions `<ISO>_<uuidv7>.jsonl`; resume, fork (artifacts copied, `parentSession`), move-to-new-cwd, export HTML, import (`--from-claude`, `--from-codex`), `/share`, `/join`.
- Multi-agent collab: process-global IRC bus (mailboxes cap 100, delivery receipts, parked-agent revival), agent registry (running|idle|parked|aborted), Agent Hub TUI, collab web guests.
- Background processes: project-scoped launch broker over `~/.omp/run/daemons/<project-hash>/` shared by every omp instance in the project; stable names, ready gates (log regex + TCP port), pty stdin/keys/signals, 25MiB log + 1 rotation, byte cursors + follow, restart policies w/ bounded backoff, persist/detached; `omp ps` CLI mirrors.

## Config & conventions
- Layered settings: schema defaults < global config.yml < project `<cwd>/.omp/config.yml` < `PI_CONFIG_FILES` < `--config` overlays < runtime flags/env. Deep-merge objects; arrays replace wholesale. Invalid YAML quarantined to `.broken-*`.
- `omp config list/get/set/reset/path/init-xdg`; `PI_CODING_AGENT_DIR`; named profiles; XDG opt-in.
- Env resolution: process env < project `.env` < agent `.env` < config-root `.env` < home `.env`; `OMP_*` mirrored to `PI_*`.
- Context files: `AGENTS.md`, `SYSTEM.md`/`APPEND_SYSTEM.md`, `RULES.md` (sticky), `WATCHDOG.md` (advisor-only), `TITLE_SYSTEM.md`, `PERSONALITY.md`; `@path` imports.
- Key per-tool defaults: bash timeout 300s, glob limit 200, grep 20-file pages/512-col/30s, browser 30s, debug 30s, lsp 20s, eval 30s, computer 120s, `tools.maxTimeout` ceiling, `tools.artifactSpillThreshold` 50KB/head 20KB.

## Distinctive features
- **Hashline editing**: content-hash snapshot tags (`[PATH#TAG]`) minted by read/grep, consumed by edit; stale-anchor recovery; registers for cut/paste across files.
- **xd:// resolution devices**: staged previews and plan approval finalized by plain `write` calls to virtual URIs, enforced via soft tool requirements.
- **TTSR**: mid-stream rule violation abort + retry injection.
- **Checkpoint/rewind**: conversation-tree pruning collapsing exploration into a retained report branch.
- **Vibe mode**: director + persistent worker tiers.
- **Advisor + WATCHDOG.md/yml**: passive reviewer roster w/ severity-gated steering.
- **snapcompact**: deterministic PNG bitmap compaction (no model call).
- **hub**: one merged tool for peer messaging + job control + process supervision.
- **FS scan cache** with WalkOptions-partitioned keys.
- **Bundled coreutils** (uutils + vendored jaq jq).
- **Browser multiplicity**: 5 backends incl. user's real Chrome via relay extension; ARIA-ref automation.
- **Native computer use** AX-first with ref-generation snapshots.
- **Codex Security cloud integration** + immutable `security://` store.
- **23-provider web_search chain** w/ consensus ranking.

## Canonical workflows
1. Interactive coding: read (summaries+anchors) → edits (hashline patches verified) → LSP diagnostics-on-write → bash run/tests (auto-background) → todo → `/tree` branching.
2. Plan-first: `--plan-yolo` → plan model → `xd://propose` → auto-approve → executor model.
3. Parallel scout fan-out: `task` batch → background jobs → async-result injections → `hub` DMs → Agent Hub oversight.
4. Long-running services: `hub start` w/ ready gates → iterate → `hub logs follow` → `hub send` stdin → `hub stop`; survive across instances via broker.
5. Deep investigation: `checkpoint(goal)` → explore → `rewind(report)` prunes exploration, retains report.
6. Persistent REPL: `eval py` kernel state, magics, `agent()`/`parallel()` from cells.
7. Web research: `web_search` → `read` URL reader-mode → edits → citations.
8. Security review: `security_scan preflight → start → security:// reads → validate`.
9. Vibe delegation: `/vibe` → fast workers → steering → escalate to good tier.
10. Automation: `omp -p --mode json` in CI; `omp acp` for IDEs; robomp webhook→RPC loop.

## Rust / low-footprint notes (for Ka)
- Hybrid shape: TS orchestration + Rust NAPI for hot paths. Natives: per-platform leaves, x86-64-v3/v2 variants w/ fallback chain, version sentinel, post-load bounded Tokio/Rayon pools, embedded tarball extraction.
- Rust modules worth copying: grep (regex→PCRE2→literal fallback), walker w/ scan cache, ast (gitignore-aware, back-to-front edit application w/ overlap check), shell (persistent sessions, quarantined timed-out sessions), pty, file_lock, iso (CoW isolation PAL w/ candidate fallback), desktop, highlight, diff, tokens, snapcompact PNG, vectors.
- Bounded-memory discipline: 50KB tail + 20KB head OutputSink w/ artifact spill; 8KiB read chunks; line/column caps; mailbox/job/artifact caps; snapshot LRU; SSE buffers (1000 events/512KB).
- Process model: one host process; workers for browser tabs (crash isolation); subprocess kernels for py/rb/jl (NDJSON); daemon brokers per project under `~/.omp/run/daemons/<hash>/`.
- Perf tricks: memoized TUI diffing; fs scan cache; lazy module/provider loading; throttled streaming updates (50–200ms); progress coalescing; mtime-sorted glob early stop; Rayon `PI_WALK_WORKERS=4` w/ 256-item parallelization threshold.
- Settings schema-driven config (single source for CLI/TUI/JSON), strict tool schemas w/ lenient opt-in repair, dynamic per-session tool schemas.
