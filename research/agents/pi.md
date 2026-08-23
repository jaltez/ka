# pi (badlogic/pi-mono) — Feature Extraction

> Source: research/repos/pi-mono clone (README, packages/coding-agent README + 34 docs, extensions/types.ts, tools/, agent-loop, providers/all.ts, tui/server/client/evals READMEs). MIT, TS monorepo. Scout: PiMono.

## Identity
- npm scope `@earendil-works/*`, site pi.dev. pnpm monorepo: `coding-agent` (CLI `pi`), `agent` (pi-agent-core), `ai` (pi-ai unified LLM API), `tui` (diff-rendered terminal UI), `telemetry`, `session-backends/sqlite-node`, `server`+`protocol`+`client` (experimental CBOR session server), `evals` (vitest-evals).
- Installs: npm, curl script, Bun-compiled standalone binaries. Runs on Node or Bun.
- **Footprint philosophy: minimal core, everything else is TS extensions/skills/prompts/themes/packages. Explicit non-features: no MCP, no sub-agents, no permission popups, no plan mode, no built-in todos, no background bash — all delegated to extensions.**

## Core loop & orchestration
- `agentLoop`: prompts → context → `streamFn` → event stream. Two-stage pipeline: `AgentMessage[]` → `transformContext()` (prune/inject) → `convertToLlm()` → provider `Message[]`; custom message types via declaration merging.
- Single flat loop, NO orchestrator/sub-agents in core. Events: `agent_start/end/settled`, `turn_*`, `message_*`, `tool_execution_*`.
- Tool execution: `parallel` (default; preflight sequential, execute concurrent, results persisted in source order) or `sequential` (global or per-tool).
- Hooks: `beforeToolCall` (block + `terminate:true`), `afterToolCall`, `shouldStopAfterTurn` (compaction between turns).
- **Steering vs follow-up queues** (signature): steering (Enter while streaming; delivered after current tool calls) vs follow-up (Alt+Enter; delivered when agent finishes); `one-at-a-time` or `all`; Escape aborts and restores queue to editor; `agent_settled`.
- Auto-retry: exponential 2s/4s/8s max 3; provider-level retries default 0 with 60s delay cap.
- `AgentSession` composes agent + session manager + resource loader + extension runner + model runtime.

## Tool catalog (7 built-ins; default active = first 4)
1. `read` — images, truncation, auto-resize to 2000×2000
2. `bash` — timeout, streamed output, truncation + full-output sidecar file, process-tree kill, detached-PID tracking, pluggable `BashOperations` (SSH/remote) + `BashSpawnHook`, PI_* session env
3. `edit` — string-replace w/ diff rendering, file-mutation queue serialization
4. `write`
5. `grep` (not default) 6. `find` (not default) 7. `ls` (not default)
- `--tools` allowlist, `--exclude-tools`, read-only preset `read,grep,find,ls`.
- Extension tools: `registerTool({name, parameters(TypeBox), execute, renderCall/renderResult, promptSnippet, promptGuidelines, constrainedSampling, prepareArguments, executionMode})`; can replace built-ins.

## Context management
- **Compaction** (auto, on): triggers on `contextTokens > contextWindow - reserveTokens` (16384) + overflow recovery (abort, compact, retry). `keepRecentTokens` 20000; never cuts at tool results; iterative structured LLM summary; `CompactionEntry{summary, firstKeptEntryId, details{readFiles, modifiedFiles}}`; split-turn double summary; `/compact [instructions]`; interceptable via `session_before_compact`.
- **Branch summarization**: `/tree` branch switch summary → `BranchSummaryEntry`.
- Context files: `AGENTS.md`/`CLAUDE.md` walk-up + per-dir `AGENTS.override.md`; `<project_context>` block; `SYSTEM.md`/`APPEND_SYSTEM.md`.
- Token footer: ↑↓ R W **CH cache-hit-rate** + cost + context %; `PI_CACHE_RETENTION=long`; compaction calls use fresh session IDs and disable cache writes.
- Skills = progressive disclosure (name+description only; SKILL.md via `read` on demand).

