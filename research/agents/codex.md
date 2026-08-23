# Codex CLI (openai/codex, Rust) — Feature Extraction

> Source: research/repos/codex clone (codex-rs/ ~130 crates: protocol, core, app-server, exec, tui, rollout, config, execpolicy, linux-sandbox, memories, skills, features...). Apache-2.0, edition 2024. Scout: CodexRust.

## Identity
- One multitool `codex` binary: subcommands (default TUI), agents, exec, review, mcp-server (deprecated), mcp, plugin, app-server (+daemon, proxy, generate-ts/json-schema), remote-control, app, resume, queue, archive/delete/unarchive/fork, login/logout, completion, update, doctor, cloud, sandbox, debug, execpolicy, apply, responses-api-proxy, stdio-to-uds, exec-server, features, migrate-rollouts. Standalone `codex-tui`, `codex-linux-sandbox`, Windows sandbox, `codex-exec-server`.
- **arg0 multitool dispatch**: same binary re-execs as `codex-linux-sandbox` when argv0 says so, and simulates the virtual `apply_patch` CLI via arg1 — avoids shipping helper binaries.
- Footprint: heavy (tokio, reqwest+rustls, ratatui+syntect, tree-sitter, sqlx, starlark, rmcp, v8, otel+sentry, image, symphonia, zstd, gix). Release profile thin-LTO, line-tables-only, codegen-units=4.

## Core loop & orchestration
- Core engine talks to UIs over **SQ/EQ queue pair** (Op submissions → Event/EventMsg; in-process Rust types, NDJSON-serializable).
- Entities: Model (Responses API) → Codex → Session (reconfigured by Op; reconfigure aborts running task) → **Task** (max one per session; aborted by new input/interrupt/fatal error/approval block) → **Turn** (one model request via SSE/WebSocket → tool exec → outputs feed next turn; `response_id` bookmark for resume/fork).
- Surfaces all drive one core: TUI (via app-server client), `codex exec` headless, Codex-as-MCP, `app-server` (JSON-RPC 2.0 for VS Code ext + SDKs), `exec-server` (PTY control, local/remote).
- **Multi-agent**: v1 `multi_agent_v1` + v2 `collaboration`; agent registry/roles, nicknames, spawn edges persisted, parent-owned subagents, concurrency limits.
- **Guardian**: policy-driven automatic reviewer assessing risky tool calls/approvals; auto-review denial retry.
- Realtime voice conversation mode (WebRTC/WebSocket). Turn steering, queued turns (≤100 FIFO/thread), background terminals (`!cmd`, `/ps`, `/stop`).

## Tool catalog
1. `exec_command` — unified PTY-backed shell; `sandbox_permissions` (`sandboxed`|`with_additional_permissions`|`require_escalated`), additional_permissions (network/file_system), justification, timeout_ms, environment_id
2. `write_stdin` — write to running PTY
3. `apply_patch` — model-emitted patch format (freeform or function tool; virtual CLI via arg0); `codex apply` CLI
4. `update_plan` — step/progress plan
5. `view_image` 6. `request_user_input` (1–3 questions w/ options + free-form)
7. `request_permissions` — mid-turn permission-profile bump
8. `request_plugin_install` / `list_available_plugins_to_install`
9. `tool_search_tool` — deferred/dynamic tool discovery (MCP tools always deferred)
10. `get_context_remaining`, `new_context` — window introspection + fresh-window restart
11. `send_user_message_async` 12. `clock`: `sleep` (≤12h), `curr_time`
13. `wait_for_environment` — remote exec-server startup
14. `multi_agent_v1`: spawn_agent, send_input, resume_agent, wait_agent, close_agent
15. `collaboration` (v2): spawn_agent, wait_agent, send_message, interrupt_agent, list_agents, followup_task
16. MCP tools `mcp__<server>__<tool>` + resources
17. Web search (hosted Responses API or extension `web.run`); `image_gen.imagegen`
18. **Code Mode**: freeform JS `exec` tool + `wait` (v8 runtime) — model writes JS that calls tools
19. Legacy `shell`; client-supplied `dynamicTools` (experimental)

## Context management
- Compaction: `/compact`; local summarizer + remote compaction v2 over Responses API + model fallback + encrypted parent-compaction reuse; auto-compact per model.
- **TokenBudget** feature adds context-window metadata; **RolloutBudget** shared across a session's agent threads; goal states budgetLimited/usageLimited.
- AGENTS.md durable instructions (root→cwd concat, override file, 32KB cap, root markers).
- **Memories (two-phase)**: Phase 1 extracts per-rollout structured memories into SQLite (claim/lease, retry backoff, secret redaction); Phase 2 single-lock consolidation agent updates `~/.codex/memories` (git-baselined: raw_memories.md, rollout_summaries/, MEMORY.md, skills/) from a generated workspace diff; read path injects memory instructions **with citations**.
- Prompt caching: `prompt_cache_key` per session; `previous_response_id` resumption; **WebSocket session prewarm (generate=false) + sticky routing header**; incremental request reuse.
- Output truncation: per-model `truncation_policy` (tokens/bytes).

