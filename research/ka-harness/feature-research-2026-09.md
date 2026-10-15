# Feature research — the Phase 9 delta (2026-09-29)

**What this is.** The decision record behind Phase 9 of `roadmap.md`: a fresh
sweep of ten harnesses for features ka still lacks, followed by a three-round
grill interview that cut the candidate list down to one cycle. Per-tool feature
detail beyond the delta continues to live in `agents/*.md` and
`aggregate/feature-taxonomy.md` (2026-08-23 corpus) and `daily-driver-study.md`
(Phase 8); this file records only the *new* delta and the choices. "Confirmed"
means shipped behavior verified against the tool's own docs/repo on 2026-09-29 —
not remembered from the 2025 scan.

## Method

Three parallel tracks: (1) a full inventory of ka's current surface from the
repo; (2) sweep A — opencode, pi, **omp**, crush, Plandex; (3) sweep B — codex,
Claude Code, gemini-cli, aider, goose. Web-verified only, nothing cloned.

"omp" resolved to **Oh My Pi** (can1357/oh-my-pi, omp.sh) — Can Bölük's
coding-first fork of pi with ~80k lines of Rust for native tooling. pi itself
has moved to earendil-works stewardship (pi.dev); the badlogic/pi-mono docs
remain the reference. Plandex was a bonus skim.

ka's starting position, so the delta reads honestly: ka already ships
delegation with worktree isolation + steering + merge-back, LSP read and
write-through, a DAP client, checkpoints/restore, `serve`/`acp`/`run --schema`,
a sandbox chain (bwrap → firejail → landlock), skills/hooks/commands/agents
with `.claude`/`.agents` compatibility, FTS session search, the
prune → shake → digest ladder, waypoints, and 867-row model catalog. Most
"obvious" features are already covered; what follows is what is genuinely
missing.

## The delta, by theme

### 1. Approval fatigue & safety — ka's weakest lane relative to peers

ka has: `[[rules]]`, clearance tiers, hardstops, protected paths, sandbox
chain, secret redaction, spend/context guards, loop_counts, plan mode.

- **Sandbox-expansion proposal** (gemini-cli, confirmed): when a sandboxed
  command fails on permissions, the tool computes the *exact missing grants*
  and asks once, run-scoped. ka fails closed with a flat denial today.
- **LLM auto-reviewer at the boundary** (codex `approvals_reviewer =
  auto_review`; goose adversary reviewer, confirmed): a cheap model judges
  escalation prompts and auto-allows the obvious ones.
- **Permission-rule import** (Claude Code `/import codex|gemini|cursor`; omp
  auto-imports `.claude`/`.cursor`/`.codex` + five more formats): ka already
  *reads* others' skill/agent/command dirs but cannot import their permission
  rules. Already a "later idea" in `architecture.md`.
- **doom_loop rule domain** (opencode, confirmed): a tool repeating 3× with
  identical input becomes an ask/deny decision. ka has the loop_counts guard
  internally; it is not a configurable rule domain.
- **Prompt-injection detection** (goose, confirmed as a homepage feature).

### 2. TUI personalization & polish

ka has: mouse capture + drag-select + OSC 52, fold chevrons, tool cards,
popups, Alt+E `$EDITOR`, `+text` deferral, Ctrl+R search, toasts, one fixed
palette.

- **Theme files + `/theme` + live editor** (crush ctrl+e; opencode/gemini/claude
  theme pickers): ka ships exactly one palette. Themes-as-data is a perfect
  data-over-code fit (`.ka/themes/*.toml`).
- **Configurable keybinds** (gemini `keybindings.json`; claude `/keybindings`;
  codex `/keymap`): ka's keys are hardcoded.
- **`/btw` side-channel** (claude): quick question answered without entering
  the transcript or burning context.
- **Focus-aware notifications** (crush): notify only when the terminal is
  unfocused (focus reporting).
- **Session diff pane `/diff`** (claude, opencode `session.diff`): every file
  change this session, diffed against checkpoints, reviewable in the TUI.
- **Screen-reader / plain mode** (gemini `--screen-reader`); **prompt stash**
  (claude Ctrl+S); **attention/sound packs** (opencode).

### 3. Model roles & context

ka has: `[roles] default/fast`, `[fallback]` chains, digest ladder with
speculative digest, MEMORY.md tiers + inbox, `.ka/rules/*.md` with `paths:`,
`/context` breakdown, `@`-mentions.

- **Role expansion** (omp: 9 roles — commit, plan, advisor, tiny, vision…;
  aider weak/editor models; codex `review_model`; opencode `small_model`):
  cheap specialized models for titles, commit messages, review. Data-driven,
  near-zero code.