## Extensibility (the core differentiator)
- **TypeScript extensions**: default-export factory `(pi: ExtensionAPI)`. API: registerTool/Command/Shortcut/**Flag (custom CLI flags)**/MessageRenderer/MarkdownTransformer/EntryRenderer/Provider (incl. `streamSimple`); actions sendMessage/sendUserMessage/appendEntry/setSessionName/setLabel/exec/get-setActiveTools/setModel/setThinkingLevel; `events` bus; ctx: ui (dialogs, editor replacement, widgets, status line, overlays), sessionManager, model, signal.
- **~30 hook events**: project_trust, resources_discover, session_* (before_switch/fork/compact/compact_failed/tree/shutdown), `context` (rewrite pre-LLM), `before_provider_request`/`headers`/`after_provider_response`, `before_agent_start`, agent/turn/message/tool events, `tool_call`/`tool_result` (typed per tool), `model_select`, `thinking_level_select`, `user_bash`, `input`.
- **Skills**: agentskills.io standard; dirs `~/.pi/agent/skills/`, `~/.agents/skills/`, `.pi/skills/`, `.agents/skills/`; `/skill:name` w/ args; can point at `~/.claude/skills`/`~/.codex/skills`.
- **Prompt templates** (`{{arg}}`), **themes** (JSON, hot-reload), **keybindings.json**.
- **Pi Packages**: npm/git/ssh/https bundles of extensions/skills/prompts/themes (`pi install npm:@foo/pkg@ver`); per-resource enable/disable via `pi config`; conventional-dir auto-discovery.
- Built-in llama.cpp extension: `/login llama.cpp`, `/llama` download/load models.
- `/reload` hot-reloads everything without restart.
- 60+ example extensions: subagent, plan-mode, sandbox, gondolin, permission-gate, git-checkpoint, ssh, custom-compaction, custom-provider...

## Safety & permissions
- **No built-in permission system** — documented; runs with user perms. Sandboxing via extensions/docs: Gondolin micro-VM, Docker, OpenShell.
- **Project trust**: interactive prompt before loading project-local resources for untrusted dirs; `~/.pi/agent/trust.json`; ask/always/never; non-interactive never prompts.
- Secrets: `auth.json`; models.json values via `!command` (keychain/1Password), `$ENV` interpolation; `getApiKey` dynamic for expiring OAuth.
- Supply-chain: exact-pinned deps, `--ignore-scripts`, npm-shrinkwrap, lifecycle allowlist, lockfile block, npm audit, release smoke tests.
- Telemetry: version check + install ping + provider attribution headers; `--offline`/`PI_OFFLINE` kills all startup network.

## Model/provider abstraction
- `pi-ai`: 4 wire protocols (openai-completions, openai-responses, anthropic-messages, google-generative-ai); ~40 built-in providers: subscriptions w/ OAuth (Claude Pro/Max, ChatGPT/Codex, Copilot) + API-key (OpenAI, Azure, DeepSeek, Google, Vertex, Bedrock, Groq, Cerebras, xAI, Fireworks...) + gateways (OpenRouter, Cloudflare, Vercel, OpenCode Zen/Go) + Chinese plans (ZAI, Kimi, MiniMax, Moonshot, Xiaomi, Qwen) + local llama.cpp router.
- Generated model catalog per provider w/ auto-refresh; contextWindow, reasoning, modalities, cost incl. cache tiers → live cost tracking.
- Thinking levels off→max w/ per-level `thinkingBudgets` (compat `thinkingTokenBudgetField`).
- Custom providers: `~/.pi/agent/models.json` (Ollama/vLLM/LM Studio/proxies) w/ compat flags, `modelOverrides`, hot-reload.
- Transports: sse | websocket | websocket-cached | auto.

## Surfaces
- TUI (regular + experimental fullscreen); print `-p`; `--mode json` (event lines); `--mode rpc` (LF-only JSONL over stdio, id-correlated commands + streamed events); SDK (`createAgentSession`, in-memory sessions, multi-session runtime); experimental CBOR session server/client (Unix socket, leases, snapshots); `/export` HTML/JSONL + `/share` gist + HuggingFace publishing; evals harness (vitest-evals, comparative tables).

