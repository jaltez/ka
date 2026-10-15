# Ka Agent — the product boundary (ratified 2026-10-15)

Ka is the engine. Ka Agent will be a separate product. This doc is the
decision record: what the repo does now, what it must never do, and the
migration path for the one name the two products collide on.

## 1. Two products, one mythology

| | ka (this repo) | Ka Agent (future) |
|---|---|---|
| What | model-agnostic coding agent: turn engine + CLI/TUI, one ≤10 MB musl binary, no daemons | optional persistent companion: long-term memory, messaging channels (Telegram…), web UI, plugins, integrations, automations |
| Process | one process; children only (user shell, stdio MCP, git, debug adapters) | its own process/workspace; drives ka or embeds its libraries |
| Lives | here | not here — never inside the `ka` binary, never in its dependency graph |

Naming stays Egyptian: **ka** is the vital essence — the engine every
surface shares. The product name is **Ka Agent** (two words; never "Ka Ba"
or "Kaba"). **Ba** (the mobile personality: channels, memory, presence) and
**Akh** (the effective spirit born of the ka+Ba union) are reserved as
narrative/layer names inside the future product — not crate names today.

## 2. Hard boundaries (enforced, not aspirational)

1. The engine crates (`ka-protocol`, `ka-dialect`, `ka-strand`,
   `ka-engine`, `ka-sandbox`, `ka-index`) never gain resident-agent
   concepts: no messaging clients (Telegram et al.), no web UI, no
   schedulers/cron, no plugin runtimes, no long-term-memory stores beyond
   the strand. A change adding one of those to these crates is wrong in
   this repo, full stop.
2. Surfaces (`ka-term`, the `ka` binary) may depend on engine crates. The
   reverse direction is already forbidden and stays forbidden.
3. Ka Agent, when it exists, consumes ka through the stable surfaces in §3
   or runs the `ka` binary as a subprocess. No ka crate ever depends on
   Ka Agent, a channel crate, or a UI crate.
4. The `ka` binary's default-feature dependency graph is allow-listed:
   `cargo xtask deps` (run in CI) fails on any crate name not in
   `crates/xtask/deps-allowlist.txt`. Future agent code cannot leak into
   the shipped binary even transitively.
5. `default-members` in the root manifest lists exactly today's crates.
   Root `cargo build` / `cargo test` never drag future agent crates even
   if they later join `members`.

## 3. The stable surfaces Ka Agent builds on (all shipped today)

Full contracts in `sdk.md`; the short list:

- `ka run --print ndjson` — headless one-turn with resume (`--session`),
  exit codes 0/1/2.
- `ka serve` — HTTP/SSE: `POST /sessions` (+ `{"resume": …}`),
  `POST /sessions/{id}/prompt` (the single writer), `GET /sessions[/{id}]`,
  `GET /sessions/{id}/events` (ring-buffered replay + live events,
  `Last-Event-ID` reconnect, presence).
- `ka acp` — Agent Client Protocol over stdio (`session/load` accepts
  strand id prefixes).
- strand files on disk — append-only JSONL; replaying records to the tip
  reproduces engine state. Any external process may read them.
- the crate APIs themselves (`ka_engine::spawn_full`, the `ka-protocol`
  Command/Event enums).

Known gap (do not rely on it yet): `ka serve` / `ka acp` sessions run
`Config::default()` — the config chain and the `[features]` baseline do not
apply there (roadmap Phase 10 note). Ka Agent must drive runtime commands
over the protocol or wait for that gap to close.

## 4. The name collision and its migration

The *binary* crate shipped as `ka-agent` through v0.2.11 (`cargo install
ka-agent`, crates.io). The name is reserved for the future product
instead. Migration:

- **Phase A (shipped with this doc)**: docs + CI enforcement only. The
  crate keeps its name; nothing breaks. This document is the reservation.
- **Phase B (shipped 2026-10-15, repo-local — not yet released)**: the
  package is renamed `ka-agent` → `ka-harness`; `[[bin]] name` stays `ka`.
  Crate dir + manifest, xtask (`install` path, publish `ORDER`, the `-p`
  flags in `link`/`dev`/`size`/`release`/`deps`), CI's three builds,
  README, AGENTS.md, and the design docs all carry the new name. The first
  release under `ka-harness` ships as 0.3.0; until then crates.io keeps
  serving `ka-agent` ≤ 0.2.11 unchanged.
- **Phase C (crates.io transition)**: the old `ka-agent` crate remains at
  its last published version forever; optionally publish one final
  `ka-agent` point release whose description points at `ka-harness`
  (maintainer call at rename time). GitHub release notes carry the
  one-line migration:
  `cargo uninstall ka-agent && cargo install ka-harness`.

Why rename at all: every release published as `ka-agent` deepens the
collision with the product that owns the name. The project is pre-1.0 and
self-declared unstable — the cheapest moment is the next minor.

## 5. Not yet — explicit non-goals in this repo

Until Ka Agent starts as its own workspace/product phase:

- No Telegram, web UI, scheduler, plugin-runtime, or long-term-memory code
  anywhere in this workspace — including behind cargo features.
- No future-product `ka-agent` crate directory, no placeholder crates, no
  empty reserved packages. The name reservation lives in this doc only.
- No public API renames; the engine surface is stable for external
  consumers (`sdk.md`).
- No new dependencies in the binary (the standing footprint contract).
- `research/` survey notes describing daemons/Telegram/plugins in *other*
  agents (aider, goose, cline…) are field notes, not commitments; they
  stay as written.

## 6. Build isolation (commands)

```sh
cargo build -p ka-harness                   # the binary, default features (no index)
cargo build -p ka-harness --features index  # + FTS search
cargo run -p xtask -- deps                 # graph allowlist gate (CI runs it)
cargo run -p xtask -- size                 # ≤10 MB budget
cargo run -p xtask -- bench                # cold-start / RSS baselines
```

Root `cargo build` / `cargo test` build `default-members` — today's crates
only. A future agent crate may join `members` (so `--workspace` and
`xtask publish` can see it) but never `default-members`, and the binary's
`-p` builds never see it at all.
