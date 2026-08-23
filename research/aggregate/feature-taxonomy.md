# Ka — Master Feature Taxonomy of AI Coding Agents

> Aggregated from 15+ agents surveyed 2026-08-23 (omp/pi, opencode, codex, claude code, gemini cli, crush, goose, amp, aider, cline/roo/kilo, openhands, plandex, droid/zed/cursor/copilot/devin).
> Attribution: ◆=near-universal · named agents = distinctive/deep implementation. Per-agent details in `agents/*.md`.

## 1. Identity & packaging
- Single-binary distribution: codex (Rust multitool binary w/ argv0 dispatch), crush (Go), goose (Rust + npm platform pkgs), pi (Bun-compiled), claude code (native binaries + bundled ripgrep), amp (Bun single-file exe) ◆
- Client/server split: opencode (OpenAPI+SSE server, all clients thin), crush (server + `--host` clients sharing workspaces), goose (`goose serve` ACP over WS), plandex (CLI + Go server + Postgres + LiteLLM sidecar), amp (server-driven loop), codex (app-server JSON-RPC for IDE/SDK; exec-server for remote envs)
- One-core-many-surfaces: goose (ACP stdio/HTTP), kilo (Kilo CLI core → IDE/CLI/mobile/Slack), cline (hub-spoke daemon), codex (TUI/exec/app-server/mcp-server all drive one core)
- Layered core library vs harness: pi (`ai` → `agent` → `coding-agent`; tui standalone), goose (`goose` / `goose-providers` / `goose-mcp` / `goose-cli`), codex (`protocol` / `core` / surfaces)
- Native/Rust hot-path layer under TS host: omp (pi-natives NAPI addon: grep/glob/ast/shell/pty/iso/desktop, AVX2/baseline variants)
- In-process embedded shell: crush (mvdan/sh for bash tool + hooks + config — identical on Windows)
- Self-update mechanisms ◆ (amp, claude, gemini, pi, goose `update [--canary]`, codex daemon hourly updater)
- Perf/memory regression gates in CI: gemini-cli (perf-tests + memory-tests vs baselines.json) — unique
- Footprint poles: pi (7 tools, no core MCP/subagents/permissions) ⇄ opencode/openhands (maximal)

## 2. Core agent loop
- Streaming turn loop: prompt → (hooks) → LLM stream → tool calls → permission gate → parallel execution → results → repeat until stop/abort/max-steps ◆
- Tool execution modes: parallel (default) with per-tool exclusivity vs sequential (pi executionMode; omp concurrency shared|exclusive; openhands regroup by llm_response_id)
- Stop reasons & continuation: `done(stop|length|toolUse|error)` mapping ◆; incomplete-output recovery → compaction or continuation-with-prefill (omp, aider `supports_assistant_prefill`)
- Retry engine: classified errors, exponential backoff + jitter, retry-after headers, credential rotation, model fallback chains with cooldown revert, replay-safety gate (omp, opencode, aider, goose, crush); overflow never retried generically — routed to compaction (omp, opencode)
- Steering vs follow-up queues: pi/omp (Enter=steer delivered between tool calls; Alt+Enter=follow-up after settle; queue restore on abort), claude (queue + Esc interrupt, work kept), crush (message queue), codex (turn steering, ≤100 queued turns w/ edit/reorder), amp (`Enter Enter` next boundary; `Esc Esc` immediate; `steer:true` stream-json), zed (queued + Steer), cursor (steering queue)
- Max-iterations/steps cap forcing text-only reply (opencode agent.steps; goose 1000 turns w/ continue-prompt; gemini maxSessionTurns)
- Loop detection: crush (SHA-256 tool+args+result signature, >5 in 10), goose (--max-tool-repetitions), opencode (doom_loop 3×→ask), gemini (disableLoopDetection), openhands (stuck detector sliding window), omp (ToolCallLoopGuard)
- Turn/state-machine architecture: goose (composable `ops_*` stages), codex (Session→Task→Turn; SQ/EQ Op/EventMsg queues), openhands (stateless Agent.step())
- Event-sourced state: openhands (append-only typed EventLog as single integration seam), omp (JSONL tree), opencode (SyncEvent single-writer aggregate log), codex (RolloutLine items)
- Mid-turn interrupts: Esc abort (work kept in history) ◆; omp TTSR aborts scoped to tool-call id
- Turn narration: gemini `update_topic` tool (title/summary/strategic-intent instead of chatty progress) — unique

