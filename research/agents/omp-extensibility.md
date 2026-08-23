# omp (extensibility) — Feature Extraction

> Source: omp internal docs (extensions.md, extension-loading.md, skills.md, skills/authoring-*.md, hooks.md, custom-tools.md, slash-command-internals.md, mcp-*.md, marketplace.md, rpc.md, sdk.md, magic-keywords.md, rulebook-matching-pipeline.md, system-prompt-customization.md, task-agent-discovery.md, agent-hub.md). Scout: OmpExt.

## Identity
- omp / "oh-my-pi" coding agent, monorepo (`packages/coding-agent`, `packages/agent`, `packages/tui`, `packages/utils`, `python/omp-rpc`). TypeScript on Bun. Extensibility lives in `src/extensibility/`, `src/discovery/` (capability providers), `src/capability/`, `src/mcp/`, `src/task/`, `src/modes/rpc/`, `src/sdk.ts`.
- Extensions run **in-process, no sandbox/isolation**; one shared EventBus + ExtensionRuntime per session; subagents get their own runner in the same process.

## Core loop & orchestration
- Prompt pipeline order in `AgentSession.prompt()`: built-in registry (TUI/ACP) → extension commands → TS custom + MCP prompt commands → file-based slash commands (`/x`) → prompt templates → delivery (idle: send; streaming: `steer`|`followUp` via `streamingBehavior`).
- Queue modes: `steeringMode`/`followUpMode` = `one-at-a-time`(default)|`all`; `interruptMode` = `immediate`(default, checks steering between tool calls, can abort remaining tools)|`wait`.
- Extension lifecycle: import module + run factory (**registration only**) → `ExtensionRunner.initialize` wires live actions → lifecycle events → every tool execution wrapped with `tool_call`/`tool_result` interception.
- Turn events: `input` → `before_agent_start` → `before_provider_request` (can replace provider payload) → `after_provider_response` → `agent_start/end` → `turn_start/end` → `message_start/update/end` → `session_stop` (awaited stop hook; `{continue:true, additionalContext}` capped at 8 consecutive continuations).
- `agent_end.isTerminal !== false` = completion.

## Extension API surface (tools of extensibility)
1. `pi.registerTool` — LLM tool w/ schema, `hidden`, `defaultInactive`, `loadMode` (`essential`|`discoverable`), `deferrable`, `approval` (`read`|`write`|`exec`), `strict`, `renderCall`/`renderResult`.
2. `pi.registerCommand` — slash command with session control context (`waitForIdle`, `newSession`, `switchSession`, `branch`, `navigateTree`, `reload`, `compact`).
3. `pi.registerShortcut` / `pi.registerFlag` — keybindings + CLI flags (reserved shortcuts ignored).
4. `pi.registerProvider(name, config)` / `pi.unregisterProvider` — add providers incl. `UsageProvider.fetchUsage`.
5. `pi.registerFileWriteFallback` / `pi.registerFileDeleteFallback` — privileged-broker seams for EPERM/EACCES/EROFS byte-writes/unlinks.
6. `pi.registerMessageRenderer(customType, fn)` / `registerAssistantThinkingRenderer` / `registerComposerShape` — TUI render points.
7. `pi.on(event, handler)` — ~40 events.
8. `pi.sendMessage/sendUserMessage/appendEntry/exec` — message injection (`deliverAs: steer|followUp|nextTurn`), durable custom session entries, shell exec.
9. `pi.setActiveTools/getActiveTools/getAllTools` — runtime tool-set mutation; system prompt rebuilt on change.
10. Model controls: `setModel`, `get/setThinkingLevel`, `get/setServiceTier` (per-family tier map).
11. `ctx.invokeTool` — tool re-registering a built-in name can delegate to the native built-in (same-tool only, no re-approval, recursion-guarded).
12. Canonical essential built-ins: `read`, `write`, `bash`, `edit`, `glob`, `computer`, `eval`, `task`, `hub`, `learn`, `manage_skill`; hidden `yield`.
13. MCP tools surface as `mcp__<server>_<tool>` (sanitized, deterministic lexicographic-origin winner on collision).
14. RPC host tools (`set_host_tools`) + host URI schemes (`set_host_uri_schemes`, virtual `scheme://` files; `security://` reserved).

