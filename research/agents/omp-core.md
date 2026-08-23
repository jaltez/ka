# omp (core/session/context) — Feature Extraction

> Source: omp internal docs (session.md, compaction.md, tree.md, prewalk.md, memory.md, mnemosyne-memory-backend.md, session-tree-plan.md, session-operations-export-share-fork-resume.md, session-switching-and-recent-listing.md, context-files.md, collab.md, secrets.md, non-compaction-retry-policy.md, handoff-generation-pipeline.md, install-id.md). Scout: OmpCore.

## Identity
- **omp / Oh My Pi** — TypeScript monorepo on Bun; packages: `coding-agent`, `agent`, `snapcompact`, `collab-web`, `utils`, `ai`, `@oh-my-pi/pi-mnemopi`.
- Binaries: `omp` (TUI/print/RPC/ACP; flags `--export`, `--fork`, `--resume`, `--continue`, `--prewalk*`, `--no-session`, `--config`, `--prompt-cache-key`, `--provider-session-id`), `omp join "<collab-link>"`.
- Footprint: `~/.omp/agent/sessions/<encoded-cwd>/<ts>_<id>.jsonl` (append-only JSONL), `~/.omp/agent/blobs/<sha256>`, `~/.omp/agent/history.db` (SQLite+FTS5), terminal-sessions breadcrumbs, `~/.omp/install-id` (0600 UUID), `mnemopi/mnemopi.db`.

## Core loop & orchestration
- `agent_end` fan-out: `TurnRecovery.isRetryableError` first (skips compaction that turn) → overflow/incomplete recovery → threshold maintenance; overflow routed to compaction, never generic retry.
- `agent.prompt()` resolves only after `#waitForPostPromptRecovery()` settles retry chains/TTSR/deferred tasks.
- Retry engine: `AIError.classifyMessage`, backoff `min(500ms·2^(n-1), 8s)` × 75–100% jitter, retry-after headers, credential rotation, model fallback chains with fresh budgets + cooldown-expiry revert; replay-safety gate (visible text/tool calls block retry).
- Compaction triggers (6): manual `/compact`, overflow, incomplete-output (`stopReason:"length"`), post-turn threshold, mid-turn threshold, idle. **Context promotion** to larger model tried before compaction.
- `compaction.methodOrder` default `["remote","snapcompact","handoff","shake","soft"]`; failures advance to next method.
- **Speculative async compaction**: starts in pre-threshold band `[t − clamp(t×0.125, 8192, 32000), t)` on a branch snapshot; committed instantly on threshold cross; invalidated by branch-prefix change, unreadable native replay, or growth past `keepRecentTokens`.
- **Prewalk**: one-shot handoff strong→cheap model (`@smol` default); armed via config/flags/`/prewalk`; triggers after successful `todo` call (gate) + first completed `edit`/`write`; disarms after.
- Session switch = guarded transaction: snapshot rollback state → `setSessionFile` → rebuild (model/thinking/tier replay, todos, provider sessions, memory re-key) → rollback on failure.

## Tool catalog (observed in core docs)
1. `read` — files/URLs/internal URIs (`memory://`, `skill://`, `rule://`, `ssh://`).
2. `write` — routes `xd://` device ops; counts for prewalk.
3. `edit` — prewalk trigger.
4. `todo` — phase todos; `view` opens prewalk gate; results serve as restore snapshots.
5. `ask` — interactive questions; `/tree` re-answers as sibling branches.
6. `bash` (`!`) / python (`$`) — shell escapes; host-only in collab.
7. `task` — subagent spawn (`<session>/<AgentId>.jsonl` transcripts).
8. `hub` — agent hub.
9. `learn` — durable lessons (`autolearn.enabled`).
10. `recall` / `retain` / `reflect` / `memory_edit` — memory tools.
11. `select` / `editor` — interactive UI requests (forwarded to collab guests).
12. Extension/skill/custom tools via discovery providers.