## Session & collaboration
- **Sessions = append-only JSONL trees** (id/parentId per entry; branch in-place in one file). Entry types: header, message, model_change, thinking_level_change, compaction, branch_summary, custom, custom_message, label, session_info.
- `/tree` navigator: search, fold, branch jump, filters (default/no-tools/user-only/labeled/all), labels, copy message; double-Escape opens.
- `/fork` (new file from prior user message), `/clone`, `--fork`, `/new`, `-c`, `-r`, `--session`. Optional SQLite+FTS5 backend package.

## Config & conventions
- `~/.pi/agent/settings.json` + `.pi/settings.json` (deep-merge); `/settings` editor. Settings span model/thinking/budgets, UI, trust, telemetry, compaction, branchSummary, retry, steering modes, transports, images, shell, defaultTools, enabledModels, markdown/mermaid, resources arrays w/ globs.
- Env: `PI_*` (agent dir, session dir, offline, telemetry, cache retention); bash children get `PI_SESSION_ID/FILE/PROVIDER/MODEL/REASONING_LEVEL`.
- Repo dogfoods itself (root `.pi/` + AGENTS.md).

## Distinctive features
- Self-extensible harness: TS extensions can replace everything (tools, compaction, editor, provider payloads, system prompt, trust flow, CLI flags) — subagents/plan-mode/MCP/permission-gates are example extensions, not core.
- In-place session tree (single JSONL).
- Steering vs follow-up dual queue with editor restore on abort.
- Pi Packages (npm/git) w/ per-resource toggles.
- Agent explains itself: system prompt embeds paths to own docs.
- llama.cpp router management (`/llama`).
- Session publishing to HuggingFace; `/share` gist HTML.
- 7-level thinking control w/ budgets; cache-hit-rate (CH) live in footer; cache-write suppression for one-off calls.
- `@`-file attach, image paste, `!`/`!!` bash passthrough, external editor.

## Canonical workflows
1. Quickstart: `pi` → `/login` → chat.
2. Interactive coding: 4 default tools; queue steering (Enter) / follow-up (Alt+Enter); footer metrics.
3. Model/thinking tuning: Ctrl+L/Ctrl+P/Shift+Tab; `pi --model sonnet:high`.
4. Read-only review: `pi --tools read,grep,find,ls -p "..."`.
5. Long session: auto-compaction; `/compact`; `/tree` to recover pre-compaction history.
6. Exploration: `/tree` jump → diverge; `/fork`/`/clone`; `/export`/`/share`.
7. Extend: drop extension/skill; `pi install`; `/reload`; `pi config`.
8. Headless/CI: `-p`; `--mode json`; `--mode rpc`; SDK; evals.
9. Local models: `/login llama.cpp` → `/llama`; models.json for Ollama.
10. Sandboxed deployment: Gondolin/Docker/OpenShell; project trust.

## Rust / low-footprint notes
- **Layered dependency graph (ai → agent → coding-agent; tui standalone) is a clean blueprint for a Rust workspace** (crates: provider layer, agent loop, harness, TUI).
- Only 7 built-in tools, all streaming w/ truncation + spill to file — bounded memory per tool call.
- TUI: differential line/viewport rendering, CSI 2026 synchronized updates, bracketed paste, cached rendered lines, zero heavy render stacks.
- Session persistence = append-only JSONL (no DB); tree via id/parentId; SQLite optional package.
- pi-ai lazy `.lazy.ts` module splits per provider + treeshake smoke entry — pay-only-for-what-you-import; TypeBox JSON-schema-compatible params.
- Process model: bash spawns detached (POSIX) w/ process-tree kill + tracked PIDs; timeouts via kill; OutputAccumulator.
- Supply-chain patterns: exact-pinned deps, no lifecycle scripts, offline mode, generated catalogs w/ manifests, retry split (agent vs provider) with server-delay cap.
- Extension model = in-process TS w/ typed event bus; Rust analog: WASM or subprocess RPC — pi's RPC/JSON/SDK modes show the seam for out-of-process extension hosts.
