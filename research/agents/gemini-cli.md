# Gemini CLI — Feature Extraction

> Source: research/repos/gemini-cli clone (README, GEMINI.md, packages/core tools/policy/scheduler, docs/*). Apache-2.0, TS on Node ≥20. Scout: GeminiCli.

## Identity
- npm `@google/gemini-cli`; Homebrew, conda, npx. Release channels: preview / latest / nightly. `gemini update`.
- Monorepo: `packages/cli` (Ink/React TUI), `packages/core` (agent loop, tools, policy, MCP — ~90 tool files), `packages/a2a-server` (Agent2Agent), `packages/sdk`, `packages/devtools` (network inspector), `packages/vscode-ide-companion`.
- Build: esbuild single bundle, wasm embedded, only native modules external; vendored ripgrep. Docker image doubles as sandbox.
- **Perf culture**: dedicated `perf-tests/` (cold-startup, idle-cpu, long-chat, skill-loading vs baselines) + `memory-tests/` nightly in CI; settings like `advanced.autoConfigureMemory`, `ui.incrementalRendering`.

## Core loop & orchestration
- `agent-session` drives turns; `scheduler/` executes tool calls incl. **parallel**; `confirmation-bus` routes approvals to UI/subagents.
- Loop: prompt → hooks (BeforeAgent/BeforeModel/BeforeToolSelection) → Gemini stream → function calls → policy engine + confirmation → parallel execution → results (masking/distillation/truncation) → repeat. Loop-detection guards infinite tool loops.
- **"Topic & Update" narration model**: model emits `update_topic` (title/summary/strategic-intent) instead of chatty progress. Model steering (experimental): user hints mid-tool-execution.
- Model fallback: on quota/server error prompts to switch (pro→flash); internal utility calls silently chain flash→pro. **Plan Mode routing: Pro while planning, Flash during implementation**.
- Subagents invoked as tools (`invoke_agent` + one tool per agent); no nesting (recursion protection).

## Tool catalog
1. `run_shell_command` — shell + PTY interactive mode + background processes (`list_background_processes`/`read_background_output`, `/shells` view)
2. `glob` 3. `grep_search` (ripgrep-backed + pure-TS fallback) 4. `list_directory`
5. `read_file` — text/images/audio/PDF, line ranges
6. `read_many_files` — multi-file concat via `@path`
7. `write_file` 8. `replace` — string edit w/ optional LLM self-correction on failed match (disabled by default)
9. `google_web_search` — Search grounding 10. `web_fetch`
11. `write_todos` 12. `ask_user` — structured questions
13. `activate_skill` — load SKILL.md on demand 14. `get_internal_docs`
15/16. `enter_plan_mode` / `exit_plan_mode` — read-only plan written to file
17. `update_topic` 18. `complete_task` — subagent finalization
19. `invoke_agent` + per-agent same-named tools
20/21. `read_mcp_resource` / `list_mcp_resources`
22–27. `tracker_*` — experimental DAG task tracker w/ ASCII graph + dependencies
28. `discovered_tool_*` — external tools via `tools.discoveryCommand`/`callCommand` (out-of-process discovery)
29. Browser agent tools (bundled chrome-devtools-mcp): a11y-tree nav, screenshot analysis + click when visualModel set

## Context management
- **Compression**: auto near token limit (`model.compressionThreshold` 0.5); manual `/compress`; rewind reconstructs across compression points.
- **Memory tiers**: `~/.gemini/GEMINI.md` → workspace dirs + ancestors; **JIT context**: when a tool touches a file/dir, GEMINI.md files in that dir + ancestors are injected (`context.discoveryMaxDirs` 200); filename configurable (`AGENTS.md` compatible); `@file.md` imports; `.geminiignore`.
- **Agent-authored memory**: model saves durable facts by editing memory files (shared→repo, private→project dir, personal→global).
- **Auto Memory** (experimental): background mining of idle sessions into unified-diff `.patch` updates + drafted SKILL.md in `<memoryDir>/.inbox/`, reviewed via `/memory inbox`; lock-file coordinated, secret-redacting, nothing auto-applied.
- **Token caching**: automatic implicit context caching for API-key & Vertex auth. Budgeting: `maxSessionTurns`, tool-output truncation 40k chars, masking + distillation services.

## Extensibility
- **MCP**: stdio/SSE/Streamable-HTTP, OAuth per-server, includeTools/excludeTools, env expansion + redaction, per-server trust, 10-min timeout; `gemini mcp add/remove/list`; resources addressable as `@server://resource/path`; MCP prompts become slash commands.
- **Extensions**: git-URL installs with manifest bundling mcpServers, contextFileName, excludeTools (incl. command-scoped like `run_shell_command(rm -rf)`), commands/, hooks.json, skills/, agents/, policies/*.toml; `security.blockGitExtensions` / allowlist.
- **Custom commands**: TOML in `~/.gemini/commands/` + `.gemini/commands/` (project wins); subdirs namespace `/git:commit`; `{{args}}`, `!{cmd}` shell injection (confirmation-gated), `@{path}` file/multimodal injection.
- **Subagents**: `.gemini/agents/*.md` + user dir; frontmatter (name, description, kind local|remote, tools w/ wildcards, inline mcpServers, model, temperature, max_turns 30, timeout_mins 10); `@agent-name` forces invocation; per-subagent policy rules.
- **Remote subagents (A2A)**: `kind: remote` + agent card URL/JSON; auth apiKey/http/google-credentials/oauth-PKCE. Ships `a2a-server` to run the CLI itself as an A2A agent.
- **Skills**: agentskills.io-standard SKILL.md; progressive disclosure (name+description in prompt, body on `activate_skill` consent); tiered discovery.
- **Hooks**: settings.json hooks on Before/After Agent/Model/Tool; JSON stdin/stdout protocol, exit-code 2 = block, arg rewriting, synthetic LLM responses, tail tool calls, context clearing.

## Safety & permissions
- Approval modes: `default` (confirm mutators), `auto_edit`, `plan` (read-only), `yolo` (flag-only, can be disabled). Hierarchy governs persistent approvals.
- **Policy engine**: TOML `[[rule]]` w/ toolName wildcards, `argsPattern` regex over stable-JSON args, `commandPrefix`, mode filters, subagent scoping; decisions allow|deny|ask_user (**deny hides tool from model entirely** for global rules); priority = tier×1000 + TOML priority; tiers: Default 1 < Extension 2 < Workspace 3 < User 4 < Admin 5 (`/etc/gemini-cli/policies`).
- **Folder trust**: per-directory trust gating system tool use (default on).
- **Sandboxing**: whole-process or **tool-level**; providers: macOS Seatbelt (6 profiles), Docker/Podman (custom image or auto-build `.gemini/sandbox.Dockerfile`), Windows icacls low integrity, gVisor/runsc, LXC. **Sandbox expansion**: modal requests extra dirs/network for that run.
- **Secrets**: env sanitization for subprocesses (redacts *TOKEN*/*SECRET*/*PASSWORD*/*KEY*/*AUTH*/*CREDENTIAL*); extension secrets in OS keychain.
- **Conseca** (experimental): LLM-based context-aware security checker generating dynamic policies. Browser agent: domain allowlist, scheme blocking, upload blocking, maxActionsPerTask.
- Dangerous-cmd handling: tree-sitter-bash parsing + shell-safety analysis.

