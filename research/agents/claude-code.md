# Claude Code — Feature Extraction

> Source: code.claude.com/docs (full page set via llms.txt) + anthropics/claude-code plugins repo. Closed-source CLI (native binary, bundled ripgrep). Scout: ClaudeCode.

## Identity
- Native binaries (since v2.1.16; npm deprecated); brew/winget/apt/dnf/apk. Anthropic Commercial Terms. Requires Claude subscription or Console API key.
- Architecture: "gather context → act (tools) → verify" loop; user interruptible; same engine across Terminal/VS Code/JetBrains/Desktop/Web/mobile.
- Public repo = issues + official plugins monorepo (security-guidance, ralph-wiggum, pr-review-toolkit, plugin-dev, hookify, feature-dev, frontend-design...). Plugin layout: `.claude-plugin/plugin.json` + `commands/`, `agents/`, `skills/`, `hooks/hooks.json`, `.mcp.json`.

## Core loop & orchestration
- Agentic loop: prompt → context gathering → tool actions → verification; Esc interrupts mid-turn (work kept), queued messages sent next.
- Subagents via `Agent` tool (renamed from Task): own context window, custom system prompt, tool subset, independent permissions; background by default; single text result to parent.
- Built-in subagents: Explore (read-only, thoroughness levels, skips CLAUDE.md + git status), Plan (plan-mode researcher), general-purpose, claude (catch-all), statusline-setup, claude-code-guide.
- Subagent nesting to a depth limit; fork mode (subagent inherits full parent conversation, background, `SendMessage`/resume by agent ID).
- **Agent teams** (opt-in): lead spawns teammates, shared task list, inter-agent messaging, TeammateIdle hook.
- **Dynamic workflows (Workflow tool)**: Claude writes a JS orchestration script (`agent()`, `pipeline()`, schema-validated outputs) executed by a runtime in background; dozens–hundreds of subagents; `/workflows` progress view (pause/resume/stop/restart); save as `/name`. Bundled `/deep-research`. Trigger keyword `ultracode`.
- Background agents: `/background` detaches session; `claude agents` supervisor (attach/logs/stop/respawn/rm); `/fork` background copies; `/tasks`.
- Cross-session messaging: `SendMessage`/`ListAgents`; `notify_when_idle`.
- Crons (`/loop`, CronCreate) and Monitor tool re-inject events mid-conversation.

## Tool catalog (exact names)
1. Agent 2. Artifact — publish HTML/MD as claude.ai page 3. AskUserQuestion
4. Bash — per-call timeout, auto-background on timeout, output streamed to file (30k inline / 64MiB read-back / 5GB kill), cwd carry-over, cgroup memory cap
5. CronCreate/CronDelete/CronList — session-scoped scheduled prompts, restored on resume
6. Edit — exact old_string→new_string; read-before-edit; replace_all
7. EndConversation — abuse last-resort lock; un-blockable
8. EnterPlanMode/ExitPlanMode 9. EnterWorktree/ExitWorktree
10. Glob — mtime-sorted, 100-file cap 11. Grep — ripgrep-backed; modes
12. LSP — via plugin-provided servers
13. ListMcpResourcesTool/ReadMcpResourceTool
14. Monitor — background command/WebSocket watcher; each output line an event
15. NotebookEdit — Jupyter cells
16. PowerShell — native pwsh tool
17. PushNotification 18. Read — line-numbered; PARTIAL paging; images/PDFs/ipynb
19. RemoteTrigger — Routines on claude.ai
20. ReportFindings — structured review findings
21. ScheduleWakeup — self-paced /loop (1min–1h)
22. SendMessage 23. SendUserFile 24. ShareOnboardingGuide
25. Skill — execute skill in main conversation
26. TaskCreate/TaskGet/TaskList/TaskUpdate — shared task list (replaces TodoWrite)
27. TaskOutput/TaskStop 28. TodoWrite (legacy)
29. ToolSearch — deferred MCP tool discovery (default on)
30. WaitForMcpServers
31. WebFetch — URL → markdown → small-model extraction; 15-min cache
32. WebSearch — ≤8 internal searches/call; 200/session cap
33. Workflow — run dynamic workflow script
34. Write
Plus `mcp__<server>__<tool>` (plugin: `mcp__plugin_<p>_<s>__<t>`); server-side advisor tool.

