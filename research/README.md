# Ka Research — AI Agent Ecosystem Survey

Goal: extract a complete aggregated feature / characteristic / workflow inventory
from existing AI coding agents, to inform **Ka** — a model-agnostic, very-low-footprint
Rust agent.

Date: 2026-08-23 · Method: shallow repo clones + official docs + omp internal docs (130 files),
scanned per-agent, then cross-aggregated.

## Corpus

| Agent | Source | Kind |
|---|---|---|
| omp (+pi) | `omp://` internal docs, badlogic/pi-mono clone | TS agent harness, native Rust layer |
| opencode | sst/opencode clone | TS/Go |
| Codex CLI | openai/codex clone (`codex-rs`) | Rust |
| Claude Code | anthropics/claude-code clone + code.claude.com docs | docs only |
| Gemini CLI | google-gemini/gemini-cli clone | TS |
| Crush | charmbracelet/crush clone | Go |
| Goose | block/goose clone | Rust |
| Amp | ampcode.com docs | docs only (closed source) |
| Aider | Aider-AI/aider clone | Python |
| Cline / Roo / Kilo | docs sites | VSCode extensions |
| OpenHands | All-Hands-AI/OpenHands clone | Python |
| Plandex | plandex-ai/plandex clone | Go |
| Droid / Zed / Cursor / Copilot / Devin | web docs | brief scan |

## Artifacts

- `agents/*.md` — per-agent feature extraction (fixed 13-section template):
  `pi.md`, `omp-core.md`, `omp-providers.md`, `omp-extensibility.md`, `omp-tools-surfaces.md`,
  `opencode.md`, `codex.md`, `claude-code.md`, `gemini-cli.md`, `crush.md`, `goose.md`,
  `amp.md`, `aider.md`, `vscode-family.md` (cline/roo/kilo), `openhands.md`, `plandex.md`,
  `additional-agents.md` (droid/zed/cursor/copilot/devin)
- `aggregate/feature-taxonomy.md` — **master aggregated feature list** (15 categories)
- `aggregate/comparison-matrix.md` — cross-agent matrix (~45 dimensions × 13 agents) + positioning summary
- `aggregate/workflows.md` — 15 canonical user workflows with exemplars
- `ka/implications.md` — mapping to Ka: workspace blueprint, build order, differentiators, risks
- `ka/roadmap.md` — final phased roadmap (post-grilling decisions baked in)
- `ka/architecture.md` — **original Ka architecture**: vocabulary, crates, protocol, dialects, safety, strand format, inspiration→reinvention map
- `repos/` — shallow clones used for scanning (1.4GB — disposable)
