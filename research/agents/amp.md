# Amp (Sourcegraph / Amp Frontier Corporation) — Feature Extraction

> Source: ampcode.com/manual (all subpages), /security, /modes, /pricing, news RSS. Closed-source, docs-only. Scout: Amp.

## Identity
- Amp, "the frontier agent" — closed-source. Built by Sourcegraph team; spinning out as Amp Frontier Corporation.
- Distribution: Amp CLI as a single-file executable compiled by Bun. Install: curl script, Homebrew, npm, PowerShell. `amp update` auto-update from static CDN.
- **Two-component architecture**: **Amp Client** (CLI: local code/context collection, local settings, local thread history, executes tools) + **Amp Server** (ampcode.com: auth, accounts, workspaces, thread sync/storage in PostgreSQL, usage tracking; server-side "thread actors" run the agent loop and connect to LLM providers with request-scoped credentials). No self-hosting.
- SDKs: `@ampcode/sdk` (TS) and `amp-sdk` (Python) — both drive the installed CLI.
- SOC 2 Type II, pentest, bug bounty. Credentials at `~/.local/share/amp/secrets.json`; MCP OAuth tokens `~/.amp/oauth/`.

## Core loop & orchestration
- Server-driven loop: client collects context → server runs agent turn → client executes tool calls and returns results.
- **Modes = capability presets, not model selectors**: `low`/`medium` (default)/`high`/`ultra` ("the Dial"). Ctrl+O palette. `--fast` per-invocation.
- Parallelism: automatic **subagents** (own context window), agent-spawned **other threads** (locally, in orbs, or on remote runners; agents exchange messages and files), fan-out prompts.
- **Schedules**: agents set their own wake-up schedules and resume with full context.
- Steering: message queueing while agent works; `Enter Enter` sends at next step boundary; `Esc Esc` interrupts; `steer: true` flag in stream-JSON input.
- Puck (meta-agent) spawns/coordinates agents; plugins can create threads/agents; `agent.end` plugin event can auto-continue turns.

## Tool catalog
1. `Bash` — shell (local or orb).
2. `Read` — read files.
3. `create_file` — create files.
4. `edit_file` — edit files.
5. `undo_edit` — undo an edit (checkpoint/rollback).
6. `glob` — file globbing.
7. `Grep` — content search.
8. `finder` — codebase-search subagent (GPT-5.6 Terra).
9. `Task` — spawn subagent task.
10. `todo_read` / `todo_write` — agent-managed TODO list.
11. `oracle` — second-opinion strong model consultation.
12. `web_search` — web search (Parallel).
13. `read_web_page` — web page retrieval (Parallel).
14. `read_mcp_resource` — read MCP resources.
15. `librarian` — subagent searching all public GitHub + private repos.
16. `painter` — image generation/editing (GPT Image 2).
17. `view_media` — analysis of images/PDFs/audio/video.
18. `read_thread` — read/summarize other Amp threads.
19. Thread-creation tool for agent→agent spawning.
20. `reload_skills` — rescan local skill dirs + fetch personal/workspace skill repos.
21. `mcp__<server>__<tool>` — MCP tools.
- `amp tools list`; disable via `amp.tools.disable` (glob patterns).

## Context management
- **AGENTS.md hierarchy**: cwd + parents (up to $HOME) always included; subtree files included when agent reads a file in that subtree; user/system-wide paths. Falls back to `AGENT.md`/`CLAUDE.md`. `agents-md list` palette. Auto-generation offer.
- **@-mentions inside AGENTS.md**: include other files (`@doc/style.md`, globs), relative to mentioning file; YAML frontmatter `globs:` in mentioned files → context loaded only when agent reads a matching file.
- **Compaction** (context summarization for long threads). History: compaction removed → "Handoff" → removed; today recommended pattern is short threads + new thread + mention original.
- **Thread referencing**: `@T-<id>` or URL; `@@` picker; read_thread extracts relevant info. Feed query syntax (`id:`, `label:`, `file:`, `repo:`, `after:`...).
- Subagents isolate verbose output; skills lazy-load; MCP-in-skills tools hidden until skill loads (explicit tool-context budgeting: "too many tools reduces model performance").
- Images: clipboard paste, @-mention paths; limits 4.9MB decoded / 8000px.

