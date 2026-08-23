# Plandex — Feature Extraction

> Source: research/repos/plandex clone (README, docs/, app/server + app/cli source). Go 1.23, MIT. Scout: Plandex.

## Identity
- 3 Go modules: `plandex-cli`, `plandex-server`, `plandex-shared` (+tygo for TS types).
- **Client–server**: cobra CLI (REPL + one-shot, Bubbletea/go-prompt TUI) over HTTP to a Go server owning all plan state. Metadata in **PostgreSQL**; plan content as **files inside one git repo per plan** — version control/branches/rewind are literal `git init/commit/checkout/reset` via exec. Diffs via `git diff --no-index`.
- Footprint: server needs Postgres + a **LiteLLM sidecar** (localhost:4000) for Anthropic/Google/Vertex/Azure/Bedrock/Ollama; CLI bundles chromedp, bubbletea, glamour, aws-sdk. Cloud wound down 10/2025; self-host (docker-compose) is the path.

## Core loop & orchestration
- REPL defaults to **chat mode**; **tell mode** implements. Two-phase plan pipeline server-side:
  1. **Context phase**: `architect` role makes a plan from the tree-sitter codebase map, emits `### Categories` + `### Files` ending `<PlandexFinish/>`; files auto-loaded (anti-hallucination: only map files; `respond_missing_file` flow).
  2. **Planning/tasks phase**: planner emits `### Tasks` (numbered subtasks w/ `Uses: file1...`), parsed to Subtask rows; subtasks can plan file ops (move/remove/reset).
  3. **Implementation**: per subtask, `coder` writes code; `Uses:` + smart-context filter files per step; `_apply.sh` commands accumulate.
  4. **Build phase** (parallel per file): `builder` converts proposed changes to pending updates with validation; `whole-file-builder` fallback.
  5. **Apply phase** (client-side): write files → run `_apply.sh` (tentative, rollback on failure) → auto-debug loop → git commit (generated message).
- Every assistant reply gets a structured **description** (files touched, tokens); background summarizer keeps convo under `max-convo-tokens`.
- Streaming via custom line protocol: JSON `StreamMessage`s separated by `@@PX@@`.

## Tool catalog
**No function-calling tool loop** — markdown-section protocols parsed server-side:
1. `### Files`/`### Categories` (context selection) 2. `### Tasks` + `Uses:` 3. Code blocks as file updates 4. `_apply.sh` special path 5. `### Move/Remove Files` / `### Reset Changes` 6. `<PlandexFinish/>` terminator.
CLI-side: `plandex browser` (chromedp/CDP; captures console logs, non-zero on JS error), `plandex debug <tries> '<cmd>'` (run→fix→retry loop).

## Context management
- Named context types: **file, url, tree, map (tree-sitter symbol map), note, piped, image**; `plandex load` (globs, `--tree`, `--map`, `-n note`), `@path` REPL shortcut; token counts per item.
- **Project maps**: tree-sitter top-level symbol maps for 30+ languages; batched (500 files/10MB), server-side cache.
- **Smart context**: per-step relevance filtering during implementation. **Auto-update-context**: refreshes stale files/trees/urls before each prompt.
- Summarization: background convo summaries; notes are sticky (never summarized).
- Limits: 25MB/context body, 1000 items, 3000 map paths, 1GB total; ~2M effective window via per-phase selective loading.
- Prompt caching: Anthropic `cache_control: ephemeral` breakpoints when supported; `usage --log` reports cached tokens.

## Extensibility
- **No MCP/plugins/skills/subagents** — an **in-process server hook registry** (`health_check, will_create_plan, will_tell_plan, will_exec_plan, will_send_model_request, call_fast_apply...`) used by the enterprise build.
- Custom models JSON: custom **providers** (any OpenAI-compatible), **models** (limits, `preferredOutputFormat: xml|tool-call-json`), custom **model packs**.

## Safety & permissions
- **Autonomy ladder** (`auto-mode`: none/basic/plus/semi/full + custom) toggling: auto-continue, auto-build, auto-load-context, smart-context, auto-update-context, auto-apply, can-exec, auto-exec, auto-debug (default 5 tries), auto-commit, auto-revert-on-rewind. Semi default; docs carry strong warnings.
- **Cumulative diff sandbox**: changes accumulate as pending updates in the plan's server-side git repo — never touch project files until `apply`. `reject` per-file. `_apply.sh` shown before execution; tentative apply + rollback; conflict detection.
- Secrets: env vars only; Claude Max OAuth on device; `.plandexignore` + `.gitignore`.