## Extensibility
- MCP client (rmcp): stdio + streamable-http; OAuth (auto/cimd/dcr); elicitation; per-tool approval modes; MCP 2026-07-28 flag; hot reload.
- **Codex-as-MCP-server**: thread/start|resume|fork|read|list, turn/start|steer|interrupt, account/config/model RPCs, `codex/event/*` streams, server→client approvals.
- Plugins: git marketplaces, remote ChatGPT catalog; bundle skills/hooks/apps/MCP servers; admin install policies.
- Skills: SKILL.md frontmatter, embedded system skills via `include_dir`, `$skill` explicit mentions, implicit invocation detection, file-watch.
- Hooks: Claude-style hooks.json; events PreToolUse, PermissionRequest, PostToolUse, Pre/PostCompact, SessionStart/End, UserPromptSubmit, SubagentStart/Stop, Stop; handler types Command/McpTool/Prompt/Agent; sources System/User/Project/MDM/SessionFlags/Plugin/Cloud; `allow_managed_hooks_only`.
- Extensions as crates (`ext/*` on codex-extension-api). Subagents/roles via `[agents]` config. Apps/connectors `app://<connector-id>`.
- ~55 TUI slash commands; **NO user-defined custom commands**.

## Safety & permissions
- Approval policy: `untrusted` (everything prompts unless exec-policy allows), `on-request` (default, model decides), `never`.
- **Sandbox policies**: `danger-full-access`, `read-only`, `workspace-write`; superseded by split `[permissions]` profiles — per-path fs entries (read/write/none), special paths, deny globs w/ depth caps, `extends` inheritance; built-ins `:read-only`, `:workspace`, `:danger-full-access`.
- **Per-OS enforcement**: macOS Seatbelt; Linux bubblewrap (system or vendored fallback; `--unshare-user/pid/--unshare-net`, ro-bind layering w/ nested deny/reopen, fresh /proc, PR_SET_NO_NEW_PRIVS + seccomp; legacy Landlock+seccomp); Windows unelevated restricted-token + elevated setup flow; WSL1 fails closed.
- **Network**: managed proxy — sandboxed `--unshare-net` + TCP→UDS→TCP bridge; per-host/protocol approvals; seccomp blocks new AF_UNIX after bridge.
- **execpolicy (Starlark)**: `prefix_rule(pattern, decision, match, not_match)` + `host_executable`; strictest-match; per-project `.rules` gated by trust.
- Guardian risk levels low→critical; model reroute on HighRiskCyberActivity; safety-buffered retry.
- Project trust gates project-local config/hooks/exec-policy/AGENTS; worktree trust checks.
- Enterprise: `requirements.toml` (MDM/cloud) constraining login, approval policies, sandbox modes, model catalogs, hooks lockdown.
- Secrets: keyring w/ age/crypto_box/ML-KEM options; RedactedString; command-backed bearer tokens; AWS SigV4 refresh.

## Model/provider abstraction
- Built-ins: `openai` (ChatGPT auth, websocket-capable), `amazon-bedrock` (SigV4), `ollama`, `lmstudio`. User-defined `[model_providers.<id>]`: base_url, env_key, **wire_api (`responses` only — `chat` removed)**, headers, bearer, retries, websocket flags.
- **Model catalog models.json (414KB)**: context windows, reasoning efforts (none→ultra — `ultra` enables task delegation), modalities, tool modes (`code_mode_only`), apply_patch type, web_search type, truncation policy, multi_agent_version, prefer_websockets, service tiers, specialties.
- Auth: ChatGPT (browser + device code), API key, Bedrock; Agent Identity JWTs for containers; attestation.
- Transport: Responses over SSE or WebSocket (v2 prewarm, sticky turn state, incremental payloads), zstd compression, service_tier, verbosity, personality, Fast mode.
- Local models: `--oss`/`--local-provider` via ollama/lmstudio.

## Surfaces
- TUI (ratatui): vim mode, remappable keymaps, themes (syntect), statusline/title, terminal pets, transcript export, raw scrollback, image rendering, fuzzy search (nucleo), mention popups, diff viewer, background terminals, steer queue, plan-mode cycling (shift+tab).
- Headless: `codex exec` `--json` (JSONL events), `-o last-message`, **`--output-schema` (JSON-Schema-constrained final answer)**, `--ephemeral`, resume/fork; `codex review --uncommitted|--base|--commit`.
- app-server: bidirectional JSON-RPC 2.0 over stdio/UDS (+proxy, websocket); generated TS/JSON schemas; `app-server-daemon` w/ pidfiles, hourly self-updater.
- SDKs TS + Python driving app-server. MCP server mode. exec-server (local WS or **Noise-relayed protobuf frames w/ segment ack/retry** for remote environments). `codex remote-control` (pairing codes, mobile/desktop).
- IDE: VS Code extension runs app-server; `/ide` folds IDE context (active file, selection, open files) into prompts.

