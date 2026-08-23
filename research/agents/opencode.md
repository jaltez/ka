# OpenCode — Feature Extraction

> Source: research/repos/opencode clone (~40-package Bun monorepo; core, tui, web, app, desktop, sdk, plugin, codemode, slack, enterprise...). MIT. Scout: Opencode.

## Identity
- TypeScript on Bun; **headless core server + thin clients**. `packages/opencode` core (session loop, tools, providers, MCP, LSP, permissions); every client (TUI, web, desktop, IDE, SDK, ACP) talks to an OpenAPI 3.1 HTTP server (`opencode serve`, default :4096) over REST + SSE. Core being rewritten onto **Effect-TS** + **SQLite via drizzle**.
- Footprint: opposite of lean — bundles ~20 `@ai-sdk/*` provider packages, Effect, OpenTelemetry, tree-sitter WASM, MCP SDK, drizzle, solid-js.

## Core loop & orchestration
- Server-owned loop: prompt → per-agent/per-model tool resolution → streaming model call (AI SDK `streamText`) → permission-gated tool execution → repeat until stop / abort / `agent.steps` max-iteration limit forces text-only reply.
- Built-in agents: primary **build** (all tools), primary **plan** (read-only); subagents **general**, **explore** (read-only), **scout** (external docs/dependency research with managed repo cache); hidden system agents **compaction**, **title**, **summary**. Tab cycles primary agents; `@name` invokes subagents.
- Subagent depth configurable; `task` spawns child sessions with the subagent's permission ruleset.
- Experimental plan mode: `plan` agent + `plan_exit` tool asks user "switch to build agent?" and injects a synthetic user message to continue.
- Retry with exponential backoff + jitter (2s, ×2, max 5) honoring `retry-after`; context-overflow errors never retried — trigger auto-compaction.
- Event sourcing: `SyncEvent` aggregate log (single-writer, monotonic seq) for replayability/multi-device sync.

## Tool catalog (from src/tool/registry.ts)
1. `bash` — shell (tree-sitter bash parsing)
2. `read` — line ranges; warms LSP in background; injects `system-reminder` blocks
3. `glob` — pattern matching (mtime-sorted)
4. `grep` — regex search (ripgrep-backed)
5. `edit` — exact-string replacement (swapped out for gpt-* models)
6. `write` — create/overwrite
7. `apply_patch` — V4A-style patch; enabled ONLY for `gpt-*` models in place of edit/write
8. `task` — subagent child session
9. `webfetch` — fetch URL
10. `websearch` — Exa (only for opencode providers / env flag)
11. `todowrite` — todo tracking
12. `skill` — load SKILL.md into conversation; skill list embedded in tool description
13. `question` — ask user (app/cli/desktop clients only)
14. `invalid` — catch-all for malformed tool calls
15. `execute` — **CodeMode** (experimental): model writes a sandboxed JS program that may only call host-supplied tools (sequence/branch/loop/parallel; no fs/net/modules; call/time/output limits)
16. `lsp` — LSP queries (experimental): goToDefinition, findReferences, hover, symbols, call hierarchy
17. `plan_exit` — plan-mode handoff
18. MCP resource tools: `list_mcp_resources`, `list_mcp_resource_templates`, `read_mcp_resource` (10MB cap)
19. MCP server tools as `mcp_<server>_<tool>`

## Context management
- **Compaction**: auto on overflow or `/compact`; hidden compaction agent summarizes older turns; recent-tail budget = clamp(2k…15k, 25% of usable context); `compaction.tail_turns`; old tool outputs truncated to 2000 chars.
- **Prune**: walks backwards erasing tool-call outputs older than the last 40k protected tokens; fires when >20k prunable.
- Token estimation heuristic (`JSON.stringify` length — no tokenizer).
- **Tool output truncation**: 2000 lines / 50KB; oversize spills to managed dir (7-day retention GC); model gets preview + path.
- **System prompt variants** per model family: default, anthropic, gemini, gpt, codex, kimi, copilot-gpt-5, beast, trinity, meta.
- Usage/cost tracking per session incl. cache read/write tokens; `small_model` for titles.

