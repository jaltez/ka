# Daily-driver study — what Phase 8 had to close (2026-09)

**What this is.** The decision record behind Phase 8 of `roadmap.md`: a
fact-check of where the tools in `agents/*.md` actually are on the four axes
ka's daily-driver gap analysis flagged, and the locked decisions that fell out
of it. Per-tool feature detail lives in the `agents/` extractions and
`aggregate/feature-taxonomy.md`; this file only records what changed ka's
build order. "Verified" below means: shipped behavior we exercised or
confirmed against the tool's own docs/changelog in 2026-09 — not vibes from
the original 2025 scan.

## The four axes

### 1. Acting through the language server (LSP write-through)

- **OpenCode** wires LSP diagnostics into its edit feedback loop; it does not
  expose server-mediated rename/code-action as agent tools. **Claude Code**,
  **Codex**, **Gemini CLI**, **Crush**, **goose**: no LSP tier at all — text
  edits + grep remain the only mutation path. **omp**: `xd://lsp` device with
  navigation + rename + code actions, clearance-gated — the existence proof
  that an agent can drive a language server safely.
- **Fact-check conclusion:** agent-driven rename through
  `textDocument/rename` is still rare; every textual multi-file rename we
  observed in rival tools is grep-and-sed with the usual missed-callsite
  failure mode. The differentiator is real, and it is only safe if server
  edits ride the same exact-match/ledger write path as `edit` (opencode
  #9102's diagnostics lesson, extended to writes).

### 2. The probe (DAP)

- **omp** ships a DAP device with ~28 actions, including instruction/data
  breakpoints and memory R/W. **VS Code family** (cline/roo/kilo): none of
  their agents drive debuggers. Nothing else in the field lets an agent
  attach, break, and read state.
- **Fact-check conclusion:** full parity with omp's 28 actions is tail
  weight; the load-bearing 80% is ~14 actions (breakpoints, stepping,
  stack/scopes/variables/evaluate, output). The consolidation cost
  (third Content-Length JSON-RPC client in ka-engine) is low because lsp.rs
  and mcp.rs already proved the pattern twice.

### 3. Delegation: contracts, steering, merge-back

- **Claude Code** has subagents with tool allow-lists; steering a *running*
  subagent and clean merge-back from worktree branches are manual.
  **omp** has the full shape: `tasks[]` fan-out with `outputSchema`-validated
  returns, mid-flight steering (`send`), sibling messaging (`hub`), and
  worktree isolation with `apply`/`merge` controls — verified firsthand; ka's
  author daily-drives it.
- **Fact-check conclusion:** delegation without contracts means the parent
  re-parses prose; without steering, a wrong-path child burns its whole
  budget; without merge-back, isolated work is stranded. ka's existing
  worktree delegates (`ka-<name>-<uuid>` branches) are the missing half of a
  shape the field has otherwise only assembled manually.

### 4. Convenience set

- Session resume (`-c`/`/resume`), skills management, and HTML export are
  table stakes everywhere (**Claude Code**, **Crush**, **goose** all ship
  polished versions). **aider**'s compaction ladder (/drop /clear /undo with
  auto-commit safety) remains the best-in-class context-hygiene reference.
- **Fact-check conclusion:** ka had resume and rewind; the gaps were skills
  lifecycle (trust-gated install, no registry), export, and formalizing the
  prune→shake→digest ladder. All filler — never allowed to block 8.1–8.3.

## Locked decisions (ratified 2026-09-14, `roadmap.md` Phase 8)

1. **DAP client in ka-engine** as the third Content-Length JSON-RPC instance
   (after `lsp.rs`, `mcp.rs`); shared framing codec extracted once, not
   copy-pasted.
2. **Adapter catalog is data** — embedded TOML seed + `[debug.adapters]`
   overlay, mirroring `[lsp.commands]`; strict-TOML all the way.
3. **Runtime gates `[debug] enable` / `[lsp] write_through`, inert in
   `--safe-mode`** — debug adapters join the allowed-children list at the
   same trust class as stdio MCP.
4. **WorkspaceEdits apply only through the existing exact-match/ledger write
   path** — caps, snapshots, protected paths, drift refusal. Server-proposed
   edits are never a second write path.
5. **Every addition re-measured via `xtask size`**; anything >100 KB becomes
   a cargo feature (the `dap` feature exists for exactly this).
6. **Keys-only auth reaffirmed**; `ka-passport` stays future.

## Explicitly declined (with the reason that killed it)

- **CoW isolation backends** (fuse/overlayfs daemons) — violates
  one-process-children-only; git worktrees are the shipped subset.
- **snapcompact** and third-party context shims — ka's ladder is
  deterministic and local.
- **Live collab / share relay, npm tarball installs** — network surface with
  no daily-driver payoff.
- **Subscription OAuth in core** — Copilot device-flow only, if ever.

## What would falsify this

- If textual rename in rival tools gets reliably good (post-edit compile
  loops catching every missed reference), 8.1's differentiation shrinks to
  latency. Watch OpenCode's LSP surface first.
- If DAP-over-MCP stabilizes as a standard, a generic MCP debug bridge could
  replace the native client — the `[debug]` gate and catalog survive either
  way.
