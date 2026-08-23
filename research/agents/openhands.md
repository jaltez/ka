# OpenHands (V1: Agent Canvas + software-agent-sdk) — Feature Extraction

> Source: research/repos/openhands clone (Agent Canvas) + github.com/OpenHands/software-agent-sdk + docs.openhands.dev. MIT. Scout: OpenHands.

## Identity
- **Multi-repo V1**: (1) Agent Canvas — TS/React control-center frontend + local-stack orchestrator; (2) software-agent-sdk — Python core (Pydantic v2, LiteLLM, FastAPI); (3) automation repo (scheduling/webhooks); (4) extensions marketplace. SWE-Bench Verified 77.6%.
- Four Python packages: `openhands-sdk` (core), `openhands-tools`, `openhands-workspace` (Docker/remote), `openhands-agent-server` (REST+WS). Minimal install = sdk+tools.
- Ports: ingress 8000, agent-server 18000, automation 18001; state `~/.openhands/`; Electron desktop; Helm chart. Heavyweight (Node 22, uv, Docker, PostHog).

## Core loop & orchestration
- **Agent = stateless step loop**: (1) execute pending actions; (2) condenser → `View` or `Condensation` event; (3) LLM query (context-exceeded → `CondensationRequest` + return); (4) response → ActionEvents or MessageEvent; (5) confirmation check → `WAITING_FOR_CONFIRMATION`; (6) tools → ObservationEvents.
- **Conversation factory**: `LocalConversation` (in-process) vs `RemoteConversation` (HTTP/WS) — same API. State via `ConversationState` + append-only `EventLog`; FIFO lock.
- Default preset assembles toolset (terminal + file_editor + task_tracker [+ browser]), shared condenser with cheaper LLM (`usage_id` pattern).
- **Parallel tool execution** (regrouped by `llm_response_id` when converting back); **ACPAgent** swaps the whole loop for Claude Code/Codex/Gemini CLI over ACP; **Critic mixin** (LLM critic + refinement); **stuck detector** (sliding-window pattern matching); goal-completion loop (judge-driven); ask-agent mid-run questions; automation server dispatches scheduled/webhook conversations.

## Tool catalog
1. **terminal** — persistent bash; tmux backend preferred, PTY fallback; client-injected env hidden from model schema; soft timeouts; interactive ipython mode; detects python-literal-in-command misuse and coaches heredoc usage
2. **file_editor** — view/create/str_replace/insert/undo_edit; Rich diffs
3. **apply_patch** — swapped in for GPT-5 preset
4. **task_tracker** — view/plan; TASKS.json
5. **BrowserToolSet** — 14 tools (navigate/click/get_state/get_content/type/scroll/go_back/tabs/storage/recording); one shared Chromium/CDP executor across parent+subagents; graceful degradation
6. **task** — subagent delegation (description, prompt, subagent_type, resume)
7. **FinishTool** (+ response_schema)
8. Others: grep, glob, planning_file_editor, delegate, workflow, gemini, tom_consult
9. **MCP tools** — dynamically generated typed actions per remote tool
10. **canvas_ui / canvas_ui_control** — agent drives the frontend (navigate_to_file, open_tab, show_preview); server executor is a no-op ack, UI effect client-side over WS

## Context management
- **Condenser system**: NoOp / LLMSummarizing (keep_first=4 head + tail, summarize middle, max 120 events, dedicated LLM) / Pipeline / Rolling threshold. Output = `Condensation` event w/ `forgotten_event_ids` + summary → event-sourced, replay-safe.
- Triggers: automatic threshold + manual `CondensationRequest` on context-window error.
- **Persistent memory** (opt-in): two-tier MEMORY.md (user + project) injected as `<MEMORY_CONTEXT>` (≤6k chars) wrapped in `<UNTRUSTED_CONTENT>`; daily logs; agent self-maintains; secrets excluded.
- **Skills as context**: repo skills always in prompt; knowledge/task skills keyword-triggered; **path rules injected into tool results** (once per conversation, zero baseline cost).
- Token/usage/cost per usage_id; debounced auto-save; incremental events.

## Extensibility
- **MCP**: FastMCP client w/ sync↔async bridge; MCP servers attachable to skills; sparse per-server mutation protocol; marketplace browsing.
- **Skills (formerly microagents)**: 4 trigger types — repository (always-on: AGENTS.md/GEMINI.md/CLAUDE.md/.cursorrules), knowledge (regex on user messages), task, **path rules** (gitignore globs → `<EXTRA_INFO>` in tool results). Inline `` !`command` `` dynamic content. Levels: repo/org/user/global + public marketplace.
- **Subagents**: markdown+YAML frontmatter `AgentDefinition`; builtin presets default/bash_runner/code_explorer/web_researcher.
- **Hooks**: 6 lifecycle points (PreToolUse, PostToolUse, UserPromptSubmit, Stop, SessionStart, SessionEnd); **Claude-Code-compatible contract** (exit 0 JSON decision, exit 2 block); 3 evaluator modes: command / prompt (one LLM completion) / agent (sub-agent with tool allowlist).
- **Plugins**: bundle skills+hooks+MCP+agents+commands; extensions hub.
- **Custom tools**: `ToolDefinition[Action, Observation]` + executor + `register_tool`; **client tools**: frontend registers tools via agent-server JSON API.
- Agent Settings serialization/recreation (encrypted settings); Canvas agent profiles.

