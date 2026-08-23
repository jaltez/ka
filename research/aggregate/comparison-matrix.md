# Cross-Agent Comparison Matrix

Legend: ✅ full · 🔶 partial/experimental · ❌ absent · — n/a. Compiled 2026-08-23.

| Dimension | pi | omp | opencode | codex | claude | gemini | crush | goose | amp | aider | cline/kilo | openhands | plandex |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **Language** | TS/Bun | TS+Bun+Rust NAPI | TS (Effect) | **Rust** | native (closed) | TS | Go | **Rust** | closed (Bun exe) | Python | TS | Python+TS | Go |
| **License** | MIT | — | MIT | Apache-2 | proprietary | Apache-2 | FSL→MIT | Apache-2 | proprietary | Apache-2 | open | MIT | MIT |
| **Footprint** | tiny core | huge | huge | heavy | — | heavy | medium | heavy | — | heavy | medium | very heavy | heavy (server+PG+LiteLLM) |
| Providers | ~40 | ~75 (10 wires) | 75+ (AI SDK) | few + custom | Anthropic-only | Google-only | ~40 (fantasy/catwalk) | 40+ | server-routed | 100+ (LiteLLM) | 24+/gateway | 100+ (LiteLLM) | 11 + LiteLLM |
| Local models | ✅ | ✅ (+ONNX tiny) | 🔶 | ✅ ollama/lmstudio | ❌ | 🔶 Gemma router | ✅ | ✅ (+in-proc llama.cpp) | ❌ | ✅ | ✅ | ✅ | ✅ ollama |
| Tool-call dialects (no-native-tools) | 🔶 | ✅ 11 dialects + healing | 🔶 | 🔶 | — | — | 🔶 toolshim | ✅ toolshim | — | ✅ edit-formats | ❌ | 🔶 | ✅ markdown protocols |
| MCP client | ❌ (ext) | ✅ deep | ✅ | ✅ | ✅ deep | ✅ | ✅ | ✅ (rmcp) | ✅ | ❌ | ✅ | ✅ | ❌ |
| MCP/ACP server mode | 🔶 rpc | 🔶 rpc/ACP | ✅ ACP | ✅ | 🔶 | ✅ ACP | 🔶 server | ✅ ACP+MCP | ❌ | ❌ | ✅ ACP | 🔶 | ❌ |
| Hooks/events | ✅ ~30 | ✅ ~40 | ✅ | ✅ | ✅ 31×5 types | ✅ | 🔶 PreToolUse | ✅ 13 | ✅ plugin events | ❌ | 🔶 | ✅ 6 | ❌ (server hooks) |
| Skills | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ❌ | ✅ | ✅ | ❌ |
| Plugins/marketplace | ✅ packages | ✅ | 🔶 | ✅ | ✅ | ✅ extensions | 🔶 | ✅ Open-Plugins | ✅ hosted repos | ❌ | 🔶 | ✅ | ❌ |
| Custom agents (md) | via ext | ✅ | ✅ | ✅ | ✅ | ✅ | 🔶 config agents | ✅ | ✅ droids | ❌ | ✅ | ✅ | ❌ |
| Subagents | via ext | ✅ + hub/park/revive | ✅ | ✅ v1+v2 | ✅ +teams | ✅ | ✅ | ✅ summon | ✅ threads | ❌ | ✅ | ✅ | ❌ |
| Parallel fan-out | via ext | ✅ batches | 🔶 | ✅ | ✅ workflows | 🔶 | ✅ | ✅ | ✅ | ❌ | ✅ | ✅ | 🔶 bg tasks |
| Sandbox (OS) | via ext | 🔶 CoW isolation | ❌ | ✅ bwrap/landlock/seatbelt | ✅ | ✅ 5 backends | ❌ | 🔶 container | ✅ e2b orbs | ❌ | ❌ | ✅ Docker/Apptainer | 🔶 diff sandbox |
| Permission engine | ❌ (ext) | ✅ 3-tier+patterns | ✅ per-input rules | ✅ profiles+execpolicy | ✅ 6 modes+classifier | ✅ TOML tiers | ✅ | ✅ +adversary | plugins-only | confirm-only | ✅ per-tool | ✅ risk-in-schema | ✅ autonomy ladder |
| Approval-free "yolo" | ✅ | ✅ | ✅ --auto | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ default | ✅ --yes | ✅ | ✅ | ✅ full mode |
| Compaction | ✅ auto | ✅ 5-method ladder + speculation | ✅ | ✅ local+remote | ✅ | ✅ | ✅ | ✅ +tool-pair summarize | ✅ | ✅ recursive | ✅ +25% truncate retry | ✅ condenser-as-event | ✅ convo summaries |
| Zero-LLM compaction | ❌ | ✅ snapcompact/shake | ❌ | ❌ | 🔶 clear outputs first | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Prompt caching | ✅ CH display | ✅ per-wire + keepalive | ✅ | ✅ +WS prewarm | ✅ | 🔶 implicit | ✅ accounting | ✅ | 🔶 usage fields | ✅ keepalive pings | 🔶 | 🔶 | ✅ breakpoints |
| Session store | JSONL tree | JSONL tree + blobs + SQLite | SQLite | JSONL +zstd +SQLite idx | JSONL | JSON per-project | SQLite | SQLite | server | markdown files | VS Code state | event log + state | Postgres + git repo |
| Session tree/fork | ✅ /tree | ✅ +branch summaries | ✅ fork | ✅ +rollback/sections | ✅ branch/fork variants | 🔶 rewind | ❌ | ✅ fork + YAML edit | ❌ (new+mention) | ❌ | 🔶 snapshots+redo | ✅ fork | ✅ git branches |
| Checkpoint/rewind | via ext | ✅ conversation-level | ✅ snapshots | 🔶 | ✅ partial restores | ✅ tri-state + shadow git | ❌ | 🔶 | 🔶 undo_edit | ✅ git commits | ✅ shadow git + redo | 🔶 | ✅ git rewind |
| Plan mode | via ext | ✅ +plan-yolo+prewalk | ✅ plan agent | ✅ | ✅ +approval | ✅ +model routing | ✅ /plan | ✅ /plan planner model | 🔶 Spec Mode | 🔶 architect | ✅ Plan/Act | 🔶 | ✅ (whole product) |
| Todos | via ext | ✅ phases | ✅ | ✅ update_plan | ✅ shared lists | ✅ +DAG tracker | ✅ | ✅ | ✅ | ❌ | ✅ | ✅ | ✅ subtasks |
| LSP integration | ❌ | ✅ 13 actions | ✅ 35+ servers | 🔶 via plugins | 🔶 plugins | ❌ | ✅ 9 tools | ✅ powernap | 🔶 | ❌ | 🔶 problems | ❌ | ❌ |
| Repo map / semantic | ❌ | 🔶 summaries | ❌ | ❌ | ❌ | ❌ | 🔶 sourcegraph | ✅ tree-sitter analyze | ✅ librarian | ✅ PageRank map | 🔶 Qdrant (roo) | ❌ | ✅ tree-sitter maps |
| Web search | ❌ (ext) | ✅ 23 providers | 🔶 Exa | ✅ hosted | ✅ | ✅ grounding | 🔶 DDG+subagent | ✅ skill | ✅ Parallel | ❌ | 🔶 | 🔶 | ❌ |
| Browser automation | ❌ | ✅ 5 backends | ❌ | ❌ | ✅ | ✅ devtools-mcp | ❌ | ❌ | ✅ agent-browser | ✅ playwright | 🔶 | ✅ 14 tools | ✅ chromedp debug |
| Images in/out | ✅ | ✅ gen+inspect | 🔶 | 🔶 view_image | ✅ | ✅ | ✅ view | 🔶 | ✅ painter | 🔶 | 🔶 | ✅ | ✅ context type |
| Voice | ❌ | 🔶 tts | ❌ | ✅ realtime convo | ✅ dictation | 🔶 push-to-talk | ❌ | ✅ whisper dictation | ✅ realtime | ✅ whisper | 🔶 TTS (roo) | ❌ | ❌ |
| Memory (persistent) | ❌ | ✅ 4 backends | ❌ | ✅ 2-phase +citations | ✅ MEMORY.md | 🔶 Auto Memory inbox | 🔶 chatrecall | ✅ extension | 🔶 threads-as-KB | ❌ | 🔶 Memory Bank | ✅ 2-tier | 🔶 notes |
| TUI | ✅ | ✅ rich | ✅ | ✅ ratatui | ✅ | ✅ Ink | ✅ bubbletea | ✅ | ✅ | ✅ REPL | — | — | ✅ REPL+stream |
| Headless/JSON | ✅ | ✅ json/rpc | ✅ serve | ✅ exec --json | ✅ -p stream-json | ✅ stream-json | ✅ run | ✅ run | ✅ -x stream-json | ✅ -m | ✅ NDJSON | ✅ +OpenAI gateway | ✅ CLI |
| IDE integration | ❌ | ACP | ✅ VS Code ext | ✅ VS Code | ✅ VS Code/JB/desktop | ✅ ACP+companion | ❌ | ✅ ACP (Zed/JB) | ✅ VS Code/JB/Zed/nvim | 🔶 watch-files | ✅ core feature | 🔶 ACP host | ❌ |
| Web/cloud surface | 🔶 share | ✅ collab-web | ✅ web app | ✅ cloud | ✅ web+mobile+Slack | 🔶 | ❌ | ✅ desktop+Telegram | ✅ web+mobile+orbs | ❌ | ✅ kilo mobile/Slack | ✅ Canvas | 🔶 (dead cloud) |
| Scheduling/cron | ❌ | 🔶 | ❌ | ✅ clock+CronCreate | ✅ crons/Routines | 🔶 GH Action | ❌ | ✅ scheduler | ✅ self-waking | ❌ | ✅ cline cron | ✅ automation server | ❌ |
| Worktree isolation | ❌ | ✅ CoW clones | ✅ | 🔶 | ✅ 4-layer enforcement | 🔶 | ❌ | 🔶 | ✅ orbs/runners | ❌ | ✅ per-agent | ✅ containers | ❌ |
| Structured output | ✅ | ✅ schema modes | ✅ +retries | ✅ --output-schema | ✅ --json-schema | 🔶 | ❌ | ✅ recipes | ❌ | ❌ | ❌ | ✅ response_schema | ✅ |
| AGENTS.md | ✅ | ✅ 8 providers | ✅ | ✅ | 🔶 via import | ✅ GEMINI.md | ✅ | ✅ .goosehints | ✅ subtree | ❌ | ✅ kilo per-dir | ✅ | ❌ (notes) |
| Enterprise tier | ❌ | 🔶 | ✅ MDM+remote | ✅ requirements.toml | ✅ deep | ✅ admin | ❌ | 🔶 | ✅ SSO/SCIM/MDR | ❌ | ✅ cline | 🔶 | ✅ orgs |
| Self-host server | 🔶 CBOR exp | ✅ broker/gateway | ✅ serve | ✅ app/exec-server | ✅ gateway/runner | ❌ | ✅ server | ✅ serve | ❌ | ❌ | 🔶 cline daemon | ✅ full stack | ✅ docker-compose |

## Positioning summary
- **Leanest core**: pi (7 tools, zero mandated subsystems; everything is an extension).
- **Deepest harness**: omp (compaction ladder, provider normalization, hashline edits, hub, TTSR, collab) — feature superset of pi.
- **Best Rust blueprints**: codex (crate unbundling, sandbox matrix, rollouts, Op/EventMsg queues) and goose (platform extensions behind McpClientTrait, state-machine ops, ACP-first UIs).
- **Best server/protocol design**: opencode (OpenAPI+SSE, everything-is-a-client) and codex app-server (generated schemas).
- **Most complete safety stack**: claude (classifier auto-mode + sandbox + protected paths) and codex (sandbox matrix + execpolicy + Guardian); droid's unbypassable blocklist + wrapper-proof resolution is the sharpest single pattern.
- **Workflow packaging**: goose recipes; claude workflows; plandex plan-as-product.
- **Remote execution**: amp orbs (portals, OIDC, webhooks) > codex exec-server > cursor cloud agents.
- **Context frugality**: omp (snapcompact/shake/speculation/elision) and claude (ToolSearch, budgets, output caps).