## Context management
- **Session = append-only JSONL tree**: `id`/`parentId` per entry + mutable `leafId`; branching moves pointer, never rewrites. Entry types: `message`, `thinking_level_change`, `model_change`, `service_tier_change`, `compaction`, `branch_summary`, `reset_boundary`, `custom` (namespaced `customType`; core reserves `tool_execution_start`, `session_exit`, `user_todo_edit`, `vibe-session-lifecycle`, `autoresearch-control`), `custom_message`, `label`, `title_change`, `ttsr_injection`, `credential_pin`, `session_init`, `mode_change`. v3 format; lazy migrations.
- `buildSessionContext`: parent-walk to root, replay runtime state, emission boundary = latest `reset_boundary` else latest compaction (`firstKeptEntryId` kept window), drop dangling tool calls/unsafe aborted turns; display transcript mode keeps full history with inline compaction dividers.
- **Compaction**: summary + kept window; adaptive `keepRecentTokens`; cut-point rules (never cut at `toolResult`; split-turn double summary). Prompt templates: first/update/turn-prefix/short/handoff.
- **Snapcompact** (no-LLM): history serialized (head+tail truncation, arg caps, dim tool noise) → model-aware PNG frames with per-provider visual-token billing table (Claude 1932/1568px under 4,784-token cap; Gemini fixed 1,120-token budget @2048px; GPT/Codex area-proportional @1568px; Kimi/GLM @1568px); foveated HQ/LQ/HQ middle ≤80 frames; needs vision-capable model.
- **Shake**: local elision of tool results/large blocks to `artifact://` refs.
- **Pruning**: protect newest 40k tokens, ≥20k savings, sub-50-token floor, protect skill/plan reads; `[Output truncated - N tokens]`.
- **Useless-result elision**: `AgentToolResult.useless` → `[Uneventful result elided]` with cache-aware timing.
- **Remote compaction**: custom endpoint; OpenAI-compatible `/chat/completions` (llama.cpp/vLLM); Responses V2 streaming compaction (replacement history, 64k budget); native `/responses/compact`.
- **Budgeting**: reserve floor 16384 / ≥15% window; keepRecentTokens 20000; idle 200k/300s.
- **Prompt caching**: `providerPromptCacheKey` inherited on forks; `credential_pin` pseudonymous account hash re-pins resumed OAuth; handoff rides live cache prefix.
- **Memory backends** (default `off`): `local` (2-phase background pipeline extraction+consolidation → MEMORY.md + memory_summary.md + skills/; lease+heartbeat; secret-redacted; ≤5000-token injection); `mnemopi` (local SQLite; auto-recall, retain every N turns, optional 4-voice polyphonic recall w/ RRF, local or remote embeddings, scoping global/per-project/per-project-tagged); `hindsight` (remote server).
- **Context files**: 8 discovery providers (native .omp, claude, codex, gemini, opencode, github, agents, agents-md) with priority shadowing; one user file + one per directory depth; `@path` imports (5-hop, cycle-skip); `<dir-context>` pointers; sticky `RULES.md` always-apply.

## Extensibility (core-side)
- Compaction/tree/switch hooks (`session_before_compact`, `session.compacting`, `session_compact`, `session_before_tree`, `session_tree`, `session_before_branch/branch`, `session_before_switch/switch`).
- `custom`/`custom_message` session entries (namespaced).
- Custom share handler `~/.omp/agent/share.{ts,js,mjs}`.
- Discovery providers are full config sources (MCP, commands, skills, hooks, tools, prompts, settings).
- `memory://` URL protocol; subagent prewalk controls.

## Safety & permissions
- **Secret obfuscation** (off by default): env vars matching KEY/SECRET/TOKEN/PASSWORD/PASS/AUTH/CREDENTIAL/PRIVATE/OAUTH (≥8 chars), global+project `secrets.yml`, built-in GitHub/GitLab/OpenAI regex. Modes: `obfuscate` (reversible `$$HASH(:hint)$$`, friendly names, case hints) vs `replace` (one-way). HMAC under per-install key. Placeholders restored in tool args before execution; re-obfuscated for provider replay.
- **Share redaction**: `share.redactSecrets` default on.
- **Collab**: AES-256-GCM E2E per payload; relay sees room ids/ciphertext/sizes only; 48-byte full link (32B key + 16B write token) vs 32B view-only; guest command allowlist; mutating ops host-only.
- Persistence: latched errors, fail-closed indeterminate errors, EPERM move-aside fallback, orphaned `.bak` recovery, no fsync (software-crash only).

## Model/provider abstraction (core-side)
- Model roles (default/smol/tiny) via registry; persisted `model_change`, `service_tier_change` per-family map, `thinking_level_change` configured-vs-effective (`auto` survives resume).
- Retry fallback chains, usage-aware fallback preflight, Fireworks Fast-to-base intrinsic fallback.

## Surfaces
- TUI (tree/session pickers, retry loaders, collab host/guest), headless `-p`, ACP, RPC mode, CLI startup flags, browser web client (`collab-web`, WebCrypto, static deployable), share viewer page, HTML export with `<omp-tool-view>` components.