## Session & collaboration
- **Rollout persistence**: `~/.codex/sessions/rollout-<ts>-<thread-id>.jsonl`; first line SessionMeta (id, cwd, git sha/branch/origin, provider, agent nickname/role); lines = ResponseItem | InterAgentCommunication | Compacted | TurnContext | WorldState | SecurityRiskScore | EventMsg; **zstd compression of cold rollouts**; reverse JSONL scanner for cheap tails.
- SQLite state DB (thread metadata/index, names, queues, memories stage, goals) — backfill from rollouts w/ leases.
- Thread ops: resume, fork (turn boundaries), rename, archive/delete (cascade to descendants), rollback, revert, **thread sections (pinned + custom)**, search, 30-min idle unload w/ SessionEnd hooks.
- Queued turns (≤100) w/ edit/reorder/start. **Side conversations** (`/side`/`/btw`) ephemeral forks w/ inherited-history boundary. Goals per-thread w/ auto-continuation. Teams: spawn edges, `/agents` global session browser across shared daemon.
- Review flows: inline or detached review threads. **Import from Claude Code** (`/import`).

## Config & conventions
- `CODEX_HOME` (~/.codex): config.toml, requirements.toml, auth.json, sessions/, memories/, skills/, SQLite DB.
- Layer stack: managed (MDM) → system `/etc/codex/config.toml` → enterprise cloud → user → profile `<name>.config.toml` → cwd → repo-tree `.codex/config.toml` → runtime flags (`-c dotted.path=value`); project layers disabled when untrusted; strict-config rejects unknown fields.
- Profiles (`[profiles.name]`); AGENTS.md discovery w/ root markers; **feature flags** (~100 staged UnderDevelopment→Removed incl. UnifiedExec, CodeMode, Collab, MultiAgentV2, NetworkProxy, Plugins, GuardianV2, RealtimeConversation); config.schema.json 182KB generated.

## Distinctive features
- Single multitool binary via argv0/arg1 dispatch; vendored fallback bwrap.
- **exec-server remote environments**: Noise-relayed protobuf WebSocket — run tools inside remote/containerized machines selected per turn.
- Realtime voice conversations in a terminal agent.
- Guardian auto policy review + model rerouting.
- Two-phase memory pipeline with git-baselined workspace + memory citations.
- Responses-over-WebSocket prewarm + sticky turn state + incremental reuse; zstd bodies.
- Enterprise requirements layering; Windows private-desktop sandbox.
- `codex exec --output-schema`.
- TUI extras: terminal pets, raw scrollback, `/side`, IDE context injection.

## Canonical workflows
1. Interactive coding: session from profile → plan (`update_plan`) → exec/apply_patch under sandbox w/ approvals → /diff /review /status; shift+tab plan mode; /model effort.
2. Headless CI: `codex exec "fix..." --json -o last.txt --output-schema schema.json --ephemeral`.
3. Long sessions: auto-compact or /compact; get_context_remaining/new_context; resume --last; fork; /export.
4. Code review: `codex review --uncommitted`; Guardian auto-reviews approvals.
5. MCP: `codex mcp add` or streamable-http w/ OAuth; tool_search_tool; per-tool approvals; Codex-as-MCP for other clients.
6. Skills & plugins: SKILL.md or marketplace; `$skill` mentions; request_plugin_install flow.
7. Multi-agent: high/ultra effort or /subagents → spawn_agent/send_message/wait_agent; /agents browser.
8. Remote environments: exec-server on remote box → environment/add → per-turn env selection.
9. Enterprise lockdown: requirements.toml; Windows /setup-default-sandbox.
10. Memory accumulation: background extraction → consolidation → MEMORY.md w/ citations.

## Rust / low-footprint notes (directly applicable to Ka)
- **Architecture to copy**: strict crate unbundling — `protocol` (Op/EventMsg wire types), `core` (engine), `app-server-protocol`+`app-server` (JSON-RPC w/ generated schemas), `tui`, `exec`, `rollout`, `config`, `model-provider-info`, tiny `utils/*` (absolute-path, pty, fuzzy-match, redacted-string, output-truncation). **Engine decoupled from UI via two mpsc queues; events are serde enums.**
- Lean core achievable: providers = base_url + env_key + responses wire; model catalog = static JSON; prompts = per-model markdown compiled in (`include_dir` + templates).
- **Sandboxing without root**: bwrap user-namespace default w/ vendored fallback binary; landlock+seccomp legacy; seatbelt profile strings; arg0 re-exec trick to run the sandbox child from the main binary.
- **Session persistence = plain append-only JSONL** w/ first-line meta + reverse scanner + optional zstd of cold files + SQLite only as rebuildable index.
- Perf: WebSocket prewarm & incremental request reuse, prompt cache keys, thin-LTO, line-tables-only, clippy deny unwrap/expect/clone-heavy, per-crate caching.
- Heavy deps to avoid for minimal footprint: v8, tree-sitter, syntect, sqlx-bundled, starlark, otel/sentry, image/symphonia, rmcp (unless MCP), ratatui (TUI only). **A minimal codex-like agent = protocol + core + config + model-provider + rollout + bwrap/landlock sandbox + one surface.**