## Extensibility
- **MCP**: stdio + remote (url/headers/OAuth with Dynamic Client Registration RFC 7591); tokens in `~/.local/share/opencode/mcp-auth.json`; `opencode mcp add/list/auth/logout/debug`; MCP catalog.
- **Plugins**: JS/TS in `.opencode/plugins/` or npm packages; hooks: `tool.execute.before/after`, `chat.message`, `chat.params`, `chat.headers`, `config`, `event`, `tool`, `auth`, `provider`, `shell.env`, TUI hooks, experimental `session.compacting`, `compaction.autocontinue`, `text.complete`, `workspace.register`.
- **Custom tools**: `.opencode/{tool,tools}/*.ts` + plugin-registered; Zod/JSON-schema args; deps via `.opencode/package.json`.
- **Skills**: `skills/<name>/SKILL.md` under `.opencode/`, `~/.config/opencode/`, `.claude/skills/`, `.agents/skills/`; loaded on demand via `skill` tool; per-skill wildcard permissions.
- **Slash commands**: markdown in `.opencode/commands/` + JSON; `$ARGUMENTS`, `$1..$n`, shell injection `` !`cmd` ``, `@file` refs; per-command agent/model/subtask overrides.
- **Agents**: JSON or markdown in `.opencode/agents/`; fields: description, mode primary/subagent/all, model, variant, prompt, temperature, steps, permission, disable; `opencode agent create` (LLM-generated agent config as structured JSON).
- **LSP servers** (35+ built-in definitions, several auto-download, disabled by default) + **formatters** (~25 built-ins, auto-run on write/edit).

## Safety & permissions
- `permission`: per-tool `allow|ask|deny`; object syntax = granular pattern rules per tool input (bash matches parsed command, edit matches path, webfetch matches URL, task matches subagent name); **last matching rule wins**.
- Special guards: `external_directory` (paths outside CWD) and **`doom_loop`** (same tool+input 3× → ask).
- Defaults: everything allow; `*.env` reads denied; question/plan tools denied for subagents.
- Ask UI: once / always (session-scoped pattern allowlist) / reject; `--auto` auto-approves non-deny.
- Secrets: `~/.local/share/opencode/auth.json` (`/connect`); `{env:VAR}` substitution.
- Org control: remote `.well-known/opencode` defaults, managed config dirs (`/etc/opencode`...), macOS MDM `.mobileconfig` — highest priority; `share: disabled` enforcement; `experimental.policies` allow/deny providers.
- No process sandbox — bash runs natively; sandboxing is purely permission-gating.

## Model/provider abstraction
- Vercel **AI SDK** + **models.dev** registry → 75+ providers (API url, npm SDK package, cost incl. cache tiers, limits, capabilities). Missing SDKs dynamically npm-installed at runtime.
- Auth: API keys / OAuth (Copilot, OpenAI); AWS Bedrock full credential chain; Vertex ADC; Cloudflare AI Gateway.
- Config: `provider.<id>.options.baseURL`, per-model `options` (reasoningEffort, thinking budgetTokens...), `blacklist`/`whitelist`, custom providers.
- **Variants**: built-in reasoning-effort variants (anthropic high/max; openai none…xhigh; google low/high) + user-defined; `ctrl+t` cycles.
- **Prompt caching**: provider-specific cache-control injection (anthropic/openrouter/alibaba ephemeral, bedrock cachePoint, copilot); promptCacheKey = sessionID.
- Provider quirks engine (65KB transform.ts): per-model schema transforms, providerOptions mapping, image normalization (>2000×2000 or >5MB resized), error normalization.
- First-party gateways: **OpenCode Zen** (curated, pay-per-use) and **OpenCode Go** (subscription open models); local models via OpenAI-compatible endpoints.

## Surfaces
- **TUI**: OpenTUI/Solid; leader `ctrl+x`; themes, full keybind remap (`tui.json`), `@` fuzzy file+reference autocomplete, `!cmd` passthrough, `/undo` `/redo`, thinking-visibility, attention system (notifications + sound packs), session sidebar & child-session navigation.
- **Web**: `opencode web` browser app; basic-auth; `opencode attach <url>` (TUI on remote backend).
- **Desktop**: Electron beta.
- **IDE**: auto-installing VS Code-family extension; `@File#L37-42` refs.
- **ACP**: `opencode acp` (Zed, JetBrains, neovim clients).
- **Server/SDK**: OpenAPI 3.1 at `/doc`; SSE; JS+Python SDKs; **structured output** via `format: {type:"json_schema"}` → hidden `StructuredOutput` tool with validation retries.
- **CI**: GitHub agent (`/opencode` or `/oc` mentions, schedule, workflow_dispatch) on org runners → branch/PR. **Slack bot**.

