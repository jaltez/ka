# Canonical Workflows Observed Across Agents

The user-level flows every surveyed agent supports (or aspires to), distilled into named patterns. Attribution shows exemplars.

## W1. First-run onboarding
Install → auth (OAuth browser/device flow, API key, subscription login, or local endpoint) → optional project init (`/init` writes AGENTS.md/CLAUDE.md from repo scan) → optional trust dialog for project-local config/skills/hooks → first prompt.
- Exemplars: claude (`/init` reads Cursor/Copilot rules; trust dialog), pi (trust.json), aider (onboarding wizard auto-picks model from present keys, OpenRouter OAuth), gemini (auth picker + quotas), goose (onboarding wizard).
- Variation: omp/pi keep core minimal; onboarding is just `/login`.

## W2. Interactive edit-test loop (the core workflow)
Prompt → agent reads (line-ranged, summarized, hashline-anchored) → edits (string-replace / patch / AST) → optional LSP diagnostics-on-write / formatter → runs tests via shell (auto-backgrounds >60s) → feeds failures back (reflection ≤3) → repeat until green → user reviews diff → commit (auto-commit w/ generated message + attribution trailers).
- Exemplars: aider (auto-commit + reflection loop + tree-sitter lint), omp (LSP writethrough + hashline verification), roo (1000ms write-delay for editor diagnostics), goose (retry-with-shell-checks), plandex (tentative apply + rollback + auto-debug).

