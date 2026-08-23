# Cline / Roo Code / Kilo Code (VSCode family) — Feature Extraction

> Source: docs.cline.bot, roocodeinc.github.io/Roo-Code (archived — Roo shut down 2026-05-15), kilo.ai/docs. Scout: VscodeFamily.

## Identity & lineage
- Roo "originated from" Cline; **shut down May 15 2026** (community fork ZooCode). Kilo forked Roo as an explicit Cline+Roo superset, then **rebuilt the extension (Apr 2026) on a portable "Kilo CLI" core** shared across all surfaces.
- **Cline** — open-source TS; VS Code/JetBrains ext, CLI, Kanban web app, SDK (`@cline/sdk`), ACP agent. **Hub-spoke daemon** (hub coordinates sessions, spoke workers execute agents). Cline provider (usage billing), ClinePass, BYOK; Enterprise (SSO/RBAC/OTel/prompt storage); OpenAI-compatible Cline API.
- **Roo** — VS Code extension (dead). **Kilo** — VS Code/JetBrains, CLI, Cloud Agent, iOS/Android, Slack, web, App Builder; **Kilo Gateway (500+ models)**; KiloClaw hosted agents.

## Core loop & orchestration
- Shared DNA: chat panel → LLM streams tool calls → per-action approval (default) → execute → feed back → loop until explicit completion. Roo/Kilo legacy XML-style tools with `attempt_completion` terminal; Cline ClineCore uses JSON tool calling.
- **Cline**: Plan & Act dual-mode (separate models per mode; full history carries over); `/deep-planning` 4-phase (investigation → discussion → implementation_plan.md → task creation); parallel read-only subagents (own context/budget, per-subagent cost tracking); CLI Agent Teams; Kanban multi-agent w/ per-card worktrees + dependency chains.
- **Roo**: 5 built-in modes (Code/Ask/Architect/Debug/**Orchestrator=Boomerang**): orchestrator has no direct tools, delegates via `new_task(mode)`; subtask isolated context returns only `attempt_completion(result)` summary — deliberately tool-less to avoid context poisoning.
- **Kilo (new)**: modes renamed **Agents** (code/ask/plan/debug); **orchestrator deprecated — any full-tool agent auto-delegates via `task` tool** (built-in general/explore subagents); **Agent Manager**: side-by-side agents with isolated worktrees + diff reviewer + multi-model comparison.

## Tool catalog
- **Cline (ClineCore)**: `bash`, `editor`, `read_files`, `apply_patch`, `search` (ripgrep), `fetch_web`, `ask_question`. Legacy XML names: read_file/replace_in_file/execute_command. Subagent set: read-only tools + use_skill.
- **Roo (22 tools / 7 groups)**: Read (read_file, list_files, read_command_output); Search (search_files, **codebase_search** semantic); Edit (apply_diff, apply_patch, edit, edit_file, search_replace, write_to_file); Image (generate_image); Command (execute_command, run_slash_command); MCP (use_mcp_tool, access_mcp_resource); Workflow (ask_followup_question, attempt_completion, switch_mode, new_task, update_todo_list, skill); hidden fetch_instructions.
- **Kilo (new core)**: `read`, `edit`, `glob`, `grep`, `bash`, `task`, `webfetch`, `list` (+ snapshot machinery).

## Context management
- **Cline**: Auto Compact near limit (cheap via prompt-cache reuse; non-supporting models → rule-based truncation); `/smol`; `/newtask` distilled handoff; Memory Bank 6-file hierarchy; @-mentions; checkpoint restore recovers pre-summarization state; subagents isolate research.
- **Roo**: **Intelligent Context Condensing** (%-threshold, custom condense prompt, active model to avoid format mismatch, before/after metrics); 30% reserve (20% output + 10% buffer); native token counting w/ tiktoken fallback; ~300 tokens/image; **error recovery: auto-truncate 25% + retry on context-limit errors**. **Codebase Indexing**: tree-sitter AST → embeddings → Qdrant → `codebase_search`; incremental, branch-aware, hash-cached.
- **Kilo**: context progress graph / task timeline (activity bars; three-segment window; **cache read/write breakdown**); **AGENTS.md per-directory dynamic injection** (as `<system-reminder>` on file read — monorepo-scoped); Memory Bank deprecated in favor of AGENTS.md.

## Extensibility
- **Cline**: MCP (stdio/streamableHttp/sse; autoApprove lists; hosted Remote Servers); **Skills** (3-level progressive loading: ~100-token metadata → <5k body on trigger → unlimited bundled files); **Rules** (.clinerules/ + auto-detect .cursorrules/.windsurfrules/AGENTS.md; conditional rules via YAML `paths:` globs); SDK Plugins (custom tools, hooks, observers); scheduled agents (cron); **chat connectors (Telegram/Slack/Discord/WhatsApp/Google Chat)**; Agent Teams.
- **Roo**: **Marketplace** (in-extension hub for MCPs + Modes; parameterized MCPs); **Custom Modes** (roleDefinition/whenToUse/customInstructions + per-group **file-regex edit restrictions**; YAML import/export as one portable unit); Skills w/ **8-level override priority** (project-roo-mode > project-roo > project-agents-mode > ... > global-agents); slash commands; experimental custom tools.
- **Kilo**: custom subagents (kilo.jsonc `agent` section or markdown files w/ frontmatter; per-tool permission allow/ask/deny w/ **glob bash patterns, last-match-wins**; `permission.task` whitelist; steps cap; `kilo agent create`); custom modes; rules; skills (reads .claude + .agents); Workflows; MCP ("ask the agent" config); AGENTS.md standard.

## Safety & permissions
- **Cline**: every action approved by default; Auto Approve categories; **model flags `requires_approval` per command+args** (no fixed allowlist); YOLO mode; `.clineignore`; CLI env `CLINE_COMMAND_PERMISSIONS`; enterprise governance; checkpoints as the enabler of auto-approve.
- **Roo**: permission tiles; **protected files** (.roo/, .rooignore); **write delay (1000ms)** so VS Code diagnostics land before next step; command prefix allow+deny lists (deny wins on longer prefix); **dangerous substitution guard** (`${var@P}`, here-string subshells, zsh `=(...)`, `e:...:` qualifiers); `.rooignore` = AI file-access control (still checkpointed).
- **Kilo**: **per-tool Allow/Ask/Deny for every tool**; per-agent permission objects w/ globs; external markdown-agent dirs untrusted ({env:} blocked); **AGENTS.md write-protected** (AI edits require approval); snapshots as safety net.

## Model/provider abstraction
- **Cline**: Cline provider / ClinePass / BYOK; Anthropic (**incl. Claude Code subscription**), OpenAI (**incl. Codex OAuth**), Bedrock (3 auth), Gemini, DeepSeek, MiniMax, OpenRouter, Qwen, Z AI, Ollama, LM Studio, OpenAI-compatible; Plan/Act model split; `--thinking none..xhigh`; OpenAI-compatible Cline API.
- **Roo**: 24 providers incl. **VSCode LM** (built-in language models); **sticky models per mode** (mode switch auto-switches model).
- **Kilo**: Kilo Gateway (500+ models, pricing/capability picker, multi-model comparisons); model-per-agent; subagents inherit unless overridden.

## Surfaces
- **Cline**: VS Code family, JetBrains, ACP, CLI (headless: auto-triggers on `--json`/piped stdin; **NDJSON events `{type:ask|say, text, ts, reasoning, partial}`**; chaining `git diff | cline "explain" | cline "write commit msg"`), Kanban, SDK, enterprise console + REST API.
- **Roo**: VS Code only (dead). **Kilo**: VS Code, JetBrains, CLI, Cloud, mobile, Slack, web; **sessions sync across all surfaces**.

## Session & collaboration
- **Cline**: task history; checkpoints persist; message editing w/ Restore All; /newtask handoffs; Agent Teams; Kanban (worktrees, dependency chains, review, ship); cron; connectors; S3/R2 prompt storage.
- **Roo**: task-scoped checkpoints; subtask hierarchy view; message queueing; TTS; mode import/export.
- **Kilo**: cross-surface synced sessions; sharing; **Agent Manager** (per-agent worktrees, diffs); **snapshot revert w/ Redo/Redo-All branching** (non-destructive until new message); **inline code review w/ line-level comments**; Slack.

## Config & conventions
- **Cline**: `.clinerules/` + global; skills `.cline/skills/` (global wins collision — rare exception); `~/.cline/mcp.json`; reads AGENTS.md/.cursorrules/.windsurfrules.
- **Roo**: `.roo/rules[-{mode}]/` w/ `.roorules` fallback; modes `.roomodes`; MCP `.roo/mcp.json`; skills `.roo/skills/` + `.agents/skills/`; `.rooignore`. Load: global → project (wins) → legacy fallbacks.
- **Kilo**: **AGENTS.md** (uppercase required, root + per-directory, write-protected, cannot be disabled); `kilo.jsonc` ($schema-validated); `.kilo/rules[-{mode}]/`, skills, agents, plans/; priority: agent prompt > project instructions > AGENTS.md > global > skills.

## Distinctive features
- **Cline**: hub-spoke daemon SDK; Kanban parallel board; CLI connectors + cron; ACP; enterprise governance; subscription-as-provider; conditional path-glob rules; /deep-planning.
- **Roo** (historical): Boomerang orchestrator contract; per-tool-group file-regex restrictions + one-file mode export; sticky models; in-extension marketplace; condensing w/ custom prompt + 25%-truncate recovery; substitution guards; diagnostics-driven write delay; tree-sitter+Qdrant indexing.
- **Kilo**: portable CLI core for all surfaces; orchestrator deprecated → automatic subagent delegation; Agent Manager w/ worktrees; snapshot + Redo branching (git write-tree objects + patch records + hourly gc); per-tool Allow/Ask/Deny; /review suite + inline line comments; multi-model comparisons; agent-as-markdown; mobile/Slack/cloud.

## Canonical workflows
1. Cline Plan→Act: explore in Plan → switch to Act (model auto-switches, history retained) → implement → checkpoints rollback.
2. Cline /deep-planning: investigation → questions → implementation_plan.md → tracked task → /newtask handoffs across windows.
3. Cline headless CI: `git diff | cline --json "review" | jq`; @cline issue comments; cron; Telegram steering.
4. Cline Kanban: cards → parallel agents in worktrees → dependency chains → review → ship.
5. Roo Boomerang: orchestrator decomposes → new_task per mode → summaries only up → synthesis.
6. Roo auto-approved refactor: edits + whitelisted commands, denylist rm/sudo/push; write-delay diagnostics; checkpoint rollback.
7. Roo marketplace: install MCP + Mode → project/global scope → committed config.
8. Kilo flow: code agent auto-spawns explore/general subagents → snapshot each step → Revert-to-here / Redo → /review uncommitted.
9. Kilo cross-surface: phone → VS Code → @code-reviewer subagent → Slack.
10. Memory Bank continuity (legacy): initialize → update at milestones → "follow your custom instructions" (Kilo: migrated to AGENTS.md).

## Rust / low-footprint notes
- **The family's trajectory IS the lean-agent argument**: Cline extracted the agent loop from a webview into a hub-spoke daemon; Kilo rebuilt on a portable CLI core for every surface. One headless Rust core + thin clients maps directly.
- **Checkpoints without history pollution**: shadow git repo separate from user's .git (exclusions in shadow `.git/info/exclude`); Kilo's rebuild is leaner: **raw `git write-tree` tree-hashes (no commits) + per-step patch records + `git checkout-index` restore + hourly `git gc --prune=7.days`** — cheap, storage-efficient; portable via git2/gitoxide.
- **Token budgeting**: reserve 30% (20% output + 10% buffer); provider-native counting w/ fallback; condense with the SAME model (avoids tool-format translation errors) then 25%-truncate retry — two-tier resilience.
- **Modes/agents are cheap**: roleDefinition + instructions near prompt end + `isToolAllowedForMode` gating + sticky model + optional file-regex restriction; subagents = child sessions with reduced tools and summary-only return (prevents context poisoning).
- **Progressive disclosure**: skills ~100 tokens until matched (3 levels); conditional rules on current-file globs; per-directory AGENTS.md as system-reminders on read.
- **Permissions as data**: per-tool allow/ask/deny + glob patterns (last-match-wins) vs prefix lists (longest-prefix deny) + substitution guards vs model-flagged `requires_approval` — three mature designs for a policy engine.
- **Search**: semantic = tree-sitter chunking + embeddings + Qdrant (Roo); Cline's new core ships plain ripgrep — evidence a lean agent can skip vector DBs.
- **Protocols**: Cline CLI NDJSON (`{type: ask|say, ...}`) is a minimal pipe-friendly agent protocol; ACP for editors; MCP as universal tool bus.