## 3. Tool catalog primitives
Filesystem: read (line selectors, images, PDFs, archives, SQLite, notebooks, URLs ◆; omp universal reader incl. internal URIs), write, edit (string-replace ◆; hashline/anchored omp; LSP-formatted crush), glob, grep (ripgrep-backed ◆; omp Rust regex→PCRE2→literal), ls/tree
Patch formats: apply_patch V4A (codex freeform tool, opencode gpt-models-only), udiff/SEARCH-REPLACE (aider edit-format family: whole/diff/diff-fenced/udiff/patch/editor-*), structured AST edit (omp ast_edit + ast_grep 60+ langs, omp hashline [PATH#TAG])
Shell: bash w/ timeout, output caps + spill-to-file, process-tree kill, auto-background after threshold (omp 60s, crush 60s, claude timeout→background), PTY mode (omp, codex PTY-backed exec_command+write_stdin, gemini node-pty), persistent sessions (omp, crush embedded shell, goose), direnv preflight (omp), uutils/jq bundled (omp)
Search/navigation: LSP tools (crush 9 incl. lsp_replace_symbol; omp 13 actions incl. rename_file/workspace diagnostics; opencode/goose/gemini/codex via plugins), repo-map (aider PageRank tree-sitter map; plandex map context type; goose analyze tree-sitter call graphs), semantic search (roo tree-sitter+embeddings+Qdrant; goose memory vectors), sourcegraph search (crush, amp librarian)
Web: web_search (omp 23-provider chain w/ consensus; claude Anthropic-backed; gemini Google grounding; crush DuckDuckGo; goose skill w/ Tavily/SearXNG), webfetch (claude URL→markdown→small-model extraction w/ 15-min cache; omp reader-mode render pipeline; amp read_web_page via Parallel)
Browser automation: claude (browser tool), omp (5 backends incl. user's real Chrome via relay; ARIA refs), gemini (chrome-devtools-mcp bundled), openhands (14 browser tools, shared executor, rrweb recording), plandex (chromedp console-error debug)
Eval/kernels: omp eval (persistent py/js/rb/jl kernels w/ agent()/parallel()/completion() prelude), goose code-mode (TS via vendored v8), codex Code Mode (JS exec tool), opencode CodeMode (sandboxed JS orchestrating tools)
Subagents/orchestration: task/agent tool ◆ (see §7); hub messaging + process supervision (omp single merged hub tool)
Planning/tracking: todo/todos/task_tracker tools ◆ (omp phase-based w/ auto-promotion; openhands TASKS.json; claude TaskCreate shared lists for teams)
User interaction: ask/ask_user/question/AskUserQuestion (structured options, multi-select, recommended, timeout) ◆
Media: view_image/inspect_image ◆, image gen (claude/omp/amp painter/goose), TTS (omp Kokoro/xAI, roo), voice input (aider whisper, gemini push-to-talk, goose whisper dictation, codex realtime voice conversation WebRTC)
Memory tools: omp recall/retain/reflect/memory_edit/learn/manage_skill; goose remember/retrieve; openhands self-maintained MEMORY.md
Misc distinctive: claude Monitor (background watcher → events), claude CronCreate/ScheduleWakeup, codex clock.sleep ≤12h, omp checkpoint/rewind, omp security_scan (+security:// immutable store), goose apps (sandboxed HTML apps + autovisualiser ui:// charts), codex tool_search/get_context_remaining/new_context, omp yield (hidden subagent terminal), openhands canvas_ui (agent drives the frontend), crush crush_info/crush_logs self-introspection, plandex debug run→fix→retry, goose manage_schedule (cron from chat)
Anti-hallucination aids: omp tool_result useless-flag elision; plandex missing-file respond flow; omp stale-snapshot recovery for edits (filetracker in crush)

## 4. Context management
Compaction (auto near threshold + manual) ◆:
- Trigger styles: % of window (roo 100%→condense; goose 80%), reserve-based floor (omp max(16384,15%)), tokens (claude /autocompact), events count (openhands 120), threshold fraction (gemini 0.5)
- Method ladders: omp methodOrder [remote→snapcompact→handoff→shake→soft]; codex local+remote v2+fallback; claude clear tool outputs→summarize; aider recursive head/tail (≤3 depth); opencode summary head + preserved tail + prune
- Kept-tail budget: omp keepRecentTokens 20k adaptive; opencode clamp(2k..15k, 25%); pi 20k; claude project CLAUDE.md re-injected + skills re-attached (5k/25k budget)
- Zero-LLM methods: omp shake (elision to artifact://), snapcompact (bitmap PNG frames with per-model visual-token billing; foveation) — unique
- Speculative/async: omp pre-threshold speculation armed on branch snapshot, instant commit on cross, invalidated by prefix change — unique
- Split-turn double summaries (omp, pi); never cut at toolResult (omp/pi); condensation-as-event w/ forgotten-ids replay (openhands)
- Same-model condensing to avoid cross-provider format mismatch (roo) + 25%-truncate+retry fallback (roo)
Prompt caching ◆: Anthropic cache_control breakpoints (rolling tail window, TTL options), OpenAI prompt_cache_key + breakpoints, DeepSeek/Bedrock/Qwen variants, per-session cache keys (opencode sessionID; codex prompt_cache_key + previous_response_id resumption + WS prewarm + sticky routing), cache-hit-rate display (pi CH footer; kilo breakdown), cache-preserving design (date/cwd as per-request reminders not system prompt — omp; CLAUDE.md as user message — claude), cache-write suppression for one-off summary calls (pi)
Tool-output hygiene: truncation caps + spill files w/ retention GC (omp 50KB/20KB sink + artifacts; opencode 2000 lines/50KB + 7-day GC; claude 30k inline/64MiB read-back/5GB kill; goose 2000-line slots), pruning old tool outputs w/ protected recent window (omp 40k/20k; opencode 40k/20k), useless/superseded-result elision (omp), output priority filtering (goose ToolAnnotations priority), background summarization of old tool pairs (goose, omp)
Token budgeting: budget/usage states (codex TokenBudget/RolloutBudget across agent threads; goals budgetLimited), context metering UI ◆, cheap JSON-length estimation (opencode), sampled line-based estimation (aider every-100th-line), native tokenizers w/ fallback ◆
Context-window introspection tools: codex get_context_remaining/new_context; claude /context colored grid; omp getContextUsage
Repo maps & selective loading: aider PageRank token-budgeted map (×2 when no files); plandex architect-selected files from map w/ anti-hallucination + smart per-subtask windows; omp prewalk (strong model plans, cheap implements)
Model/effort routing on overflow: omp context promotion to larger-context sibling before compaction; roo largeContextFallback; plandex per-role largeContext/largeOutput/error fallbacks
Token frugality patterns: skills ~100-token metadata until triggered ◆; tool definitions deferred (claude ToolSearch + discovery cache, codex tool_search_tool, amp builtin-tools gating, goose available_tools, "<25 tools performs best"); Code Mode batching (goose/codex/omp workflowz)

## 5. Memory & knowledge
- Memory files: claude auto-memory MEMORY.md (200 lines/25KB, topic files, typed user/feedback/project/reference); openhands two-tier MEMORY.md w/ <UNTRUSTED_CONTENT> wrap; gemini Auto Memory (idle-session mining → reviewable patch inbox, nothing auto-applied); codex two-phase pipeline (rollout extraction → git-baselined consolidation agent → citations); omp backends (off/local pipeline/mnemopi SQLite+hindsight) w/ ≤5k-token injection; goose memory extension + chatrecall; cline Memory Bank (6-file hierarchy, deprecated by kilo in favor of AGENTS.md); devin Knowledge org bank w/ trigger retrieval
- Skills (agentskills.io standard — universal by 2026): SKILL.md + frontmatter (name/description/globs/allowed-tools/user-invocable/disable-model-invocation); progressive disclosure ◆; cross-harness discovery (.claude/.codex/.agents/.gemini dirs) ◆; skill:// URIs (omp); managed auto-learn skills (omp managed-skills, claude claude.ai sync)
- Cross-session search: omp history.db SQLite+FTS5; goose chatrecall + SQLite search; claude resume picker w/ preview
- Vector recall: roo Qdrant codebase indexing; goose vectors module; mnemopi embeddings (omp)
- Lessons/learning: omp learn tool + autolearn; gemini skill drafts from sessions; devin playbooks from sessions

## 6. Session model
- Append-only JSONL ◆ (omp/pi tree w/ id/parentId + leaf pointer; codex rollout-<ts>-<thread>.jsonl w/ SessionMeta first line + zstd cold compression + reverse scanner; claude ~/.claude/projects/<id>.jsonl)
- Tree semantics: omp/pi in-file branching + branch summaries + reset boundaries; opencode fork at messageID; codex fork w/ turn boundaries + rollback + revert + thread sections (pinned+custom); claude /branch (in-process) vs --fork-session vs /fork (background) vs fork subagents
- SQLite storage: goose sessions.db; crush per-project DB; opencode drizzle; codex state DB as rebuildable index over rollouts; omp history.db for prompt search only
- Resume/continue ◆: pickers w/ search/preview/rename (claude, gemini, omp, crush); terminal breadcrumbs per TTY/pane (omp — unique: per-pane continue, re-rooting vanished cwds); interrupted-turn recovery (omp synthetic aborted stopReason + session_exit records)
- Session naming: AI-generated titles via cheap model ◆ (claude Haiku, crush title sessions, goose, opencode, amp Luna); omp fixed-width 256-byte title slot avoiding rewrites
- Import/export: HTML export ◆ (omp w/ tool views, pi ANSI→HTML, claude /export); import from other agents (codex /import claude; goose imports claude/codex/pi jsonl; omp @claude/@codex; claude /import codex/gemini)
- Sharing: opencode opncd.ai links; claude artifacts pages; omp E2E-encrypted /share (AES-GCM, blob/gist, fragment key) + collab live replicas; goose Nostr NIP-44 encrypted session events (unique); amp thread sharing levels
- Multi-client attach: crush workspace model (live session mirroring); goose serve; codex /agents global browser across daemon
- Idle lifecycle: codex 30-min unload w/ SessionEnd hooks; omp subagents idle(7min TTL)→parked→revivable via hub message (unique)

## 7. Planning & task tracking
- Todo/phase tools ◆ (omp phase-based w/ single-active-task normalization; claude TaskCreate shared lists; openhands task_tracker; gemini write_todos + experimental DAG tracker w/ ASCII graph)
- Plan modes (read-only research + plan doc + approval handoff) ◆: claude plan mode + ExitPlanMode approval; gemini enter/exit_plan_mode + Pro-for-plan/Flash-for-implement routing; opencode plan agent + plan_exit synthetic continuation; crush/goose /plan + /endplan w/ history clear; droid Spec Mode + ExitSpecMode; zed plan profile; cline Plan/Act w/ per-mode models; omp plan mode w/ xd://propose + PlanYolo; amp (via mode presets); plandex (whole product is plan-first)
- Architect two-model split: aider architect(editor model) applies; plandex planner/coder/builder roles; omp prewalk strong→cheap at first edit; claude opusplan alias (plan on opus, execute on sonnet)
- Checkpoints/rewind ◆: claude per-prompt snapshots (100 retained, 30d) + /rewind partial restores (code/conversation/summarize-from-here); gemini shadow-git + tri-state rewind + re-proposes tool call; cline shadow-git (3 restore options); kilo git write-tree snapshots + patch records + Redo branching + hourly gc; opencode /undo //redo + per-message revert w/ snapshots; omp conversation-tree checkpoint/rewind (branch_summary pruning, no file restore); cursor local non-git checkpoints; zed checkpoints per edit
- Goal loops: claude /goal (keep working until condition); codex goals w/ auto-continuation + blocked/budgetLimited states; goose goals
- Deep-planning flows: cline /deep-planning 4-phase; amp mode presets; claude /deep-research workflow

## 8. Orchestration (subagents & parallelism)
- Subagents as tools ◆: task/agent/Task/invoke_agent/spawn_agent; markdown-defined agents w/ frontmatter (name/description/model/tools/spawns/output schema) ◆ across .omp/.claude/.gemini/.kilo/.agents/.factory/droids/.opencode
- Bundled agent fleets: omp scout/designer/reviewer/security-reviewer/librarian/task/sonic; claude Explore/Plan/general-purpose; opencode build/plan/general/explore/scout + hidden compaction/title/summary; crush coder/task; gemini built-ins + codebase_investigator; kilo general/explore; goose presets default/bash_runner/code_explorer/web_researcher; droid worker/explorer
- Read-only research subagents to protect main context ◆ (claude Explore skips CLAUDE.md+git status; roo boomerang summary-only returns)
- Parallel batch spawning: omp tasks[] array w/ isolation modes; claude agent teams (shared task list, SendMessage, TeammateIdle); goose summon delegate parallel + sub_recipes; codex multi_agent_v1/collaboration v2 (spawn edges persisted, parent-owned, concurrency limits)
- Background agents: claude /background + supervisor daemon + claude agents manager; codex app-server-daemon + /agents; amp orbs (remote VMs); opencode/gemini git worktrees per session
- Agent-as-tool interop: goose external subagents (Codex as MCP subagent) + ACP providers (claude-acp/codex-acp); openhands ACPAgent (Claude/Codex/Gemini as engine); codex Codex-as-MCP-server (thread/turn RPCs)
- Multi-agent messaging: omp hub (send/await/inbox/revive parked); claude SendMessage/ListAgents + notify_when_idle; codex send_message/interrupt_agent/followup_task; amp agent-spawned threads exchanging messages/files; goose orchestrator start_agent/send_message
- Scheduling: goose cron over recipes (manage_schedule from chat); claude CronCreate + /loop + ScheduleWakeup + Routines (cloud); amp self-waking schedules; cline cron; gemini GitHub Action cron; codex clock.sleep
- Orchestration-as-code: claude Workflow tool (LLM-written JS agent()/pipeline() scripts, pausable, savable); omp workflowz eval-kernel; goose recipes w/ retry-with-shell-checks + sub_recipes; devin playbooks; amp Puck meta-agent
- Isolation: git worktrees ◆ (claude --worktree w/ 4-layer enforcement, cursor, zed, gemini, opencode lifecycle, kilo Agent Manager); CoW filesystem clones (omp apfs/btrfs/zfs/overlayfs/reflink); Docker/VM (openhands DockerWorkspace ladder, goose --container, codex exec-server remote envs w/ Noise-relayed protobuf, amp orbs e2b, cursor Cloud Agents)
- Advisors/critics: omp advisor (second model reviewing deltas, nit/concern/blocker, WATCHDOG.md rosters); claude advisor tool; openhands critic mixin; codex Guardian (policy reviewer w/ risk levels + model rerouting); goose adversary.md natural-language policy reviewer

## 9. Extensibility
- MCP client ◆: stdio/http(s)/ws transports; OAuth (DCR + PKCE + pre-registered) ◆; tool namespacing mcp__server__tool ◆; per-tool approval ◆; resources + prompts-as-commands ◆; startup gates + deferred tools (omp 250ms gate; claude ToolSearch discovery cache; codex always-deferred); reconnect backoff + crash-storm breakers (omp); elicitation (codex, goose); MCP channels pushing server events into sessions (crush claude/channel, claude channels)
- MCP server mode (agent-as-tool-for-others): codex mcp-server, goose mcp <name>, goose itself over ACP, cline/codex ACP agents
- Hooks/event lifecycle ◆: claude 31 events × 5 handler types (command/http/mcp_tool/prompt/agent); codex Claude-style hooks w/ managed sources; crush PreToolUse (exit 2 block, exit 49 halt, JSON envelope, allow pre-approval); goose 13 events Open-Plugins spec; openhands 6 events Claude-compatible w/ 3 evaluator modes; gemini hooks w/ arg rewriting + synthetic responses; omp ~40 events + tool_call/tool_result middleware + context rewrite chain; amp plugin tool.call (allow/reject-and-continue/modify/synthesize)
- Plugins/packages ◆: claude marketplaces w/ version constraints; omp marketplace.json (Claude-registry compatible) + .omp-plugin; goose Open-Plugins + git install; gemini extensions (full distribution unit: MCP+commands+hooks+skills+agents+policies, migratedTo); pi packages (npm/git bundles w/ per-resource toggles); amp hosted personal/workspace plugin repos; plandex enterprise in-process hooks
- Custom tools ◆: TS/JS modules (pi/omp/amp/opencode), Python (openhands/gemini?), out-of-process discovery commands (gemini discoveryCommand/callCommand), executable Toolboxes (amp, removed), inline_python extensions (goose uvx)
- Slash commands ◆: markdown files w/ $ARGUMENTS/$N/!cmd shell injection/@file injection ◆; namespaces via subdirs; MCP prompts as commands ◆; TUI built-ins 30-60 per agent
- Skills standard cross-compat ◆ (.claude/.agents/.codex/.gemini/.omp dirs read by everyone)
- Custom agents as markdown ◆ (universal by 2026)
- System prompt customization: SYSTEM.md/APPEND_SYSTEM.md/PERSONALITY.md (pi/omp); output styles (claude); overridable prompt template files (goose prompts/ dir: system.md, plan.md, compaction.md...)
- Marketplaces: claude plugin marketplace + hints + relevance; omp; roo in-extension; gemini extensions directory; goose GitHub recipe repos; devin MCP marketplace
- LLM-driven config generation: `opencode agent create`, `kilo agent create`, crush self-configuring via crush-config skill, omp learn/manage_skill

## 10. Permissions & safety
- Approval modes ◆: default/ask → acceptEdits/auto_edit → yolo/bypass/danger-full-access (claude 6 modes incl. classifier-driven auto; gemini plan<default<autoEdit<yolo hierarchy; goose auto/approve/smart_approve/chat; codex untrusted/on-request/never; droid Off-Low-Med-High; plandex 5 autonomy levels × 10 toggles; crush a/s/esc; aider --yes; omp always-ask/write/yolo)
- Rule engines as data ◆: Tool(specifier) allow/ask/deny lists w/ path anchors/globs (claude, gemini TOML tiers w/ priority arithmetic + admin, omp tools.approval + bash.patterns compound-segment splitting, kilo per-tool allow/ask/deny w/ globs, goose Always/Ask/Never per tool, opencode granular per-input patterns last-match-wins)
- Bash command analysis: subcommand decomposition + wrapper stripping + redirection-as-write (claude); tree-sitter bash AST (opencode, gemini); prefix allow/deny + longest-prefix deny + substitution guards `${var@P}`/zsh `=(...)` (roo); Starlark execpolicy prefix_rule strictest-match (codex); wrapper-proof program resolution + unbypassable blocklist (droid — strongest)
- Model-classified risk: cline `requires_approval` per command; openhands inline `security_risk` tool-call argument (zero extra calls); goose smart_approve + adversary.md second-model reviewer; codex Guardian + server-side classifiers (claude auto mode w/ block/allow rulebook + verdict caching + 3-point subagent review); gemini Conseca LLM policy generator
- Sandboxing (OS-level): codex Seatbelt/bwrap(vendored)+landlock+seccomp+restricted-token matrix + network TCP→UDS proxy; claude Seatbelt/bubblewrap+socat+seccomp + credential masking; gemini 5 backends (Seatbelt profiles, Docker/Podman, icacls, gVisor, LXC) + tool-level sandboxing + expansion dialogs; goose --container; openhands workspace ladder incl. Apptainer/Sysbox; droid OS sandbox + Shield secret scanning; amp e2b orbs + OIDC workload identity
- Protected paths/critical-path circuit breakers: claude (.git/.claude/dotfiles never auto-approved; critical rm un-approvable); roo protected files; kilo AGENTS.md write-protected
- Secrets: env scanning + secrets.yml regex (omp reversible HMAC placeholders w/ case hints restored at tool boundary — unique); amp automatic deep redaction ([REDACTED:amp]); claude env scrubbing + headersHelper; gemini env sanitization for subprocesses + keychain; codex keyring w/ age/crypto_box/ML-KEM + RedactedString; goose OS keyring; pi !command/keychain resolution; crush $VAR/$(cmd) (1Password); `!cmd` secret indirection keeping keys out of files (omp, pi, crush)
- Project trust ◆: first-run trust dialogs gating project config/skills/hooks (pi trust.json, claude hasTrustDialogAccepted, codex project layers disabled when untrusted, gemini folder trust)
- Prompt-injection defenses: goose pattern + ML classifier; amp Parallel web-content defenses + defense-in-depth doc; claude posture page + auto-mode probe scanning tool results; openhands <UNTRUSTED_CONTENT> wrapping
- Extension supply-chain: goose malware scan + allowlist; gemini blockGitExtensions + allowedExtensions regex; pi exact-pinned deps + --ignore-scripts + lifecycle allowlist; codex admin install policies
- Enterprise lockdown ◆: managed settings (claude MDM + server-managed, codex requirements.toml, gemini admin policies, opencode MDM+remote .well-known, amp managed-settings + MCP registry allowlist fail-closed)

## 11. Model/provider abstraction
- Provider counts: omp ~75 (10 wire APIs), pi ~40 (4 wire APIs), goose 40+, aider/openhands via LiteLLM 100+, opencode 75+ via AI SDK + models.dev, crush via fantasy/catwalk, claude (Anthropic-only + Bedrock/Vertex/Foundry/gateways), codex (openai/bedrock/ollama/lmstudio + user-defined responses-only), gemini (Google-only + local Gemma router), plandex (11 + LiteLLM sidecar), kilo gateway 500+
- Wire protocol normalization: omp compat-flag system (~60 flags, whenThinking pointer-swap, tool-call dialects harmony/gemini/qwen3/deepseek/kimi/glm/gemma/hermes/minimax/xml/anthropic + StreamMarkupHealing + Harmony leak defense); goose toolshim for no-native-tools models; aider edit-format-per-model; opencode 65KB provider transform layer; claude/codex per-model prompt files
- Model catalogs as data: models.dev (opencode), catwalk community DB auto-updated (crush), codex models.json 414KB (context, efforts, tool modes, truncation, specialties), omp bundled catalog + fingerprinted SQLite cache, goose models-manager
- Roles/multi-model ◆: omp modelRoles (default/smol/slow/vision/plan/designer/commit/tiny/task/advisor + custom @roles); plandex 9 roles w/ per-role fallback chains; aider main/weak/editor; amp dial (agent+oracle pairs per mode) + per-task roles (search/titling/compaction/thread-reader); goose planner split; claude aliases + opusplan + advisor
- Thinking/reasoning control ◆: levels off→max (+xhigh/ultra), per-level token budgets, auto classifiers, magic keywords (ultrathink claude+omp, ultracode claude), effort→model-id routing for providers without wire fields (omp)
- Fallback chains: per-role/model/provider w/ cooldown revert (omp, claude --fallback-model, roo largeContext/largeOutput/error/strong, plandex) + quota-aware credential rotation (omp multi-account ranked)
- Local models ◆: Ollama/llama.cpp/LM Studio/vLLM discovery + KV-cache-preserving reasoning replay (omp replayReasoningContent); goose in-process llama.cpp/candle + HF model manager; pi /llama router; gemini LiteRT-LM local router classifying hosted routing; embedded tiny ONNX models for titles/memory (omp transformers.js)
- Auth ladders: omp 7-layer credential resolution + auth broker/gateway pair; codex ChatGPT OAuth + device code + Agent Identity JWTs + attestation; bedrock SigV4 chains (omp no-AWS-SDK WebCrypto impl); OAuth subscription reuse (Claude Code sub, ChatGPT/Codex, Copilot device flow) across cline/pi/omp/codex/amp
- Structured output ◆: JSON-schema-constrained final answers (codex --output-schema, opencode format json_schema w/ retry validation, omp outputSchema strict/permissive, claude --json-schema, pi constrainedSampling, plandex response schema)
- Stream-JSON wire compat: Claude-Code-compatible stream-json in/out (amp, omp --mode json, gemini --output-format stream-json, codex --json, goose run --output-format, cline NDJSON) — de-facto interop standard

## 12. Surfaces
- TUI ◆ (ratatui: codex, goose; bubbletea: crush, plandex; custom diff-rendered: pi/omp; Ink: gemini; OpenTUI/Solid: opencode; ratatui+pets: codex)
- Headless/print mode ◆ + JSON event streams ◆ + exit codes as API (claude 0/143, gemini 42/53)
- IDE: claude VS Code+JetBrains+desktop app; codex app-server VS Code w/ IDE context files; omp ACP; gemini ACP + companion ext; cline/kilo VS Code family; opencode auto-installing ext; crush none; zed Agent Panel w/ ACP; aider watch-files (IDE-less @AI comments — unique)
- SDK ◆: claude TS/Python (subprocess CLI driving); codex app-server SDKs w/ generated schemas; omp in-process + RPC + python omp-rpc; cline hub-spoke SDK; pi createAgentSession; goose uniffi Python/Kotlin + TS; openhands Python SDK
- Server/daemon: opencode serve (OpenAPI 3.1 + SSE); crush server (~60 endpoints + swagger) w/ workspace sharing; goose serve (ACP over WS+TLS+cert pinning); codex app-server(-daemon) + exec-server (remote envs, Noise relay); claude gateway/remote-control/self-hosted-runner
- Web/desktop/mobile: claude desktop+web cloud+mobile Remote Control+Slack+Chrome ext+channels (Telegram/Discord/iMessage); amp web+mobile+Slack+runners+orbs; kilo all surfaces w/ session sync; openhands Agent Canvas (Electron + embeddable library); goose Electron desktop (pure ACP client) + Telegram gateway; cline Kanban board
- Terminal multiplexer integration: omp herdr-style pane reporting is crush (herdr client); omp terminal breadcrumbs; goose term shell integration (@goose prefix + command-not-found handler — unique)
- CI/GitHub ◆: claude @claude + Actions + autofix; codex review in CI; opencode /oc comments; gemini Action + @gemini-cli; cline @cline; copilot coding agent (cloud PRs); droid-action code review w/ bug taxonomy

## 13. Project conventions
- AGENTS.md ◆ (universal by 2026; root→cwd concatenation, per-directory/subtree lazy loading — amp subtree-on-read, kilo per-directory system-reminders, gemini JIT dirs, codex root markers + 32KB cap, droid 80k/40k char budgets); fallbacks CLAUDE.md/GEMINI.md/.cursorrules ◆; @path imports ◆
- Rules systems: claude .claude/rules w/ paths globs + 1000-pattern budget; omp rulebook (always-apply vs rule:// on-demand vs TTSR stream rules); gemini policy TOML; kilo .kilo/rules-{mode}; roo mode-scoped rules
- Config hierarchy ◆: defaults < system/admin < user < project < env < CLI flags; arrays replace vs merge semantics documented (omp replace; opencode merge across 8 tiers); profiles (omp --profile, codex [profiles], goose CUSTOM_DISTROS)
- Ignore files: .gitignore respected ◆ + agent-specific ignores (.aiderignore, .plandexignore, .rooignore, .clineignore, .crushignore, .geminiignore)
- /init generators ◆ (claude reads Cursor/Copilot rules; gemini; crush; omp; goose)
- Cross-harness import/migration ◆: claude /import, codex /import, omp @claude/@codex resume, goose imports transcripts, crush copilot config import, kilo cursor/windsurf migration

## 14. Collaboration
- Share links (opencode, claude artifacts, amp levels, devin) ◆; E2E-encrypted (omp share + collab live replicas w/ write tokens; goose Nostr)
- Teams/orgs: claude teams + enterprise; amp workspaces (pooled credits, multiplayer orbs, sharing levels); plandex orgs w/ RBAC; openhands team backends; cline enterprise
- Review flows ◆: claude /code-review + ultrareview + ReportFindings; codex review (uncommitted/base/commit) + Guardian; amp Ship/Push-to-Branch/Custom Ship + Agentic Review; goose review w/ .agents/checks + severity models; droid CI review w/ taxonomy; kilo /review + inline line comments; claude PR review apps
- Real-time collab: omp collab (guest TUI replicas, host-authoritative, AES-GCM); amp multiplayer orbs; claude agent view + remote control; crush multi-client workspaces
- Remote execution: amp orbs+runners+portals; codex exec-server environments; cursor cloud agents; goose serve remote; claude cloud sessions + teleport

## 15. Cost/usage/telemetry
- Per-session token/cost accounting ◆ w/ cache read/write split; usage commands/dashboards (claude /usage /insights; amp usage --details; goose stats HTML; omp usage); cost display toggles; effort/quota-aware fallbacks ◆
- Rate-limit awareness: amp quota error classes w/ distinct backoffs; codex account rate limits + earned resets; gemini quota prompts to switch models; omp per-account rotation
- Telemetry: OTel (claude SDK, goose, opencode experimental, gemini off-by-default); anonymized stats opt-out ◆; offline modes (pi PI_OFFLINE, omp)
