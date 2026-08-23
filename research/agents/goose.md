# Goose (block/aaif, Rust) — Feature Extraction

> Source: research/repos/goose clone (crates/*, ui/desktop, documentation/docs). Rust workspace, Apache-2.0, v1.47. Scout: Goose.

## Identity
- Upstream `aaif-goose/goose` (formerly block/goose; AAIF/Linux Foundation). Rust workspace + Electron/React desktop.
- Crates: `goose` (core: agent, scheduler, sessions, config, permissions, hooks, plugins, skills, recipes, ACP server), `goose-cli`, `goose-mcp` (bundled MCP extension servers), `goose-providers`/`goose-provider-types`, `goose-context-management`, `goose-local-inference` (llama-cpp-2 + candle), `goose-download-manager`, `goose-sdk` (uniffi → Python/Kotlin), `goose-acp-macros`. Vendored V8 for code mode.
- Key deps: rmcp 3.0 (official Rust MCP SDK), agent-client-protocol 2.0, tokio, axum, clap, reqwest/rustls, keyring, tokio-cron-scheduler, tree-sitter (7 langs), schemars, nostr-sdk, llama-cpp-2, candle, otel, posthog, whisper. Features gate telemetry/code-mode. Footprint NOT lean overall, but the minimal core is.

## Core loop & orchestration
- Interface (desktop/CLI) → Agent (core loop) → Extensions (MCP). Errors (invalid JSON, unknown tool) returned to the model as tool responses (self-healing loop).
- Agent core driven by explicit **state machine** (`agents/state_machine/ops_*.rs`): toolcalling, tool approval, tool-pair compaction, stop hook, **steering** (mid-turn interrupt/queue), slash commands, skills, retry, recipe, project, maxturns, llm, exit_on_error, entry hook, doctor, compaction, bang shell.
- Multi-agent: **Summon** subagents (parallel/sequential, own context/extensions/max-turns), Orchestrator extension managing separate sessions, `goose review` multi-check fan-out, external agents as subagents (Codex as MCP `subagent`), ACP agents as providers (Claude/Codex via ACP).
- Max turns 1000 w/ continue-prompt; `--max-tool-repetitions` loop guard.

## Tool catalog
**developer** (in-process platform ext, default): `write`, `edit` (unique match), `shell` (2000-line cap/stream, overflow→temp file, cancellation kills child), `tree` (gitignore-aware w/ line counts), `read_image`.
**analyze** (tree-sitter, default): `analyze` (directory overview / file details / symbol call graphs).
**todo** (default): `todo_write`.
**apps**: `list_apps`, `create_app`, `iterate_app`, `delete_app`, `create_app_content`, `update_app_content` (LLM-generated sandboxed HTML apps).
**chatrecall**: `chatrecall` (search past sessions).
**extensionmanager** (default): `search_available_extensions`, `manage_extensions` (enable/disable w/ user approval), `list_resources`, `read_resource`.
**scheduler** (hidden): `manage_schedule` (list/create cron from recipe, run_now, pause, kill, inspect, sessions).
**summon** (default): `load` (knowledge/recipes/agents), `delegate` (subagent execution, sequential or parallel).
**summarize**: `summarize` (file/dir + LLM summary in one call).
**code-mode** (feature-gated): `list_functions`, `get_function_details`, `execute_typescript` (vendored V8), `execute_bash`.
**orchestrator** (hidden): `list_sessions`, `view_session`, `start_agent`, `send_message`, `interrupt_agent`.
**tom** (Top Of Mind): context injection every turn.
**skills**: `load_skill`.
**goose-mcp bundled servers** (also standalone `goose mcp <name>`): memory (`remember_memory`, `retrieve_memories`, remove ×2), computercontroller (`xlsx_tool`, `docx_tool`, `pdf_tool`, `computer_control`), tutorial, autovisualiser (`show_chart`, `render_sankey/radar/donut/treemap/chord/map/mermaid` via `ui://` MCP-UI resources).
**recipes**: `recipe__final_output` (structured output collection).
Plus external MCP servers, namespaced `name__tool`.

## Context management
- **Auto-compaction** at 80% of context limit (GOOSE_AUTO_COMPACT_THRESHOLD); smaller/faster model summarizes; manual `/compact`; customizable `compaction.md` prompt template.
- **Tool-output summarization**: background summarization of old tool calls (`compute_tool_call_cutoff`, `protect_last_n`, `maybe_summarize_tool_pairs`).
- **Context-limit strategies**: `summarize | truncate | clear | prompt` via GOOSE_CONTEXT_STRATEGY.
- Token budget UX: usage indicator, cost estimate. **Code Mode** (agent writes TS to batch tool calls), priority filtering, shell truncation, tree/analyze instead of full reads, `<25 tools performs best` guidance.
- Context files: `.goosehints` global + per-directory nested (deeper ones loaded as agent touches subdirs); `@file.md` includes; AGENTS.md compat.
- Memory: memory extension (categorized/tagged), chatrecall cross-session, top-of-mind each turn.

## Extensibility
- **6 extension types**: `stdio`, `streamable_http` (OAuth DCR/pre-registered/PKCE), `builtin`, **`platform` (in-process)**, `frontend` (tools implemented by desktop UI), `inline_python` (uvx w/ deps). Config `extensions:`; deeplinks `goose://extension`; mid-session `/extension`.
- **MCP server mode**: bundled extensions as standalone MCP servers. MCP features: roots, sampling, elicitation, resources, MCP-UI.
- **Recipes** (signature): YAML/JSON packaging instructions + prompt + extensions + parameters (Jinja) + activities + settings + **retry (max_retries, shell success checks, on_failure cleanup)** + response (structured output schema) + **sub_recipes** (parallel/sequential composition); created from live session via `/recipe`; validated; deeplinks; GitHub repo sharing; first-run trust dialog.
- **Subagents**: internal (recipes/prompts; 25 max turns, 5-min timeout) + external (any agent as stdio MCP `subagent`).
- **Custom agents**: markdown + frontmatter in `~/.agents/agents/` or project; `@name` invoke.
- **Skills**: SKILL.md, agentskills.io-compatible, .claude compat; built-in web-search skill; lazy-loaded bodies.
- **Plugins** (Open Plugins spec): plugin.json + hooks/ + skills/ + agents/; git install.
- **Hooks**: shell commands on 13 events (SessionStart/End, Stop, UserPromptSubmit, PreToolUse, PreToolUseResult, PostToolUse, PostToolUseFailure, BeforeReadFile, AfterFileEdit, Before/AfterShellExecution); regex matcher; JSON stdin; deny via exit 2 or `{"decision":"block"}`; fails open.
- **All system prompts overridable files** (system.md, plan.md, recipe.md, compaction.md, subagent_system.md, session_name.md, permission_judge.md...).
- **Goose Apps**: sandboxed HTML apps; MCP-UI; custom distros.

## Safety & permissions
- **Modes**: `auto` (default), `approve`, **`smart_approve` (LLM risk-based)**, `chat`.
- Per-tool permissions: Always/Ask/Never (permission.yaml + runtime).
- **Adversary mode**: `adversary.md` natural-language BLOCK/ALLOW policy → silent second-model reviewer watching each tool call; fail-open; context-aware.
- Prompt-injection detection: pattern-based + optional ML classifier endpoint.
- Extension safety: **malware scan of external extensions before activation**; extension allowlist.
- Scheduler hardening: O_NOFOLLOW, 0600, 1MB caps, generic errors.
- Secrets: OS keyring (keyring crate) w/ plaintext fallback (headless/CI); env precedence.
- `--container <id>` Docker for extensions; CLI-provider permissions routed through goose UI.

## Model/provider abstraction
- ~40+ providers: Anthropic, OpenAI (+any OpenAI-compatible), Google (+Vertex), Azure (×2), Bedrock, SageMaker, Databricks, Ollama, OpenRouter, Groq, Mistral, xAI (OAuth), Cerebras, Snowflake, Copilot (device-flow), ChatGPT Codex (browser OAuth), Perplexity... + **declarative JSON-defined custom providers**.
- **CLI providers** (`cursor-agent`); **ACP providers** (`claude-acp`, `codex-acp`) — goose delegates to external agents, passing goose extensions through as MCP servers.
- **Local**: in-process llama.cpp/candle + GGUF/MLX model manager from HuggingFace + Whisper dictation.
- Prompt caching: automatic Anthropic cache_control on several providers; `supports_cache_control` flag.
- **Planner/provider split** (GOOSE_PLANNER_*); reasoning capture (DeepSeek/Kimi reasoning_content); **Toolshim** — interprets tools for models without native tool-calling.

## Surfaces
- CLI: interactive session (markdown themes via bat, editor integration), `goose run` headless (`--output-format json|stream-json`, `--no-session`), `goose tui`, **`goose term`** shell integration (`@goose` prompt prefix, command-not-found handler, named sessions), `goose schedule`, `goose recipe/skills/plugin/review/gateway/info/doctor/update/acp/serve/mcp-probe/local-models`.
- **ACP server** (`goose acp` stdio for Zed/JetBrains); **`goose serve`** (ACP over HTTP/WS + TLS, shared secret, cert fingerprint pinning, `--platform desktop --enable-scheduler`) → remote desktop backend.
- Desktop (Electron+React): multi-session sidebar, recipes library + deeplinks, extensions manager, schedules UI, Apps windows, quick launcher, dictation, i18n, session search, token meter; spawns/manages `goose serve` child.
- SDKs: Python & Kotlin via uniffi; TS SDK. Gateway: Telegram bot + pairing codes. CI via `goose run --output-format json`.

## Session & collaboration
- SQLite `sessions.db` (legacy .jsonl auto-imported); AI session naming. Resume (`-r`), **fork**, **edit-in-$EDITOR as YAML** then continue or fork, export markdown, **import** from Claude Code / Codex / Pi `.jsonl` transcripts + goose:// links.
- **Nostr sharing**: NIP-44-encrypted session events published to relays → `goose://sessions/nostr` deeplinks.
- Search: within-session, across-session (desktop + chatrecall + SQLite); orchestrator agent-to-agent messaging; review flow (`.agents/checks/*.md`, REVIEW.md, severity + per-check model).

## Config & conventions
- `~/.config/goose/config.yaml` (provider/model, GOOSE_* settings, extensions, slash_commands); `permission.yaml`; `secrets.yaml`; `prompts/` overrides. Precedence: env > config > defaults.
- Project: `AGENTS.md` + `.goosehints`; `.agents/{skills,agents,checks,plugins}/`; `.claude/*` compat.
- Data: sessions `~/.local/share/goose/sessions`, logs `~/.local/state/goose/logs`.

## Distinctive features
- **Recipes ecosystem** (session→recipe, subrecipes, retry-with-shell-checks, structured response, GitHub sharing, deeplinks) — most complete workflow-packaging story surveyed.
- **Scheduler as first-class capability**: cron over recipes, manageable from inside chat (`manage_schedule`).
- Adversary-mode LLM policy reviewer + prompt-injection detection + extension malware scanning.
- **Goose Apps** + MCP-UI charts — "agent with a GUI".
- Terminal integration (`@goose` shell function w/ history context); goose as ACP server AND other agents as ACP providers.
- Nostr-encrypted session sharing; Telegram gateway; remote `goose serve` w/ cert pinning.
- In-process local inference + HF model manager + Whisper in one binary.
- Open-Plugins plugins + 13-event hooks; tools-as-platform-extensions (no subprocess for builtins).

## Canonical workflows
1. Interactive dev: tree/analyze → shell → edit → tests → smart_approve gates → `/compact`.
2. Headless CI: `goose run -i instructions.md --output-format json --no-session --max-turns 25`.
3. Recipe lifecycle: session → `/recipe` → edit params → share repo/deeplink → run w/ param dialog → cron schedule → inspect.
4. Subagent fan-out: summon `delegate` ×3 parallel, or sub_recipes, or external Codex subagent.
5. Plan-then-execute: `/plan` w/ separate planner model → Q&A → `/endplan` → clear history & act.
6. Code review: `goose review main...HEAD` w/ `.agents/checks/*.md` subagent reviewers.
7. Terminal-native: `eval "$(goose term init zsh)"` → `@goose why did this fail?` w/ history.
8. Remote backend: `goose serve --tls --enable-scheduler` on VM → desktop connects w/ secret+fingerprint.
9. Telegram companion: `goose gateway start telegram` → pair.
10. Fully-local: `goose local-models search/download` → local provider + toolshim + whisper, offline.

## Rust / low-footprint notes
- **Clean layering worth copying**: `goose` core lib vs `goose-providers` (Provider trait + per-vendor + declarative JSON) vs `goose-mcp` vs thin `goose-cli`; UIs are pure ACP clients over stdio/HTTP — process boundary between UI and agent.
- **Built-in tools are in-process platform extensions implementing `McpClientTrait`** (list_tools/call_tool/get_info) — no subprocess for builtins; only external MCP servers spawn processes.
- Tool defs via rmcp `Tool::new` + `schemars::schema_for!::<Params>()` + **ToolAnnotations (readOnly/destructive/open-world, priority for output filtering)**.
- **Turn lifecycle as composable state-machine ops** — each independently testable.
- MCP via rmcp 3.0 `#[tool]`/`tool_router` proc macros for server-side extensions.
- Scheduler: tokio-cron-scheduler, persisted jobs, CancellationToken registry, hardened file IO.
- Token discipline: output schemas, 2000-line shell caps w/ slot-rotated temp files, background tool-pair summarization, priority-based output rendering.
- Misc: keyring vendored, rustls (aws_lc_rs), axum for serve, tree-sitter, once_cell Lazy static platform-extension registry, uniffi FFI, dev-profile debug stripping.