## Event catalog
- Session: `session_start`, `session_before_switch/switch` (cancelable), `session_before_branch/branch`, `session_before_compact`/`session.compacting`/`session_compact`, `session_before_tree/tree`, `session_shutdown`.
- Prompt/turn: `input`, `before_agent_start`, `before_provider_request`, `after_provider_response`, `context` (rewrite message list before each LLM call, chained), `agent_start/end`, `session_stop`, `turn_start/end`, `message_start/update/end`.
- Tool: `tool_call` (block / revise `input` — revision revalidated), `tool_result` (middleware chain, patch content/details/isError), `tool_execution_start/update/end`, `tool_approval_requested/resolved`.
- Signals: `auto_compaction_*`, `auto_retry_*`, `ttsr_triggered`, `todo_reminder`, `goal_updated`, `credential_disabled`.
- MCP: `mcp_notification` (bounded FIFO buffer cap 100 drop-oldest).
- User-command interception: `user_bash`, `user_python` (override `{result}`).
- Ordering: handlers run in registration order; `tool_call` first block short-circuits, last non-block wins; `tool_result` last override wins; `context` chained; handler throws → extension-error event (except `tool_call` = fail-closed block).

## Loading & discovery (capability-provider registry)
- Load order: native auto-discovered `.omp/extensions` (project + user) → discovered JS/TS hook factories → installed-plugin manifest entries → explicit CLI (`--extension`) + `extensions:` settings. Dedup by absolute path, first wins.
- Priority-sorted capability providers: native(100) > omp-plugins(90) > claude(80) > claude-plugins/agents/codex(70) > opencode(55) > github(30) > builtin-defaults(1); name-keyed first-wins dedup, `_shadowed` duplicates, universal `disabledExtensions` id namespace (`extension-module:`/`skill:`/`context-file:`).
- Cross-harness import matrix: skills/commands/rules/MCP/custom-tools from Claude, Claude-plugins, Codex, Agents(.agent/.agents), OpenCode, Cursor, Windsurf, Cline, GitHub. Task agents deliberately NOT imported from `.claude/.codex/.gemini/agents`.

## Skills
- Layout `<skills-root>/<name>/SKILL.md`, non-recursive; frontmatter `name/description/globs/alwaysApply/hide/disableModelInvocation`.
- `skill://<name>` → SKILL.md; `skill://<name>/<rel>` → in-skill asset; traversal/absolute rejected.
- `/skill:<name>` commands; embedded-token recognition in prose; delivery mode = submission keybinding (Enter→steer, Ctrl+Enter→followUp).
- Managed auto-learn skills at `~/.omp/agent/managed-skills` (always defers to authored same-name).

## Slash commands (files)
- Roots incl. `.omp/commands`, `~/.omp/agent/commands`, `~/.claude/commands/**` (recursive + `foo:bar` namespace aliases), `~/.codex/commands`, `.opencode/commands`, plugin `commands/*.md` prefixed `<plugin>:<cmd>`, `.agent[s]/commands` walking to repo root.
- Expansion: `$1..$n` positionals, `$@[start:len]` slices, `$ARGUMENTS`/`$@`, `prompt.render` templating, quote-aware arg parser. Unknown `/x` falls through to the LLM as literal text.