- **Standing advisor** (claude `/advisor`; omp Advisor agent): second-opinion
  model reviewing the primary model on demand.
- **Branch summaries** (pi, confirmed): leaving a forked branch auto-summarizes
  it and pins the summary onto the branch you enter.
- **Cache-hit footer** (pi): ka tracks cache_read/cache_write in Usage but
  never surfaces hit-rate/savings — already flagged as a candidate
  differentiator in `implications.md`.
- **JIT subtree context** (gemini): per-directory context files load when a
  tool touches that subtree; ka resolves root→cwd at session start.
- **`/context` optimization hints** (claude): colored grid + prune advice.
- ka's own open item rides here too: post-digest re-read of ledger-hot files
  (roadmap 8.4, still open pending the record_ids invariant pass).

### 4. Extensibility

ka has: `pre_tool_use`/`post_tool_use` hooks (exit-2 block, stdout mode
steering), skills + custom commands + markdown agents, MCP client (stdio +
streamable-HTTP), `skill install` (git URL/path, trust-gated), LSP/DAP
config-as-data, strict TOML everywhere.

- **Hook lifecycle breadth + power** (claude ~30 events; crush hooks with
  `updated_input` patching + deterministic aggregation + `allow` pre-approve;
  codex config-declared hooks): ka's two events cannot express session
  lifecycle, prompt interception, compaction hooks, argument rewriting, or
  deterministic pre-approval.
- **Packages** (pi `install npm:…/git:…`: versioned bundles of
  skills+agents+commands+themes with per-resource glob filters).
- **MCP OAuth + dynamic client registration** (opencode, RFC 7591) and
  per-server tool enable/disable lists (crush/codex): ka's MCP client is
  keys-only.
- **Command richness** (claude frontmatter `argument-hint`/`allowed-tools`/
  `model`; gemini TOML commands with `!{shell}`/`@{file}` injection).
- **Output styles** (claude): data-defined persona/verbosity.
- **Shell completions** (codex/omp/goose all generate them).
- **crushrc — bash-based config** (crush): considered and rejected by
  implication; strict TOML with line numbers is a ka contract, not a cost.

### 5. Automation & workflows

ka has: `ka run` headless (`--schema`, `--print stream-json`), `ka serve`
(HTTP+SSE with reconnect), `ka acp`, waypoints, background jobs.

- **Recipes** (goose): typed parameters, JSON-schema responses, retry shell
  checks, parallel sub-recipes — workflows as data for humans.
- **Watch mode** (aider `AI!`/`AI?` comment protocol on save).
- **`-i` prompt-then-interactive** (gemini `--prompt-interactive`): run one
  prompt, land in the TUI with its context.
- **Schedules** (goose `schedule --cron`); a daemon-free equivalent for ka
  would emit crontab entries calling `ka run`.
- **Multi-client workspaces** (crush `serve`: two TUIs share session,
  permission queue, LSP/MCP state, with `IsBusy`/`AttachedClients` presence;
  opencode is the server-first extreme). ka serve exists but no TUI ever
  attaches to it.
- **Live session collaboration** (omp `/collab` + `omp join`): previously
  declined ("live collab/share relay"), stays out.

### 6. Git & onboarding odds