## Context management
- Startup context: system prompt + output style + CLAUDE.md hierarchy (managed → user → ancestors root-down → ./CLAUDE.md → CLAUDE.local.md; subdir lazy-load) + `.claude/rules/*.md` (path-scoped `paths:` globs) + **auto-memory MEMORY.md** (200 lines/25KB) + **skill descriptions listing (budget = 1% of context window)**.
- CLAUDE.md imports `@path` (≤4 hops); AGENTS.md via import/symlink; `/init` generates; `/import` migrates codex/gemini.
- Compaction: auto-compact (`/autocompact <tokens>|auto`); `/compact [focus]`; clears older tool outputs first, then summarizes; project CLAUDE.md re-injected; invoked skills re-attached (5k each / 25k budget); PreCompact/PostCompact hooks; thrash guard.
- **/rewind checkpoint menu**: Restore code+conversation / conversation only / code only / Summarize from here / up to here (100 snapshots, editing-tool snapshots only, 30-day retention).
- **/context**: colored grid of context consumers. Prompt caching automatic w/ hit-rate view.
- Auto memory: Claude-written notes (user/feedback/project/reference) in project memory dir (MEMORY.md index + topic files); subagent memory scopes.
- MCP context economy: ToolSearch deferral, discovery cache (2h), MAX_MCP_OUTPUT_TOKENS=25000.

## Extensibility
- **MCP**: 4 transports (stdio/http/sse/ws); scopes local/project(.mcp.json)/user; OAuth 2.0 + PKCE DCR + CIMD + pre-configured clients; headersHelper dynamic auth (env-scrubbed); `${VAR}` expansion; roots/list; auto-reconnect; >2min calls auto-background; enterprise allow/denylists; channels pushing webhooks/chat into sessions.
- **Skills** (= custom commands, merged): SKILL.md + frontmatter (name, description, when_to_use, argument-hint, arguments, disable-model-invocation, user-invocable, allowed-tools, disallowed-tools, model, effort, context: fork + agent, background, hooks, paths, shell...); enterprise > personal > project > plugin; monorepo dir-qualified names; **dynamic context injection** `` !`cmd` `` and fenced blocks; up to 6 stacked skills; skillOverrides; live reload; claude.ai sync.
- **Slash commands**: ~50 built-ins + bundled skills + user/plugin; `!` shell mode; `@file`/`@server:resource`/`@session` mentions.
- **Hooks**: 31 events (SessionStart/Setup/End, UserPromptSubmit/Expansion, PreToolUse/PermissionRequest/PermissionDenied/PostToolUse/Failure/PostToolBatch, SubagentStart/Stop, TaskCreated/Completed, Stop/StopFailure, TeammateIdle, Notification, MessageDisplay, InstructionsLoaded, ConfigChange, CwdChanged, DirectoryAdded, FileChanged, WorktreeCreate/Remove, PreCompact/PostCompact, Elicitation/Result); 5 handler types: **command / http / mcp_tool / prompt (LLM) / agent (subagent verification)**; matcher exact|regex; `if` permission-rule filter; exit 2 = block; JSON outputs (permissionDecision, updatedInput, additionalContext, systemMessage...).
- **Plugins**: marketplace install; components = commands/agents/skills/hooks/.mcp.json/output-styles/LSP servers/workflows/monitors/theme; user_config options; plugin-dependencies version constraints; zip/URL loading.
- **Subagents**: markdown + frontmatter in `.claude/agents/` etc.; fields: name, description, tools/disallowedTools, model, permissionMode, maxTurns, skills (preload full content), mcpServers, hooks, memory (user/project/local), background, effort, **isolation: worktree**, color, initialPrompt; live-reload.
- **Output styles** (markdown, Default/Proactive/Concise/Explanatory/Learning); **statusline** (script fed JSON w/ model, cost, context %, rate limits, worktree, PR); workflows as shareable extensions; LSP plugins; custom keybindings.