## Session & collaboration
- SQLite per project; resume (`-c`), specific (`-s`), **fork** (`--fork` at messageID), `/sessions` picker.
- Parent/child session trees; navigation keybinds.
- **Undo/redo**: removes last message + reverts file changes via snapshots/git; **revert/unrevert** per message (snapshot + diff stored).
- **Share**: public `opncd.ai/s/<id>`; enterprise restrict/disable.
- Auto titles, summaries, per-session diff, cost accounting. `/init` writes/updates AGENTS.md.
- Event-sourced sync for multi-device replay.

## Config & conventions
- `opencode.json/.jsonc` merged across 8 precedence tiers: remote `.well-known/opencode` → global → `OPENCODE_CONFIG` → project root walk-up → `.opencode` dirs → inline → managed dirs → MDM.
- Rules: `AGENTS.md` (project walk-up + global), CLAUDE.md fallback; `instructions: [paths, globs, URLs]`; `{env:}`/`{file:}` substitution.
- Keys: model, default_agent, subagent_depth, share, command, agent, permission, provider, mcp, lsp, formatter, snapshot (default on), autoupdate, tool_output, compaction, references...

## Distinctive features
1. **CodeMode** (`execute`): sandboxed JS program orchestrating tool calls with no ambient authority — collapses multi-turn tool chatter.
2. **Model-dependent tool swap**: `apply_patch` vs `edit`/`write` by model family.
3. **References**: config-aliased external dirs AND git repos (cached clones, async refresh) in `@` autocomplete + agent context.
4. **Git worktree lifecycle** (create/remove/reset with per-worktree start commands).
5. Per-model-family system prompt corpus.
6. **Doom-loop guard** and `external_directory` as first-class permissions; spill-to-file truncation with retention GC.
7. **Managed/MDM config + remote org config** enforcement tiers.
8. First-party model gateway (Zen/Go); free models.
9. Full surface matrix from one server: TUI + web + desktop + IDE + ACP + SDK + GitHub + Slack.
10. LLM-driven agent generation.
11. Attention/sound system with per-event sound packs; mDNS discovery.

## Canonical workflows
1. TUI coding loop: prompt (`@file`, `!cmd`, `/command`) → build agent → `/undo` → `/sessions` resume.
2. Plan-first: Tab to `plan` → plan in `.opencode/plans/*.md` → `plan_exit` handoff → build continues.
3. Delegate: `@general` / automatic `task` spawn → child session → summarized result; `@explore`, `@scout`.
4. Long-session survival: overflow → auto-compaction; oversize tool output spilled.
5. Share & review: `/share` → public link → `/unshare`.
6. Headless: `opencode run --auto -m provider/model "task"`; GitHub `/oc`.
7. Programmatic: SDK `createOpencode()` → structured output; SSE streaming.
8. Extend: plugins, MCP, skills, agents/commands as markdown.
9. Multi-context: `references` for upstream repos.
10. Parallel worktrees per task.

## Rust / low-footprint notes
- Maximal-footprint reference — feature checklist rather than footprint model: one Bun process hosting server+TUI; Effect DI; SQLite persistence; ULID ascending IDs for cheap ordering.
- Lean tricks worth stealing: (a) lazy tool loading; (b) bundled-vs-dynamic provider SDK loading; (c) cheap token estimation via JSON length for compaction budgets; (d) truncation-spill files w/ GC instead of big outputs in context; (e) single in-flight prewarm+boot promise in `opencode run`; (f) LSP warm-on-read forked; (g) per-project caching of resolved registries; (h) ripgrep subprocess; web-tree-sitter WASM.
- Process model: server owns sessions; TUI is just an OpenAPI client (attachable to remote backends); SSE events.
- Perf: prompt-cache keying by sessionID; provider-specific cache_control injection; per-session cost accounting.