## Safety & permissions
- **Security analyzers**: `LLMSecurityAnalyzer` injects a required **`security_risk` param into non-read-only tool schemas** — model predicts risk inline, zero extra LLM calls. Risk LOW/MEDIUM/HIGH/UNKNOWN.
- **ConfirmationPolicy**: AlwaysConfirm / NeverConfirm / **ConfirmRisky(threshold=HIGH, confirm_unknown=True)**; conversation parks in WAITING_FOR_CONFIRMATION; rejection fed back as tool message.
- **Workspace ladder**: LocalWorkspace → DockerWorkspace (prebuilt image, extra_ports VSCode+VNC) → DockerDevWorkspace → RemoteAPIWorkspace → hosted/Modal/K8s/Cloud; **Apptainer** rootless for HPC; Sysbox for enterprise.
- Server auth: session API keys (indexed, rotation-ready); `OH_SECRET_KEY` encrypts conversation secrets; CORS allowlist.
- Secrets: conversation SecretRegistry (memory-only, masked logging); ACP `LookupSecret`; `acp_file_secrets` materialize credential files.
- Hooks as guardrails; self-host hardening runbook.

## Model/provider abstraction
- **LiteLLM**: 100+ providers incl. local (LM Studio/Ollama/vLLM/SGLang); `litellm_proxy/` prefix; `openhands/` first-party.
- Dual API paths: `completion()` + `responses()` (auto for gpt-5*); reasoning traces surfaced.
- Retry/backoff; per-call telemetry; LLM registry & routing; **fallback strategy** (auto-retry on alternate LLMs); **LLM profile store** (named configs, switchable mid-session); **subscription login** (ChatGPT Plus/Pro for Codex models).

## Surfaces
- **Agent Canvas web GUI** (chat + tabbed panel: files/git-diff, terminal, browser, vscode, planner, tasklist, commits; settings pages; automations; extensions hub; shared conversations; usage); embeddable as a library.
- **Agent Server** (REST+WS): OpenAPI-driven; **OpenAI-compatible `/v1/chat/completions` gateway** so any client drives the agent; deferred-init warm pools.
- **CLI** (separate repo): TUI, headless, GUI-server, resume, cloud, ACP IDE integrations.
- **SDK** (Python in-proc); ACP host mode; GitHub Actions; Electron; K8s/Helm; cloud/enterprise (SSO, org workflows).

## Session & collaboration
- Event-sourced persistence + resume; **fork** from previous message; pause/resume; send-message-while-running; ask-agent sidebar; conversation goals; shared-conversation viewer; team backends; automation dispatch history; PR review flows.

## Config & conventions
- Repo-level `.openhands/`: `setup.sh` (workspace start), `hooks.json` (+ scripts), `memory/`. `AGENTS.md` canonical (GEMINI/CLAUDE/.cursorrules parsed).
- Env: `LLM_*` auto-typed; `OH_*` server keys. Settings hierarchy: server → profiles → per-conversation encrypted → env.
- Version pinning via defaults.json + OpenAPI breakage CI.

## Distinctive features
- **Agent-agnostic control center**: one GUI drives OpenHands agent *or* Claude Code/Codex/Gemini CLI as interchangeable ACP engines.
- **Inline security-risk prediction** as a required tool argument — zero extra inference.
- **Event-sourced everything**: append-only typed event log is the single integration seam.
- Condensation-as-event with forgotten-id sets; deterministic replay.
- Path-triggered rules with zero idle context cost.
- Agent-driven UI (`canvas_ui`) as tools with no-op server executors.
- Automation server (scheduled + GitHub/Slack/webhook runs w/ templates).
- PR-label eval harness (`run-eval-{1,50,200,500}`).
- OpenAI-compatible agent gateway.
- Sync↔async MCP bridge; two-tier self-maintained MEMORY.md with untrusted wrapping.

## Canonical workflows
1. Local quickstart: `npm i -g @openhands/agent-canvas && agent-canvas` → onboarding → chat; agent calls canvas_ui to show results.
2. Docker-sandboxed laptop: `docker run` with PROJECTS_PATH mounted.
3. Programmatic SDK task: `LLM(...)` + `get_default_agent(llm)` → `Conversation(agent, workspace=...)` → `send_message` → `run()`.
4. Sandboxed remote conversation: `DockerWorkspace` context manager → RemoteConversation over WS.
5. Self-hosted VM control center: firewall → `--public` + API key → nginx+TLS.
6. Automated PR review via GitHub automation.
7. Eval-gated contribution: `run-eval-50` label → benchmarks → GCS → bot comments.
8. Swap engine mid-project (OpenHands ↔ ACP preset).
9. Repo-customized agent: commit setup.sh + AGENTS.md + skills + hooks quality gates.
10. Memory-accumulating maintainer: persistent memory + auto-condense.

## Rust / low-footprint notes
- The opposite pole of lean — value is architectural:
  - **Append-only typed event log as the only state and integration seam**; every capability (compaction, stuck-detection, hooks, resume, streaming) is a read-only observer → trivially optional modules.
  - Tool = `Action`/`Observation`/`ToolExecutor` triple with **ToolAnnotations (readOnly/destructive/idempotent/openWorld)** + global registry; per-conversation `create(conv_state)` factory.
  - **Workspace trait with Local/Remote swap** deciding in-proc vs client-server — maps to a Rust trait + enum dispatch.
  - Condenser as pure `events → View` transform emitting records → idempotent replay, no history mutation.
  - Inline `security_risk` argument instead of separate safety call.
  - Markdown-file agents resolved through registry with precedence.
  - Claude-Code-compatible hook contract — cheap to adopt, big ecosystem win.
  - Parallel tool-call regrouping by response id; python-literal-in-shell detection; shared browser executor across subagents; graceful tool-backend degradation.
- Process model: agent-server per host, one container per conversation, plain filesystem state (no DB in OSS).