## Safety & permissions
- **6 permission modes**: default, acceptEdits, plan, **auto** (classifier on Sonnet 5 reviews actions; extensive blocked list: curl|bash, secret exfil, prod deploys, mass delete, IAM grants, force push, git reset --hard, terraform destroy, unapproved PR merge, tunnels/reverse shells...; allowed: local file ops, lockfile installs, read-only HTTP...; verdict caching; 3-consecutive/20-total block fallback; subagents checked at spawn/during/return), dontAsk, bypassPermissions.
- Permission rules: `Tool(specifier)` in allow/ask/deny; deny→ask→allow; Bash glob wildcards + **compound-command subcommand matching + wrapper stripping (timeout/nice/nohup/xargs/env)**; Read/Edit gitignore-style path anchors; Edit-allow implies read; WebFetch(domain); Agent(name); Skill(name); Tool(param:value); built-in read-only command set never prompts; redirections checked as writes; PowerShell AST parsing + alias canonicalization.
- **Protected paths** (.git, .vscode, .claude/...) never auto-approved; critical-path rm circuit breaker un-approvable.
- "don't ask again" saves to settings.local.json; Ctrl+E risk explainer (Low/Med/High).
- PreToolUse hooks can deny/allow/ask/rewrite; exit-2 blocks even allow rules.
- Workspace trust dialog; `claude -p`/SDK never shows it.
- **Sandboxing**: OS-level Bash sandbox — macOS Seatbelt, Linux bubblewrap+socat network proxy; auto-allow mode; filesystem allowWrite/denyRead; network allowedDomains + Unix sockets + seccomp; credentials.files/envVars deny or mask (sentinel copy+proxy); failIfUnavailable.
- Enterprise: managed-settings.json (MDM) + server-managed; allowManagedPermissionRulesOnly/HooksOnly; gateways (SSO, model routing, spend limits); apiKeyHelper; OTel.
- Secrets: env scrubbing, keychain, `claude setup-token` CI token.

## Model/provider abstraction
- Providers: Anthropic API/subscription, Bedrock, Vertex, Microsoft Foundry, LLM gateways (ANTHROPIC_BASE_URL), apps gateway.
- Model selection: /model → --model → env → settings; aliases default/best/fable/sonnet/opus/haiku/opusplan (plan on opus → execute on sonnet); availableModels allowlist + enforce; **fallback model chains**; fast mode (Opus speed tier); **advisor tool** (stronger model consulted at key moments).
- Effort levels low→max + `ultracode`; `ultrathink` keyword; MAX_THINKING_TOKENS; extended thinking default on, inherits to subagents.
- Prompt caching automatic; documented invalidation semantics.

## Surfaces
- Terminal CLI (classic + fullscreen renderers, voice dictation, vim mode, accessibility); VS Code + JetBrains; Desktop app (visual diffs, parallel sessions, computer use, iOS Simulator pane, browser, scheduled tasks, SSH); Web cloud sessions (`--cloud`/`--teleport`); mobile via Remote Control + push; Slack; GitHub Actions (@claude, review app), GitLab CI; Chrome extension; Channels (Telegram/Discord/iMessage/webhooks).
- Headless: `claude -p` + flags (--output-format, --json-schema structured output, --allowedTools, --permission-mode, --bare (skip auto-discovery), --worktree, --fork-session); exit codes; capability init events.
- **Agent SDK** (Python/TS): full loop with hooks-as-callbacks, permission callbacks, sessions incl. fork/resume + external storage (S3/Redis), streaming input, structured outputs (Zod/Pydantic), custom tools via in-process MCP server, subagents, skills/plugins, checkpointing, cost tracking, OTel; subprocess architecture driving the CLI.
- Server: `claude gateway`, `claude remote-control`, `claude self-hosted-runner`.

## Session & collaboration
- Transcripts JSONL `~/.claude/projects/<project>/<id>.jsonl`; /export.
- Resume: --continue/--resume <id|name>/--from-pr <n>//resume picker (all projects, worktrees, branch filter, rename, preview); restores history/model/agent/permission-mode/active goal/unexpired crons; resume-from-summary dialog.
- **/branch** (in-process copy) vs **--fork-session** (new process) vs **/fork** (background copy) vs fork subagents.
- Naming; /clear; /compact; /context; **/btw** side questions (no history); session recap; /insights HTML report.
- Teams: shared tasks, SendMessage, agent view, background sessions survive terminal exit (supervisor daemon).
- **Worktrees per session** (`--worktree name|#PR|URL`; fresh vs head base; `.worktreeinclude`; 4-layer isolation enforcement: edit paths, cwd checks, git-redirect detection, unverifiable command-shape refusal; non-git VCS via hooks).
- Scheduled: /loop (+ .claude/loop.md), CronCreate tools, Routines (cloud), desktop tasks, GH Actions cron. **/goal**: keep working until condition met.