## Model/provider abstraction
- Google-only: OAuth "Sign in with Google" (Code Assist free tier), `GEMINI_API_KEY`, Vertex AI. No OpenAI/Anthropic/Ollama support.
- Local: Gemma 4 via Gemini API; experimental **local model router** — locally served Gemma classifies which hosted model to route to (LiteRT-LM).
- Aliases `auto`/`pro`/`flash`/`flash-lite`; quota prompts to switch; overage strategies.

## Surfaces
- TUI (Ink/React, render in worker thread): themes, vim mode, screen-reader mode, `/stats`, background shells view, experimental voice mode, OSC 52.
- Headless: `-p`, `--output-format text|json|stream-json`; exit codes 0/1/42(input)/53(turn limit); stdin; `-i`.
- SDK; **ACP mode** (`--acp`, JSON-RPC, IDE can expose itself as MCP server); VS Code companion (workspace context: recent files, cursor, selection); a2a-server; GitHub Action + `@gemini-cli` mentions; devtools inspector.

## Session & collaboration
- Auto-saved project-scoped sessions `~/.gemini/tmp/<project_hash>/chats/`; `/resume` browser; retention (default 30d).
- Manual tagged checkpoints: `/chat save|list|resume|delete|share <tag>`; export file.md|json.
- **Checkpointing** (opt-in): shadow git repo snapshots project before each mutating tool + conversation + tool call; `/restore [id]` reverts files, history, and re-proposes the tool call.
- **`/rewind`** (Esc Esc): conversation only, AI file changes only, or both.
- Experimental git worktrees (`--worktree`); `--include-directories` multi-root; A2A delegation.

