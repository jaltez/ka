# Droid / Zed / Cursor / Copilot coding agent / Devin (brief) — Feature Extraction

> Source: docs.factory.ai, zed.dev/docs, cursor.com/docs, docs.github.com, docs.devin.ai. Scout: AdditionalAgents.

## Factory Droid (docs.factory.ai)
- Interactive CLI + **Spec Mode** (plan-first read-only) + **Mission Mode** (multi-agent orchestrator with workers/validators); headless `droid exec` (read-only default, `--auto low/med/high`, `--skip-permissions-unsafe`, text/json output, **stream-jsonrpc JSON-RPC subprocess protocol**, session continue/fork).
- Tools: Read (PDF)/LS/Grep/Glob/Create/Edit/ApplyPatch/Execute/WebSearch/FetchUrl/MCP/Task/TaskOutput/TaskStop/AskUser/TodoWrite/Skill/ExitSpecMode/GenerateDroid.
- **Autonomy Levels** Off-Low-Med-High + commandAllowlist/Denylist/**Blocklist** where blocklist is unbypassable and droid resolves the actual invoked program before matching (wrapper-proof); OS sandbox + **Droid Shield** secret scanning + blocking hooks.
- **Custom droids** = subagents in `.factory/droids/*.md` (model/reasoningEffort/tools/mcpServers frontmatter; built-ins worker+explorer; complexity-tier model routing; background task_id+resume; Claude Code agent import).
- AGENTS.md with 80k/40k char context budgets, nested files, CLAUDE.md compat.
- CI code review via droid-action with deep/shallow depth, explicit bug taxonomy focus, repo guidelines via SKILL.md auto-injection.
- `-w/--worktree` isolation; Droid Computers + BYOM relay; **Factory Router** auto model selection + BYOK.

## Zed (zed.dev)
- Agent Panel w/ first-party agent + **ACP external agents** + Terminal Threads.
- Built-in tools: diagnostics/fetch/find_path/grep/list_directory/read_file/search_web(Pro)/copy_path/create_directory/delete_path/edit_file/move_path/write_file/terminal/skill/spawn_agent.
- Agent Profiles (tool availability) + Tool Permissions (allow/deny/confirm) + terminal OS sandbox w/ host grants.
- **@-mentions** (files/dirs/symbols/threads/skills/diagnostics/diffs/URLs).
- Auto-compaction + /compact + **New From Summary** (80k min window banner); checkpoints per edit; queued msgs + Steer; threads sidebar + per-thread worktree isolation.
- Provider paths: Zed-hosted/API keys/subscriptions/gateways/local models.

## Cursor (cursor.com/docs)
- Agent w/ browser tool, image gen, clarifying questions, checkpoints (local, non-git), steering queue, /goal + /loop.
- Rules = `.cursor/rules` .mdc (alwaysApply/description/globs/manual) + user + enforceable team rules + AGENTS.md; Plan/Debug/Design modes; Agents Window.
- CLI w/ shell-mode/ACP/headless/GitHub Actions.
- **Cloud Agents** (ex-Background Agents) = VM envs (Dockerfile/.cursor/environment.json, snapshots, agent-led setup) + pre-baked Builds, multi-repo, MCP http/stdio+OAuth, hooks set (preToolUse, beforeShellExecution, afterFileEdit, beforeSubmitPrompt, subagentStart/Stop, preCompact, afterAgentResponse/afterAgentThought, stop), artifacts (screenshots/videos/logs), remote desktop takeover, share URLs w/ repo-access verification.
- Entry via iOS/web/desktop/Slack/@cursor on GitHub+Bitbucket+Linear/API; Composer+Grok custom models, Cursor Router.

## GitHub Copilot coding agent (docs.github.com)
- Cloud agent: research→plan→branch edits→draft PR in ephemeral GitHub-Actions env; 59-min cap; one repo/branch/PR per task.
- Entry: agents panel, issue assignee, @copilot on PRs, VS Code, automations (schedule/event), security campaigns, Teams/Slack, Azure Boards/Jira/Linear.
- Custom agents, custom instructions, Copilot Memory (preview), MCP (GitHub+Playwright default), hooks, skills, copilot-setup-steps.yml env setup.
- Usage = Actions minutes + AI credits. `gh copilot` CLI: suggest (-t shell/git/gh), explain, alias.

## Devin (docs.devin.ai, brief)
- **Knowledge org bank** w/ trigger-description retrieval, macros !name, per-user toggles, nested folders w/ bulk toggle + auto-organize, repo pinning, enterprise scope + promote.
- **Playbooks** w/ Procedure/Specifications/Advice/Forbidden Actions/Required-from-User sections, .devin.md, version history, generate-from-session.
- Parallel/duplicate/scheduled sessions, PR-comment replies, MCP marketplace, native integrations.

## Cross-cutting signals for Ka
- Plan-first read-only mode (Spec Mode / Plan mode) is now table stakes.
- Risk-tiered approvals with an **unbypassable blocklist + wrapper-proof program resolution** (Droid) is the strongest safety pattern observed.
- Budget-capped AGENTS.md + lazy SKILL loading (Droid 80k/40k char budgets).
- Worktree/VM isolation for subagents and sessions.
- Markdown subagents + hooks as extension substrate.
- Headless stream protocol + SDK as contract (stream-jsonrpc, ACP, Claude-Code-compatible stream JSON).