## Config & conventions
- Settings precedence: managed-settings.json (MDM/console) > --settings > settings.local.json > project settings > user settings; live-reload via watchers.
- Reads CLAUDE.md (AGENTS.md via import/symlink); `.claude/rules/` path-scoped.
- `.claude/` contents: settings, CLAUDE.md, rules/, agents/, skills/, commands/, workflows/, output-styles/, hooks/, worktrees/, loop.md; `~/.claude` mirrors + projects/, plugins/, debug/.
- 30-day transcript sweep; `claude project purge`.

## Distinctive features
- **Auto mode**: server-side classifier permission system with domain-aware block/allow rulebook + verdict caching + subagent 3-point review.
- **Dynamic workflows**: LLM-written JS orchestration scripts, inspectable/pausable/savable — plan-in-code.
- OS-level Bash sandbox + credential masking via sentinel copy + proxy.
- 31-event hook lifecycle with 5 handler types incl. HTTP, MCP-tool, prompt-LLM, agent.
- Agent Skills standard (agentskills.io) with budgeted listing, eval tooling.
- Surface breadth + session mobility (`--cloud`/`--teleport`/Remote Control); artifacts publishing; channels.
- Checkpoint/rewind with partial restores.
- Worktree-native parallelism + `/batch` fan-out.
- MCP depth: OAuth DCR+CIMD, headersHelper, protocol negotiation, tool-search deferral, channels.
- Enterprise tier: managed+server settings, apps gateway, model allowlists, ZDR.

## Canonical workflows
1. First run: login → `/init` → `/memory` → `/mcp` → `/permissions` → `/doctor`.
2. Explore unfamiliar code (Explore subagent for heavy research).
3. Plan-then-execute: plan mode → ExitPlanMode approval → implement → verify.
4. Bugfix loop: paste error → tests → edits → Esc steer → Esc Esc /rewind → /diff → "create a pr".
5. Parallel dev: `--worktree` per feature; /batch decomposition (one bg subagent+worktree+PR each); `claude agents` monitor.
6. Review: /code-review [high] [--fix], ultra cloud review, /security-review, /simplify.
7. CI: `claude -p ... --json-schema --bare`; @claude mentions; /autofix-pr; ultrareview.
8. Scheduled: /loop 5m; Desktop tasks; Routines via /schedule.
9. Extension authoring: subagent/skill → /plugin-dev → marketplace; hooks for policy.
10. /deep-research or `ultracode:` orchestration; /goal; /workflows monitor; save script as reusable command.
11. Cross-device: /desktop, --cloud/--teleport, Remote Control, Slack.

## Rust / low-footprint notes
- Design patterns, not code: subagents as in-process contexts with narrowed tool sets; background sessions via supervisor daemon; SDK drives CLI via stable stream-json wire format (system/init capabilities) — worth copying.
- Native binary + bundled ripgrep; Glob vs Grep gitignore asymmetry; caps as cheap context guards.
- **Output economics**: bash output streamed to file w/ 30k read-back; MCP cap 25k tokens; hook outputs capped; tool defs deferred via ToolSearch; skill listing 1% of window; MEMORY.md 200-line cap; CLAUDE.md 4MiB skip.
- **Caching discipline**: CLAUDE.md as user message not system prompt; documented invalidation semantics.
- **Bash analysis engine**: subcommand decomposition, wrapper stripping, redirection-as-write, read-only allowlist — mini shell-semantics engine is the moat for safe auto-approval.
- Sandbox: Seatbelt/bubblewrap+socat+seccomp pluggable OS layer; cgroup memory caps.
- Checkpoints: per-prompt content-addressed snapshots, no VCS dependency.
- Everything-is-files config; exit codes as API (hook exit-2, -p 0/143).
- Footprint caution: pick core loop + tools + skills/hooks/MCP + 2–3 permission modes + worktrees; treat enterprise/classifier/cloud as optional modules.