## Config & conventions
- Settings precedence: defaults < system-default < user < project < admin file < env < CLI. 233KB JSON schema. `/settings` editor.
- Context convention: GEMINI.md (name configurable, AGENTS.md compatible); `.geminiignore`; `/init` generator.
- Telemetry: OTel off by default; anonymized usage stats default ON (opt-out); `/quit --delete` wipes traces.

## Distinctive features
- Generous free tier with plain Google OAuth (60 req/min, 1,000 req/day Code Assist Individual).
- Shadow-git checkpointing + tri-state `/rewind` as first-class UX.
- Tiered TOML policy engine with admin enforcement and deny-hides-tool semantics.
- "Topic & Update" narration tool replacing chatty streaming progress.
- Auto Memory: background session mining → reviewable patch inbox (nothing auto-applied).
- Local Gemma router deciding hosted-model routing to cut costs.
- Bundled browser agent with domain jail & sandbox-aware modes.
- Extensions as full distribution unit (MCP+commands+hooks+skills+agents+policies+settings) with `migratedTo` auto-migration.
- Sandbox expansion dialogs; 5 sandbox backends incl. gVisor and LXC.
- Out-of-process tool discovery (`discoveryCommand`/`callCommand`) as MCP alternative.
- Experimental DAG task tracker with ASCII visualize.

## Canonical workflows
1. First run/auth → `/about`, `/stats model`.
2. Codebase Q&A: `@src/ explain the auth flow`; `!git log`.
3. Plan-then-implement: `/plan` → read-only research → plan file → approve → Flash implementation.
4. Guarded editing: diffs → confirm/always-allow (arg-narrowed) → checkpoint → `/restore` or `/rewind`.
5. Team command library: `.gemini/commands/git/commit.toml` w/ `!{git diff --staged}` → `/git:commit`.
6. MCP: `gemini mcp add github npx ...` → `@server://resource` → `/mcp auth`.
7. Subagent specialization: `.gemini/agents/security-auditor.md` → `@security-auditor audit this diff`.
8. Sandboxed automation: `GEMINI_SANDBOX=docker gemini -p "..."`; custom sandbox.Dockerfile.
9. CI/headless: `--output-format stream-json`; GitHub Action `@gemini-cli`.
10. Long sessions: auto-compression → `/stats` → `/chat save` → `--resume latest`; `--worktree`.

## Rust / low-footprint notes
- Process model: single Node process; Ink render in worker thread; tool-level sandboxing spawns containers per tool instead of wrapping the CLI.
- Bundling discipline: everything inlined except native modules; wasm embedded; code-split chunks.
- **Perf/memory culture: committed baselines + nightly regression gates** for cold start, idle CPU, long chats — worth copying for Ka.
- Lean-candidate patterns: policy engine = declarative TOML + tier arithmetic (pure data); confirmation bus as async channel decoupling UI from scheduler; ToolRegistry with legacy aliases and per-model-family declaration snapshots; JIT memory discovery bounded by discoveryMaxDirs; tool output truncation/masking/distillation as separate services; parallel tool scheduling with per-call confirmation futures.
- Droppable in a Rust port: OTel suite, puppeteer, xterm-headless — true core: HTTP client, tool registry + scheduler, policy TOML evaluator, MCP client, session JSON store, shadow-git checkpointer.
- Storage layout simple & replicable: per-project hash dirs (chats, checkpoints, plans), shadow git under home, user/project `.gemini` mirrors.