## Model/provider abstraction
- Providers: OpenRouter, OpenAI, Anthropic, Claude-Max (subscription OAuth), Google, Vertex, Azure, Bedrock, DeepSeek, Perplexity, Ollama (local), custom OpenAI-compatible. Non-OpenAI/OpenRouter routed through embedded LiteLLM. Provider by env vars; OpenRouter as failover.
- **9 roles**: planner, coder, architect, summarizer, builder, whole-file-builder, names, commit-messages, auto-continue. Per-role: temperature/topP, **largeContextFallback**, **largeOutputFallback**, **errorFallback**, **strongModel** (builder escalates after 2 failed validations).
- 15+ built-in packs (daily-driver, reasoning, strong, cheap, oss, ollama...); per-plan + org defaults; model changes versioned in plan history.

## Surfaces
- REPL (fuzzy autocomplete, `@file`, chat/tell toggle) + plan-stream TUI (vim keys, `s` stop, `b` background) + build TUI + cobra CLI. `diff --ui`: local browser review UI. No IDE extension/SDK; HTTP API is the integration surface.

## Session & collaboration
- Plans: create, auto-naming, list (parent/child dirs), rename, archive, delete. **Version control**: `log`, `rewind` (by count/hash; reverts project files, conflict analysis), **branches** (`checkout`) for comparing prompts/models/approaches. Background tasks: `tell --bg`, `ps`, `connect`, `stop`.
- **Orgs**: multi-user, invites, RBAC; `.plandex` dir committed or gitignored.

## Config & conventions
- Per-plan config (JSONB) + org defaults; CLI flags override. `.plandex/` per project root. **No AGENTS.md-style rules file** — persistent instructions go in context **notes**. `.plandexignore`.

## Distinctive features
- Plan-content-in-git: version control/branch/rewind = real git repo per plan.
- Cumulative diff sandbox + tentative apply + rollback + auto-debug loop; Chrome CDP browser debugging as a first-class step.
- Architect/auto-context two-phase map-driven context selection with anti-hallucination rules; smart per-subtask context windows.
- Role-based multi-model packs with per-role fallback chains and cross-provider mixing incl. Ollama.
- Markdown-protocol "tools" instead of function calling; XML or tool-call-json output per model.
- In-process hook system for enterprise fork.

## Canonical workflows
1. Chat-first → `\tell` to implement.
2. Standard task: semi-auto map+context → planner subtasks → coder → builder diffs → review → apply → `_apply.sh` + debug → auto-commit.
3. Full-auto: `--full` / `tell --apply --commit --auto-exec --debug`.
4. Debug loop: `plandex debug 10 'npm test'`; pipe `npm test | plandex tell`.
5. Iterate via rewind: prompt files → `log`/`rewind` → edit → re-run.
6. Branch comparison: `checkout exp-a`/`exp-b` (different models/prompts) → compare diffs → apply winner.
7. Manual-context precision: `load file.ts`, `load lib -r`, `load . --map`, `load -n 'always add logging'`.
8. Browser app build: `_apply.sh` starts dev server + `plandex browser` → JS errors stream back → auto-debug.
9. Background parallel tasks: `tell --bg` ×N → `ps` → `connect`.
10. Multi-user: commit `.plandex/`, org invites, org default pack.

## Rust / low-footprint notes
- Shell out to system `git` for plan VC instead of libgit2; `git diff --no-index` for diffs. tree-sitter only for maps + structured-edit validation.
- Process model: CLI never calls LLMs — single server owns clients/streams/locks (per-plan advisory locks); goroutine fan-out with panic capture. LiteLLM sidecar = the multi-provider adapter (Rust equivalent: one HTTP client + quirks inline).
- Takeaways: **plan = folder + git repo is a cheap durable design**; sandbox = accumulate updated-file contents and diff/apply client-side; per-role routing + fallback chains is pure config; markdown-section protocols avoid tool-call plumbing; validation cascade (syntax check → targeted fix → strong model → whole-file rewrite) is a robust edit strategy.