## MCP
- Config: `.omp/mcp.json` (project), `~/.omp/agent/mcp.json` (user/profile); imports Claude/Codex/Toml `[mcp_servers]`/Gemini/OpenCode/Cursor/Windsurf/VSCode/plugin configs. First definition wins, no merge.
- Transports: `stdio` (JSONL), `http` (Streamable HTTP, Mcp-Session-Id, per-request SSE + optional background GET SSE listener), `sse` (legacy). Protocol version `2025-11-25`; answers server→client `ping`/`roots/list`.
- Secrets: `${VAR}`/`${VAR:-default}` expansion; pre-connect `!command` (10s timeout, cached) / env-var-name / literal resolution for env+headers; OAuth managed credentials keyed per profile+URL.
- Overrides: user `disabledServers` (wins) / `enabledServers` allowlist; `mcp.enableProjectConfig:false`; `OMP_MCP_TIMEOUT_MS`.
- Runtime: 250ms fast-startup gate → live tools / per-server errors / cached `DeferredMCPTool`s; slow servers register late via `#onToolsChanged` (never block startup). Auto-reconnect with backoff 500/1000/2000/4000ms; crash-storm breaker >5 reconnects/30s. `/mcp add|remove|enable|disable|test|reauth|unauth|reconnect|reload|resources|prompts|notifications`, Smithery search/login.

## Marketplaces & plugins
- Catalog `.omp-plugin/marketplace.json` (Claude-registry compatible fallback `.claude-plugin/marketplace.json`). Sources: relative path, git URL (+ref/sha), github shorthand, git-subdir, npm (parsed, install rejected).
- Plugin content: `skills/`, `commands/*.md`, `agents/*.md`, `hooks/pre|post/`, `tools/`, `.mcp.json`, `lspServers`→`.lsp.json`, `dapAdapters`→`.dap.json`, `package.json#omp.extensions`.
- Scopes user vs project; `/marketplace add|remove|update|list|discover|install|uninstall|installed|upgrade`, `/plugins list|enable|disable`, `omp plugin *` CLI. `marketplace.autoUpdate`: off|notify(default)|auto.

## Subagents & Agent Hub
- `AgentDefinition` frontmatter: `name/description/systemPrompt` required; `tools`, `spawns`, prioritized `model` list (role aliases `@role`), `thinkingLevel`, `output` schema, `blocking`, `autoloadSkills`, `readSummarize:false`, `prewalk` (hand off to smol-role at first edit/write), `advisor` (bool/model[:level]).
- Discovery precedence: project `.omp/agents` > user > extension-package roots > Claude marketplace plugins (gated) > bundled (`scout`, `designer`, `reviewer`, `security-reviewer`, `librarian`, `task`, `sonic`).
- Guards: `task.disabledAgents`, parent spawn policy, `PI_BLOCKED_AGENT` self-recursion env, `task.maxRecursionDepth` (default 2), plan-mode effective agent (read/grep/glob/web_search only).
- Agent Hub (Alt+A): live roster (status/model/age/task/cost/tokens/IRC unread), flat or parent-child tree, inspector (current tool+args, retry, context-window use, lineage, output/patch paths, worktree branch), `r` revive parked, `x` abort+kill; focus subagent in main TUI.

## RPC & SDK
- RPC (`omp --mode rpc`): JSONL over stdio; protocol v1 (1MiB frames) / v2 (negotiated lossless `rpc_chunk` base64 reassembly to 64MiB). ~50 commands (prompt/steer/follow_up/abort, get_state, set_todos, set_host_tools, set_host_uri_schemes, subagent streaming, session ops, paged `get_messages_page` 256/page with `session_busy`/`stale_cursor` codes, OAuth login). TS `RpcClient` + Python `omp-rpc`.
- SDK: `createAgentSession()` — provide-to-override, omit-to-discover; `restrictToolNames` + `toolNames` allowlist; inline extensions; `beginDispose()` sync admission barrier → idempotent `dispose()`. Startup perf: `fetch.preconnect(model.baseUrl)` (100–300ms), conditional LSP warmup.