## Session & collaboration
- `/tree` (in-file leaf move; filters, fuzzy AND search, Shift+L labels, ask re-answer, user-message prefill), `/branch` (new session file from selected user message), `/fork` (duplicate + artifacts + cache-key inheritance), `/resume` (Tab folder↔all-projects, prefix matching, `@claude`/`@codex` foreign import), `--continue` (terminal breadcrumb first; 8-level subagent walk-up; cross-cwd re-root).
- Terminal breadcrumbs keyed by TTY path → env ids (ZELLIJ_PANE_ID, TMUX_PANE, CMUX_SURFACE_ID, KITTY_WINDOW_ID, WEZTERM_PANE, TERM_SESSION_ID, WT_SESSION).
- `/collab`: native-TUI replica sharing (frames welcome+snapshot-chunk/entry/event/state/bus/agents/ui-request); host-authoritative; WS relay.
- `/share`: E2E gzip+AES-256-GCM snapshot; blob store or secret gist via `gh`; key in URL fragment only.
- `/new` `/fresh` (rotate provider stream state only) `/clear` (durable reset_boundary) `/drop` `/export` `/dump`; recursive subagent transcript embed in export.
- Interrupted-turn recovery: `session_exit` records + synthetic `stopReason:"aborted"` assistant message on resume.

## Config & conventions
- `~/.omp/agent/config.yml` + `.omp/config.yml` + `--config` overlays; profiles (`--profile` → `~/.omp/profiles/<name>/agent`); `PI_CODING_AGENT_DIR`; arrays replaced not merged.

## Distinctive features
- Snapcompact: compaction via model-billing-aware bitmap PNG frames, zero LLM calls, overflow-safe.
- Speculative async compaction in pre-threshold band with branch-snapshot invalidation.
- Prewalk plan-strong/implement-cheap one-shot handoff.
- Terminal-scoped breadcrumbs for per-pane continue + re-rooting vanished cwds.
- `ask` re-answer sibling branch via `/tree`.
- Collab guests render native replicas (session file + event replay), not terminal mirroring.
- Reversible secret placeholders: HMAC-per-install base, friendly names, case hints.
- 256-byte fixed title slot; 4KiB-prefix/32KiB-tail bounded listing with lifecycle status.
- Cache-aligned handoff side request.
- Useless-result elision with cache-aware timing.

## Canonical workflows
1. Continue: `omp --continue` → breadcrumb/newest → migrate → replay state → synthetic abort for interrupted turn.
2. Explore abandoned approach: `/tree` → select user msg → optional branch summary → prefilled draft → new branch same file.
3. Long session: threshold → armed speculation/promotion → methodOrder walk → CompactionEntry → auto-continue.
4. Overflow recovery: remove failing assistant → promotion → compaction → `agent.continue()`.
5. `/handoff [focus]`: cache-aligned `toolChoice:"none"` side request → commit as compaction entry w/ `<files>` tag.
6. Cheap delegation: `--prewalk-into @smol` → todo gates → first edit flips model.
7. Share: `/share` (redact → seal → blob/gist → fragment-key link) or `/collab` (relay room, QR, guest trust levels).
8. Project memory: `memory.backend: local` → extraction → consolidation → next-session injection.
9. Secrets: enable + `.omp/secrets.yml` → provider text carries placeholders → de-obfuscate at tool boundary.
10. Import foreign: `/resume @claude` → convert → persist as omp session → switch.

## Rust / low-footprint notes
- Append-only JSONL + in-memory index (byId map, children adjacency, labels, leaf) — whole tree derivable from flat log; branching is pointer movement; trivially replicable with Vec+HashMap.
- Bounded I/O: 4KiB prefix (+32KiB tail) listings, stat-keyed caching, bounded parallel workers, streaming loader ≥8MiB.
- Fixed-width 256-byte title slot avoids rewrites.
- Content-addressed sha256 blob store; externalization thresholds (500k strings, 1k base64 images) + load-time rehydration.
- Explicit cheap durability: no fsync; stage+rename atomic w/ EPERM fallback; lazy on-disk gate (first assistant message).
- SQLite+FTS5 with trigger sync + ~100ms batched drain.
- Zero-LLM compaction paths (shake regex elision; snapcompact local PNG rasterization w/ bundled fonts).
- Install UUID via O_EXCL 0600 + EEXIST adoption — lock-free race safety.
- Cache-aware design throughout: elision timing vs prompt-cache lifetime; handoff shares cache prefix.