ka has: `[git] auto_commit`, checkpoints (`/checkpoint`//`/restore`), `/review
[base]`, `ka init`, `doctor`, signed `update`, `config schema`.

- **Attribution trailers** (`assisted-by`/`co-authored-by`, crush; aider
  `(aider)` suffix + dirty-commit separation) + commit messages via a `commit`
  role.
- **`/pr` / `ka pr <n>`** (claude gh flows; opencode PR checkout).
- **`/init` merging Cursor/Copilot rules** (opencode); omp's config
  auto-import lowers switching cost further.
- **`#:schema` directive** in config files for editor autocomplete (codex) —
  ka already emits the schema, just not the pointer.
- **Session rename/archive/delete** (codex/goose).

## Distinctive ideas worth stealing (unchosen; mined later)

- Checkpoint menu with **"Summarize from here / up to here"** — rewind and
  compaction fused into one interaction (claude).
- **Shadow-git checkpoints** — snapshots to a private repo so rewind never
  touches the user's git (gemini).
- **Repo map**, token-budgeted and ranked (aider) — ka's answer stays LSP
  symbols + ka-index; revisit if small-context local models demand it.
- **Scout subagent** — clones dependency repos into a managed cache for
  upstream research (opencode).
- **Session trees with branch summaries** (pi) — ka has `/tree` + `/fork`;
  the summary pinning is the missing half.
- **Snapshot `/undo` //`/redo`**, repeatable, conversation included
  (opencode).
- **URL schemes** `pr:// issue:// agent:// skill://` (omp).
- **Cumulative diff sandbox until explicit apply** (Plandex).
- **Virtual models + classifier routing** (pi/TypeSafe Jev).
- **Server-first pivot** — TUI as a thin client of an always-on server
  (opencode); ka's serve+attach is the bounded version of this.
- **`codex sandbox` as a general sandboxed runner subcommand.**
- **pi's hardened supply chain** (pinned deps, min-release-age) — context for
  `ka install` trust decisions.
- gemini's **free OAuth tier** and codex's **personality dial** — noted, not
  planned.

## Interview record (2026-09-29, three rounds)

Round 1 — direction. Round 2 — the four flagships in detail. Round 3 —
extras, trailing items, deliverable. Decisions, with the rationale as stated:

1. **Cycle priorities = approval & safety UX + extensibility & automation.**
   TUI personalization and model roles/context were deliberately left out —
   ka's TUI and context story are already strong; prompts are the pain.
2. **ka-passport stays a design stub.** Subscription OAuth is the #1 onboarding
   moat every competitor has, but mid-cycle provider maintenance is weeks of
   scope; land the `TokenSource` trait + data shapes + docs so the crate can
   plug into the auth ladder later.
3. **Generalized `ka install` over full package manifests.** One command for
   skills/agents/commands/rules from git URLs (themes join when a palette loader exists), same trust gate, no
   registry, no manifests — pi-style versioning machinery is not warranted
   yet.
4. **Multi-client reopened — observe-only v1.** The only non-goal reopened.
   `ka attach` proves the wire read-only (live transcript + presence, single
   writer) before any engine-relocation decision; the prior decline covered
   write-sharing.
5. **Flagship = sandbox-expansion modal.** Deterministic, no model in the
   safety path, fits fail-closed; gemini's most-praised UX. The LLM
   auto-reviewer trails instead of leading.
6. **One-shot `ka import claude`** over live-reading `.claude/settings.json`:
   explicit, auditable, keeps strict-TOML purity; printed diff before writing.
7. **All three hook powers** (lifecycle events, `updated_input` rewriting,
   pre-approve). File-watch hooks declined — overlaps the watch-mode non-goal.
8. **Safety trail = both** the opt-in auto-reviewer (fast role, auto-allow
   only, never auto-denies, transcript-logged) and the doom_loop ask-rule.
9. **Extras = command frontmatter, `ka -i`, `ka completions`.** MCP OAuth +
   tool filters was offered and not taken — parked.
10. **This session produces docs only**; implementation starts next session
    in roadmap order.

## Declined / parked (with reasons)

| Item | Reason |
|---|---|
| TUI themes, keybinds, `/btw`, `/diff`, focus-notify | Lane not prioritized this cycle; themes-as-data is the first candidate when it is |
| Model-role expansion, advisor, branch summaries, cache-hit footer, `/context` hints | Same — cheap follow-ups, mostly data |
| MCP OAuth (RFC 7591) + per-server tool filters | Real protocol work, not selected in extras; revisit when an HTTP-MCP-with-auth use case actually bites |
| Watch mode | Would need an fs watcher; non-goal upheld |
| Recipes + schedules | Overlaps agents + commands; daemon-free cron is possible but not wanted now |
| Full shared workspaces (crush parity) | Biggest architectural change in ka's history; observe-only v1 first |
| Package manifests / registries | Install generalization covers the need |
| Live-read `.claude` compat for rules | One-shot import chosen instead |
| Live collab / share relay | Standing decline, reaffirmed |
| Free OAuth / personality / sound packs | Not ka-shaped |

## Sources

- opencode — <https://opencode.ai/docs/>, <https://github.com/sst/opencode>
- pi — <https://github.com/badlogic/pi-mono> (docs in-repo), <https://pi.dev/docs>
- omp — <https://github.com/can1357/oh-my-pi>, <https://omp.sh>
- crush — <https://github.com/charmbracelet/crush>, <https://charm.land/crush>
- Plandex — <https://github.com/plandex-ai/plandex>
- codex — <https://github.com/openai/codex>, <https://learn.chatgpt.com> (config/sandbox/skills pages)
- Claude Code — <https://code.claude.com/docs> (overview, commands, memory, hooks, permissions, checkpointing, subagents)
- gemini-cli — <https://github.com/google-gemini/gemini-cli> (cli-reference, sandbox, custom-commands, checkpointing, gemini-md, keyboard-shortcuts)
- aider — <https://aider.chat/docs/> (usage, commands, modes, git, watch, lint-test)
- goose — <https://github.com/block/goose>, <https://goose-docs.ai> (cli-commands, recipes, extensions)