## Rules / system prompt / magic keywords
- Rules from native `.omp/rules/*.{md,mdc}` + `RULES.md` + omp-plugins + `.agent[s]/rules` + `.cursor/rules/*.mdc` + Windsurf + `.clinerules` + `.github/instructions/*.instructions.md`. Three buckets: TTSR (regex/ast-grep conditions scoped to text/thinking/tool streams, interrupt modes) > always-apply > rulebook (name+description listed, `rule://` on-demand).
- System prompt: `SYSTEM.md` (template switch), `APPEND_SYSTEM.md`, `TITLE_SYSTEM.md`, `PERSONALITY.md`. Plain-text contract. Prefix-cache-preserving: date+CWD live in per-request `<system-reminder>` on first user turn.
- Magic keywords: `ultrathink` (max reasoning), `orchestrate` (multi-agent delegation), `workflowz` (deterministic eval-kernel agent()/parallel()/pipeline()/completion()).

## Safety & permissions
- `tool_call` fail-closed; tool `approval` classification (`read`/`write`/`exec`); file write/delete fallback seams (symlink-attack-aware); contained timers (raw `setInterval`/detached-promise throws are process-fatal); MCP committed config trust model + `!cmd` secret indirection; reserved shortcuts cannot be overridden.

## Distinctive features
- Single unified in-process `ExtensionAPI` superset (events+tools+commands+renderers+providers+UI+session control) — no plugin sandbox, zero IPC cost.
- File write/delete fallback seams for hosts embedding the agent in write-denying sandboxes with privileged broker channels.
- TTSR (Time Traveling Stream Rules): regex/ast-grep conditions scoped to text/thinking/tool-arg streams with per-rule interrupt modes.
- Magic keywords with anti-false-positive token matching.
- Internal URL protocols as extensibility: `skill://`, `rule://`, `local://`, `xd://`, `history://`, `agent://`, host-registered schemes.
- RPC v2 lossless chunked framing + host-owned tools and host-owned virtual URI schemes.
- MCP hybrid fast-start (250ms gate + cached deferred tools + late registration), crash-storm circuit breaker, per-profile URL-keyed OAuth.
- Agent Hub: revive/steer/kill parked subagents; advisor observability.

## Canonical workflows
1. Safety policy extension: `guardrails.ts` in `~/.omp/agent/extensions/` → block/redact/monitor.
2. Marketplace plugin install → ships skills/commands/agents/hooks/.mcp.json → `/reload-plugins`.
3. Add MCP server via `/mcp add` or `.omp/mcp.json` → secret indirection → OAuth per profile.
4. Custom subagent fleet: `.omp/agents/reviewer.md` with `model: @review` + `modelRoles.review`.
5. Embed via RPC: `omp --mode rpc` → v2 → host tools + host URI schemes.
6. Embed via SDK: `createAgentSession({sessionManager: SessionManager.inMemory(), toolNames})`.
7. Author a skill → listed in system prompt → model reads `skill://name/...`.
8. Prompt/context governance via SYSTEM.md/APPEND_SYSTEM.md/rules/TTSR.
9. Push-capable MCP integration via `mcp_notification` → mid-turn steer.
10. Sandboxed host embedding via registerFileWriteFallback/DeleteFallback.

## Rust / low-footprint notes
- Two-phase extension model (registration object → runtime handle) maps to a Rust trait; catch_unwind or task supervision per handler replaces sandbox.
- Capability-provider registry: Vec of providers sorted by priority, HashMap<name, item> first-wins — allocation-light, directly portable.
- MCP client: pending-request map, never-await stdin writes, drop-malformed-and-continue, 250ms startup gate + deferred tools + late registration; reconnect backoff table + crash-storm breaker.
- Caching tricks: `?mtime` cache-busting, preconnect warming, per-request date/cwd reminders for prefix-cache, bounded notification FIFO.
- RPC: line-delimited JSON, id-correlation, chunk reassembly, paged history with machine-readable error codes instead of huge frames.
- Heavy parts are breadth of discovery importers and TUI renderer contracts — a lean Rust agent can keep capability registry + extension events + MCP manager + task spawn policy and drop surface area per target.
