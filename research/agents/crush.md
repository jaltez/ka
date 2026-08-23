# Crush (charmbracelet) — Feature Extraction

> Source: research/repos/crush clone (README, docs/, internal/*). Go 1.26, FSL-1.1-MIT. Scout: Crush.

## Identity
- Single binary `crush`; brew/npm/apt/winget/scoop/nix/FreeBSD/go install. Platforms incl. Windows, Android, FreeBSD, illumos.
- Architecture: **TUI-first (bubbletea v2 + lipgloss v2 + glamour v2)**, headless `crush run`, HTTP/SSE **server mode** (`crush server`, ~60 `/v1/*` endpoints + swagger) and client mode (`--host`), SQLite persistence (goose migrations + sqlc), **embedded POSIX shell (mvdan.cc/sh v3)** used for the bash tool, hooks, and config.
- LLM layer delegated to external libs: **`charm.land/fantasy`** (agent loop, AgentTool, providers) + **`charm.land/catwalk`** (model/provider catalog w/ prices, context windows, reasoning levels) + internal `hyper` first-party provider. LSP via `charm.land/x/powernap`.

## Core loop & orchestration
- `SessionAgent`: per-session turn runner with **message queue** (prompts submitted while busy are queued and drained), per-session cancel (accept-sequence high-water marks so cancel never poisons later prompts), RunID completion correlation, `OnAuthRefresh` → transparent in-place retry on HTTP 401.
- `coordinator.go`: parallel agent init (system prompt + tools + MCP gate), PreToolUse hook wrapping, large/small model slots, config reload, AWS SSO refresh.
- Two built-in agents: **coder** (all tools) and **task** (read-only subset incl. LSP + sourcegraph; no MCP).
- **Loop detection**: SHA-256 signature over (tool+params+result); >5 repeats in 10-step window breaks the loop.
- Auto **title generation** in child `title-<id>` session. Estimated-usage fallback when provider reports zero usage.

## Tool catalog
1. `bash` — embedded mvdan/sh interpreter (cross-platform incl. Windows); auto-backgrounds >60s; output capped 30k chars; banned-commands list; safe read-only allowlist auto-executes only without chaining metacharacters; full git-commit & PR conventions embedded
2. `edit` — exact string match 3. `multiedit` — batched 4. `write` — LSP-formatted
5. `view` — paged read w/ line numbers, renders images, records filetracker reads, marks skills loaded
6. `ls` 7. `glob` (doublestar, gitignore-aware) 8. `grep` (in-Go, 5s timeout)
9. `fetch` (100KB cap) 10. `agentic_fetch` — browsing sub-agent (own tmp dir; web tools) that follows links to answer
11. `download` 12. `sourcegraph` — public code search
13. `web_search` (DuckDuckGo) / `web_fetch` — sub-agent-only
14. `agent` — parallel task sub-agent; child session keyed by messageID+toolCallID
15. `todos` — per-session list, TUI pills
16. `question` — interactive Q&A (top-level only)
17. `job_output` / `job_kill` — background shells
18. `crush_info` — introspect runtime state 19. `crush_logs` — tail own log
20. LSP tools: `lsp_diagnostics`, `lsp_references`, `lsp_symbols`, `lsp_definition`, `lsp_call_hierarchy`, `lsp_rename`, **`lsp_replace_symbol`** (structural insert/replace/delete by symbol), `lsp_restart`
21. `list_mcp_resources`, `read_mcp_resource` + `mcp_<server>_<tool>`

## Context management
- **Auto-summarize**: context-window-aware threshold (>200k → 20k buffer; smaller → 20% ratio); large-model summary w/ todos folded in; history trimmed to start at summary; manual endpoint.
- Context files auto-loaded: `.github/copilot-instructions.md`, `.cursorrules`, `.cursor/rules/`, `CLAUDE.md`/`CLAUDE.local.md`, `GEMINI.md`, `crush.md`, `AGENTS.md` (dedup, recursive walk); global CRUSH.md + AGENTS.md.
- `.gitignore` + `.crushignore` everywhere. `initialize` writes AGENTS.md from codebase analysis.
- **filetracker**: SQLite record of file reads per session → staleness detection for edits ("file changed since read").
- Per-model max_tokens; cache pricing for cost accounting.

## Extensibility
- **MCP** (stdio/http/sse, official go-sdk): per-server tool lists, OAuth 2.1 (dynamic + pre-registered), prompts as slash commands, hot reconfigure (generation-numbered restarts), **channels** (server pushes sanitized `<channel>` elements into session), Docker MCP command, per-agent AllowedMCP, TUI state card.
- **LSP**: auto-discovery via root markers, lazy start, per-server config.
- **Skills** (agentskills.io standard): builtin embedded (crush-config, crush-hooks, jq); discovered from many roots incl. `.claude/skills`; `user-invocable` → command palette; `<available_skills>` XML in system prompt; model MUST `view` SKILL.md before use.
- **Hooks**: PreToolUse only (Claude-Code-compatible); matcher regex; parallel exec via embedded shell; first deny wins, last input-rewrite wins; exit 2 = block, **exit 49 = halt turn**, JSON envelope w/ `decision:allow` pre-approving; timeout → non-blocking abandon.
- **Custom slash commands**: markdown in config/command dirs; `$UPPERCASE` args; MCP prompts as commands.
- Config-definable agents with tool/MCP allowlists.

## Safety & permissions
- Default: prompt before every tool; **allow (a) / allow for session (s) / deny (esc)**; `allowed_tools` config; `permissions deny` hides tools.
- `--yolo` / ctrl+y. Bash banned commands + safe allowlist (no chaining). Hook deny blocks before permission prompt; hook allow pre-approves.
- Secrets: `$VAR`/`$(cmd)` expansion (1Password/op), crushrc documented as trusted code; data dirs never execute config.
- Metrics: pseudonymous PostHog, opt-out.

## Model/provider abstraction
- Catalog from **Catwalk** (community DB auto-updated; embedded fallback); types: openai, openai-compat, anthropic, hyper, google, google-vertex, bedrock, azure, vercel, openrouter + local (ollama, llamacpp, lmstudio, litellm, omlx) with **model auto-discovery** via /v1/models.
- **large/small model slots** (small for titles/summarization); mid-session switch preserving context; per-model overrides incl. `reasoning_effort`, anthropic `think`.
- Provider extras: `--system-prompt-prefix`, `--extra-header`, `--extra-body` JSON merge, `aws_auth_refresh` command then transparent retry, flat-rate billing, copilot config import, GitHub Copilot OAuth.
- Usage/cost per session; `crush stats` HTML report.

## Surfaces
- **TUI**: ctrl+p command palette, ctrl+l model picker (+reasoning-effort picker), ctrl+s session manager (busy/attached badges), ctrl+t pills, ctrl+d usage, ctrl+o external editor, ctrl+f file/image picker, ctrl+y yolo toggle, Kitty-aware keys, esc interrupt; compact mode, split/unified diff, themes, fuzzy path completions.
- Onboarding wizard; notifications (auto/native/osc/bell; OSC 99/777 over SSH; only when unfocused).
- **CLI**: `crush run` (stdin pipe, --quiet/--model/--session/--continue), `crush session list|show|last|delete|rename` (--json), `crush models|projects|logs|stats|schema|login|update-providers|server`.
- **Server**: REST + SSE (~60 endpoints: workspaces, sessions, messages, filetracker, LSPs, permissions grant/skip, questions, run/cancel, summarize, config, MCP), swagger UI.
- **Workspace/client model**: multiple TUIs share one backend keyed by `--cwd` — shared sessions, history, permission queue, LSP/MCP state; live mirror of in-progress sessions; torn down when last SSE stream closes.
- **herdr integration**: reports agent state (idle/working/blocked) to the terminal multiplexer.

## Session & collaboration
- SQLite per project; sessions with parent_session_id, title, token counts, cost, todos, summary_message_id; **xxh3 7-char hash IDs**.
- Session types: main, task sub-agent, title. Resume via `-s <hash>`/`--continue`; exit prints resume hint.
- Busy sessions queue prompts; cancel in-flight; multi-client live attach. No fork/branch/teams.

## Config & conventions
- **`crushrc` = Bash config**: full shell logic, `source`, version gating; builtins: `provider add/remove`, `model add/remove/large/small`, `mcp add/remove`, `lsp add/remove`, `hook add`, `permissions allow/deny`, `option <key>`. Precedence: `./.crushrc` > `./crushrc` > XDG config; JSON deprecated but merged.
- Attribution: `Assisted-by: Crush:<ModelID>` trailer / co-authored-by / none.
- State in `~/.local/share/crush` (never executed).

## Distinctive features
- **Bash as the config language** running on the same embedded shell as the bash tool — config is self-programmable (the agent configures itself via the crush-config skill).
- **Embedded POSIX shell everywhere** (bash tool, hooks, config) → identical behavior on Windows without WSL.
- **LSP-native editing**: `lsp_replace_symbol` + `lsp_rename` as first-class edit tools, preferred over text edits.
- **MCP channels** pushing validated `<channel>` elements into live sessions.
- **Client/server workspaces** with REST+SSE, swagger, multi-client live session mirroring.
- **herdr** multiplexer state reporting.
- Skill-read enforcement loop; `crush_info` self-introspection; `crush_logs`.
- Community model DB (Catwalk) auto-updated + embedded fallback; Hyper first-party subscription.
- Exit 49 halt-turn hook semantics; loop detection via interaction-signature hashing; xxh3 session hashes.

## Canonical workflows
1. First run: onboarding → model picker → paste key/OAuth → optional AGENTS.md init.
2. Interactive coding: search → edits (incl. lsp_replace_symbol) → permission (a/s/esc) → tests via bash → todos pills → ctrl+d usage.
3. Headless: `cat file | crush run "summarize"`; `--continue` follow-ups; JSON session CLI.
4. Session management: ctrl+s picker (busy/attached badges) → rename/delete; `crush -s <hash>`.
5. Self-configuration: tell Crush "add an Ollama provider and allow bash" → crush-config skill writes crushrc.
6. MCP: `mcp add github --type http ...` → `mcp_github_*` tools; per-agent AllowedMCP scoping.
7. Policy via hooks: block `git push -f`, scrub secrets; deny blocks before any prompt.
8. Multi-client: `crush server` → multiple `crush --host` TUIs share the workspace; one drives, others watch.
9. Local models: `provider add ollama` → auto-discovery → `model large ollama/qwen3:30b`.
10. Long-running commands: auto-background after 60s → job_output/job_kill.

## Rust / low-footprint notes
- **One process**: LLM I/O, shell (embedded interpreter — no subprocess for bash/hook/config), DB, TUI; children only for LSP, stdio MCP, shebang scripts. Rust lacks an mvdan/sh equivalent — either accept subprocess bash or embed a small interpreter.
- Storage: single SQLite per project w/ migrations (note ncruces sqlite is WASM/heavy); alternative: append-only JSONL.
- **Concurrency**: `csync` tiny lock-wrapped generics + `pubsub.Broker[T]` event buses for every subsystem — cheap with tokio mpsc/broadcast.
- **Tool definitions**: each tool = one file + embedded `.md` template (env-conditional docs); one struct per tool with context keys.
- **Layered design**: core loop in a library, providers behind a trait, harness adds queueing/cancel/summarize/hooks — validates keeping Ka's loop small.
- Perf: xxh3 hashing, fastwalk, doublestar, token-estimate fallback, parallel hook composition, parallel agent init, MCP generation-numbered restarts, lazy LSP w/ backoff.
- Dep map: bubbletea→ratatui, mvdan.sh→(none), go-sdk MCP→rmcp, powernap→async-lsp/tower-lsp, goose/sqlc→rusqlite+refinery, cobra→clap, beeep→notify-rust, go-git→git2, chroma→syntect.