## Extensibility
- **Plugins** = TS/JS modules, in-process (Bun). Events: `session.start`, `agent.start`, `tool.call` (`allow`/`reject-and-continue`/`modify`/`synthesize`), `tool.result`, `agent.end` (`continue` + follow-up). `amp.registerTool`, `amp.registerCommand` (palette), `amp.registerSkill`, `amp.createAgent` + `amp.registerAgentMode`, `amp.getBuiltinAgent(mode).run()/createThread({executor})`, `amp.ai.ask()` classifier, `ctx.ui.notify/confirm/input/select`, `ctx.thread.append()`, `amp.$` shell, helpers, status-bar items, `amp.createWebhook`, `amp.onDispose`. Plugin API explicitly "inspired by pi's extension API".
- **Plugin locations/precedence**: project `.amp/plugins/` > system `~/.config/amp/plugins/` > personal (Amp-hosted global repo) > workspace (admin-pushed repo). Live reload in-thread.
- **Skills**: `SKILL.md` dirs (frontmatter `name`, `description`; optional `mcpServers`, `builtin-tools`). Source precedence: `~/.config/agents/skills/` > `~/.agents/skills/` > `~/.config/amp/skills/` > project `.agents/skills/` > `.claude/skills/` > plugin cache > `amp.skills.path` > built-in > personal repo > workspace repo. Personal/workspace skills stored in Amp-hosted Git repos, shareable via URL.
- **MCP**: local (`command`/`args`/`env`) and remote (`url`/`headers`); streamable-HTTP w/ SSE fallback; `$VAR` expansion; `amp mcp add/doctor/approve/oauth`; auto-OAuth + manual registration; `--mcp-config`; skill-bundled `mcp.json` with `includeTools` glob filtering.
- **Mode plugins**: official experimental agent modes as plugins (`@amp/glm-52-mode`, etc.).
- **SDK** (TS/Python): `execute()` streaming; thread continuity, cwd, settings, MCP per-session, custom skills.
- Historical: "Toolboxes" (executable files as tools) and Amp Tab removed.

## Safety & permissions
- **No approval prompts by default** — signature stance. Permissioning opt-in via plugins (`tool.call` hooks) or legacy config.
- Legacy: `amp.permissions`, `amp.guardedFiles.allowlist`, `amp.dangerouslyAllowAll`.
- `amp.mcpPermissions`: ordered first-match allow/reject rules with globs; can block all MCP.
- **Workspace MCP server trust**: servers in `.amp/settings.json` require explicit approval (`amp mcp approve`); enterprise **MCP registry allowlist** (package-name matching, fail-closed).
- **Automatic secret redaction** at the lowest system level: detected secrets never visible to LLM/cache/providers/server; `[REDACTED:amp]` markers. Covers AWS/GCP/Azure/GitHub/GitLab/OpenAI/Anthropic/Stripe/Slack/npm/generic keys.
- Client avoids reading `.env`; token revoke endpoint; passkey auth; SSO (SAML) + SCIM; audit logs; Minimal Data Retention; IP allowlisting; enterprise managed settings override everything.
- Orb security: isolated e2b VMs; **OIDC workload identity** instead of long-lived cloud creds; secrets/env hierarchy (personal > project > workspace) with change history (never values); portals authenticated; webhooks rate-limited, at-least-once, `Idempotency-Key`.

## Model/provider abstraction
- **Multi-model routing as a product**: dial modes map to (agent, oracle) pairs — low=GLM-5.2+GPT-5.6 Sol; medium=Sol(med)+Sol(high); high=Sol(x-high)+Fable 5; ultra=Fable 5(high)+Sol(high). With linked ChatGPT subscription, low/med/high use OpenAI models only.
- Dedicated small-model roles: Search/finder=Terra; Librarian=Sol; Read Thread=GLM-5.2; Titling=Luna; Compaction=Sol; Dictation=GPT-4o Transcribe; Realtime voice; View Media=Gemini Flash; Painter=GPT Image 2; Puck=Sol.
- BYOK API keys; link ChatGPT or Grok consumer subscriptions; enterprise regional endpoints. No local/OSS model support. No user-facing prompt-caching controls; Anthropic-style cache fields in usage.

## Surfaces
- **TUI CLI** (Ctrl+O command palette; configurable keymap); execute mode `-x/--execute`; piped stdin; **Claude-Code-compatible `--stream-json`** (+`--stream-json-input` with steer flag, multi-turn stdin); notification sounds.
- **Web app** (feed, thread pages, changes sidebar/review, portals, projects, settings, Puck); **mobile web**; realtime voice.
- **IDE**: VS Code/Cursor/Windsurf + Neovim + Zed — CLI sees open file/selection, edits through IDE with full undo.
- **Runners**: any machine runs threads created remotely (`amp --no-tui [--runner-id] [--remote-control-terminal]`; web terminal remote control).
- **Orbs**: remote unsupervised execution (Debian 12 e2b VMs; 5 sizes; 5-min auto-pause; shared tmux Terminal pane with the agent; `amp sync <thread-id>` mirrors changes locally); **Portals** = authenticated public URLs to orb HTTP servers; `.amp/services.yaml` supervised services; webhooks (event-driven orbs).
- **Slack app** (@Amp mentions → Puck), **SDKs**, **CI** via `AMP_API_KEY`.