## W3. Plan-then-execute
Enter read-only plan mode (tool subset restricted; often a stronger/planner model) → research → write plan file → user approves (UI dialog / elicitation / auto) → handoff to executor (same session w/ synthetic continuation, or model switch) → implement → optionally clear history and act.
- Exemplars: claude (ExitPlanMode approval UI; opusplan alias), gemini (plan file + Pro→Flash routing), opencode (plan agent + plan_exit synthetic message), droid (Spec Mode + ExitSpecMode), crush/goose (/plan + /endplan + history clear), omp (xd://propose + PlanYolo + prewalk strong→cheap), cline (Plan/Act w/ per-mode models), plandex (whole pipeline), aider (architect mode).

## W4. Long-session survival
Context approaches limit → (speculative compaction armed) → prune old tool outputs → compact (LLM summary / provider-native / handoff / bitmap) keeping recent tail → auto-continue → display transcript keeps full history with dividers → on overflow: drop failing turn, try model promotion, compact, retry.
- Exemplars: omp (6 triggers, methodOrder ladder, speculation, promotion), claude (clear outputs → summarize; re-inject project files), codex (auto-compact + get_context_remaining/new_context), roo (condense same-model + 25%-truncate retry), openhands (condensation-as-event), goose (80% threshold + tool-pair summarization).

## W5. Exploration & branching
Navigate session tree → jump to earlier user message → optional branch summary of abandoned path → diverge in same file (or /fork to new file) → re-ask questions as sibling branches; or rewind (conversation-only / files-only / both / summarize-from-here).
- Exemplars: omp/pi (/tree + branch summaries + ask re-answer), codex (fork at turn boundaries + rollback + thread sections), claude (/rewind partial restores), gemini (tri-state rewind + checkpoint re-proposes tool call), kilo (Revert + Redo branching), opencode (/undo //redo per-message revert).

## W6. Parallel task fan-out
Decompose goal → spawn subagents (markdown-defined, role-routed models, read-only scouts vs full-tool workers) → isolated contexts (worktrees / CoW clones / containers / remote envs) → results self-deliver as messages or summaries → parent merges, verifies claims → steer/kill/revive via hub or agent manager.
- Exemplars: omp (tasks[] batch + hub + parked/revive + CoW isolation), claude (agent teams, /batch one-PR-per-unit, workflow scripts), codex (multi-agent v2 spawn edges + shared budgets), goose (summon delegate + sub_recipes), amp (fan-out prompts + orbs), kilo (Agent Manager + auto-delegation), openhands (task tool + shared browser executor).

## W7. Headless / CI automation
One-shot prompt with structured flags → stream-json event output → schema-constrained final answer → exit codes as API → used in GitHub Actions (mention-triggered issue→PR), cron schedules, pipelines (`cmd | agent "explain" | agent "commit msg"`).
- Exemplars: claude (-p --json-schema --bare; @claude Actions; autofix-pr), codex (exec --json --output-schema --ephemeral), amp (-x --stream-json Claude-Code-compatible), cline (NDJSON chaining), gemini (exit 42/53), goose (run --output-format json), opencode (run --auto + /oc comments).

## W8. Review before ship
Diff review by reviewer subagents w/ severity taxonomy / bug categories → findings fed back to implementer → or dedicated review CLI (uncommitted / base / commit / PR) → ship action (commit+push, PR creation, custom project-configured action) → inline line comments.
- Exemplars: claude (/code-review [high] [--fix], ultrareview cloud, ReportFindings), codex (review --uncommitted + Guardian auto-review), amp (Ship / Push-to-Branch / Custom Ship + Agentic Review), goose (review + .agents/checks severity models), droid (CI action w/ taxonomy + SKILL.md guidelines), kilo (/review + inline comments).

## W9. Sandbox-first / remote execution
Run agent (or a single tool call) inside OS sandbox (seatbelt/bwrap/docker/gVisor) or remote VM → sandbox-expansion requests for extra dirs/network → portals expose dev servers → changes synced back → unsupervised work with wake-up schedules and webhooks.
- Exemplars: amp (orbs + portals + OIDC + schedules + webhooks), codex (bwrap matrix + network proxy + exec-server remote envs), gemini (5 backends + tool-level sandboxing + expansion dialogs), claude (sandbox + credential masking), openhands (DockerWorkspace ladder), cursor (cloud agent VMs).

## W10. Extend the agent
Drop extension/plugin (TS module, WASM/Lua analog) hooking tool calls/results/context/system prompt → or SKILL.md (progressive disclosure) → or custom markdown agent/command → or MCP server (stdio/http, OAuth) → hot reload → share via marketplace/git/npm repo.
- Exemplars: pi (60+ example extensions replace everything incl. providers and CLI flags), omp (in-process ExtensionAPI superset + marketplaces), claude (31-event hooks, plugins, skills standard), gemini (extensions as full distribution units), goose (Open-Plugins + platform extensions), crush (bash-as-config + self-configuring skill), amp (hosted plugin repos).

## W11. Knowledge accumulation
Background memory extraction from sessions → consolidation (git-baselined diff review, patch inbox) → injected next session (budgeted, untrusted-wrapped, cited) → lessons promoted to skills; cross-session search.
- Exemplars: codex (two-phase + citations), gemini (Auto Memory inbox, nothing auto-applied), claude (auto-memory MEMORY.md tiers), omp (mnemopi/local backends + learn tool), openhands (two-tier MEMORY.md), goose (memory + chatrecall), devin (knowledge bank + playbooks).

## W12. Cross-device / collaborative session
Start local → hand off to cloud/desktop/mobile/Slack → remote-control terminal → guests join live replicas (E2E-encrypted) → multiplayer orbs → session sharing links with visibility levels.
- Exemplars: claude (--cloud/--teleport, Remote Control, channels, agent view), amp (web/mobile/Slack, runners, multiplayer), omp (/collab replicas + /share), kilo (synced sessions across surfaces), crush (multi-client workspaces), goose (serve + Telegram).

## W13. Scheduled / event-driven agents
Cron jobs over saved prompts/recipes/workflows → self-waking schedules set by the agent mid-conversation → webhook-triggered remote runs → monitors watching command output/WebSocket lines re-injecting events.
- Exemplars: goose (manage_schedule from chat + cron recipes), claude (CronCreate + /loop + ScheduleWakeup + Monitor tool + cloud Routines), amp (self-set schedules + orb webhooks), cline (cron + connectors), openhands (automation server), gemini (GH Action cron).

## W14. Cost/context auditing
Live footer (tokens, cost, cache-hit-rate, context %) → /context grid of consumers → usage dashboards/HTML reports → per-subagent cost attribution → quota-aware model switching.
- Exemplars: pi (CH cache-hit-rate), claude (/context, /usage, /insights), omp (per-agent cost in hub), goose (stats HTML), amp (usage --details + explain-usage agent), kilo (cache read/write breakdown).

## W15. Migration between agents
Import sessions/configs from other agents; read their context files/skills/commands/MCP configs; cross-harness resume.
- Exemplars: claude (/import codex/gemini), codex (/import claude), omp (@claude/@codex resume import), goose (imports claude/codex/pi transcripts), crush (copilot config import), kilo (cursor/windsurf migration); AGENTS.md + agentskills.io + `.agents/` dirs as universal interchange (everyone reads everyone's skills/commands/MCP configs).