## Session & collaboration
- Threads sync local↔server; `/feed`. Continue; message editing (Tab to prior messages + `e`), restore-to-point; fork removed — new thread + `@T-id` mention (parent/child links, Thread Map). Queue/steer; archive; **labels**.
- **Sharing levels**: Unlisted, Workspace-shared (default), Group-shared (Enterprise), Private (admins can view, audit-logged).
- **Workspaces**: pooled credits, activity leaderboard; **multiplayer orb threads** (co-drive: messages, files, portals, shared terminal; @-mention teammates; owner pays; TTL).
- **Puck** as coordination home base; realtime voice; explain-usage Q&A.
- Review flows: changes sidebar (**Ship** = commit+push to origin/main; **Push to Branch** = `gh pr create`; **Custom Ship** = project-configured prompt); Agentic Review agent (pre-scans diff, recommends review order).

## Config & conventions
- Settings: user `~/.config/amp/settings.json[c]` < workspace `.amp/settings.json[c]` (nearest up to repo root) < enterprise managed paths; all keys `amp.`-prefixed.
- Catalog: `amp.fuzzy.alwaysIncludePaths`, `amp.showCosts`, `amp.git.commit.ampThread.enabled` (Amp-Thread-ID trailer) & `amp.git.commit.coauthor.enabled`, `amp.keymap`, `amp.mcpServers`, `amp.defaultVisibility`, `amp.tools.disable`, `amp.mcpPermissions`, `amp.updates.mode`...
- Env: `AMP_API_KEY`, `AMP_SKIP_UPDATE_CHECK`, `AMP_ORB`, `AMP_THREAD_ID`, proxies.
- Repo conventions: `AGENTS.md`, `.amp/` (settings.json, plugins/, services.yaml, portals/), `.agents/` (setup, resume hooks, skills/), `.claude/skills/` compat.

## Distinctive features
- **Oracle** tool (strong second-opinion model on demand).
- **The Dial**: capability presets with server-side model routing, silently remixed as frontier models change; subscription-aware routing.
- **Orbs**: per-thread ephemeral remote machines with portals, webhooks, OIDC identity, shared-with-agent tmux terminal, `amp sync`.
- **Puck** meta-agent + realtime voice; Slack entry.
- **Agent-to-agent**: agents spawn threads on other machines/runners/orbs and exchange files; self-waking schedules.
- **Amp-hosted personal/workspace plugin & skill Git repos** with share URLs, imports, live reload.
- **No-approvals philosophy** + permissions-as-plugins; automatic deep secret redaction.
- Claude-Code-compatible streaming JSON (in and out).
- Thread-as-knowledge-base: enterprise-owned threads, feed search, labels, Thread Map.
- Custom agent **modes as plugin products**.

## Canonical workflows
1. Local iterate loop: prompt (with @file, screenshots, @@thread refs) → tools → edit prior message or queue/steer → archive & new thread per task.
2. Headless/CI: `AMP_API_KEY=… amp -x "..." --stream-json`; multi-turn via `--stream-json-input`.
3. Orb fire-and-forget: `amp -ox "prompt" --orb-size a1.small` → fresh VM, `.agents/setup` → unsupervised work → review on web → `amp sync` → Ship/PR.
4. Portal preview: commit `.amp/services.yaml` → agent ensures services → open portal URL → try app live → iterate.
5. Event-driven automation: webhook wakes paused orb; agent sets schedules ("every morning DM me 5 slowest queries").
6. Team collaboration: workspace pooled credits, shared threads, multiplayer orbs, @mentions.
7. Meta-orchestration via Puck (web/mobile/Slack, voice).
8. Deep research combo: Librarian + Oracle + subagents → merge.
9. Runner fleet: `amp --no-tui --runner-id cloud-dev-box` → create threads on that runner from anywhere.
10. Extending: plugin in `.amp/plugins/` → share via personal repo → admin pushes org-wide.

## Rust / low-footprint notes
- Thin local client + server-side loop: local binary stays small/stateless-ish, secrets stay local, threads cache locally. (Ka is local-first, so this split is a contrast, not a template.)
- Single-file Bun-compiled executable for fast cold start; installer fetches binary; auto-update via CDN. Rust analog: static musl single binary + self-update.
- Plugin runtime: in-process with typed events/tools/commands/UI; live reload. Rust analog: WASM or Lua plugins with similar event surface.
- **Tool-context economy**: tool definitions cost tokens — tools hidden until skills load (`includeTools` filters, `builtin-tools` gating, disable globs). Worth copying.
- Per-role small models for background tasks (titling, thread-reading, search, compaction).
- Skills/AGENTS.md layering (subtree activation + globs frontmatter) is pure convention — cheap.
- Claude-Code-compatible stream JSON gives free ecosystem interop.
- MCP streamable-HTTP + SSE fallback with OAuth token storage.
- Caveat: closed-source; doc-grounded only.
