# Aider — Feature Extraction

> Source: research/repos/aider clone (README, main.py, coders/, repomap.py, models.py, commands.py, repo.py, history.py, watch.py, voice.py, linter.py, llm.py, sendchat.py, help.py, gui.py, copypaste.py, onboarding.py, website docs). Scout: Aider.

## Identity
- Repo: Aider-AI/aider, Python, **Apache-2.0**, `aider-chat` / `aider-install` on PyPI; entry points `aider` CLI, `python -m aider`, `aider --browser` (Streamlit GUI), Python API `Coder.create()` (unsupported/unstable).
- Architecture: **monolithic single-process Python chat REPL**. No tool-calling agent loop — the LLM replies in a structured *edit format* parsed into file edits. `main.py` (~1300 lines argparse wiring), `coders/` (edit-format implementations, each `*_coder.py` + `*_prompts.py` pair), `commands.py`, `models.py`, `repomap.py`, `repo.py` (GitPython wrapper), `io.py` (prompt_toolkit), `watch.py`, `voice.py`, `linter.py`, `history.py`, `llm.py`, `sendchat.py`, `scrape.py`, `gui.py`, `onboarding.py`, `analytics.py`, `copypaste.py`.
- Provider layer entirely **delegated to LiteLLM** (`LazyLiteLLM` proxy because `import litellm` takes ~1.5s). Model metadata from litellm's `model_prices_and_context_window.json` cached 24h.
- Footprint: heavy Python dep tree (litellm, GitPython, prompt_toolkit, rich, tree-sitter via grep-ast, Playwright/browser/sounddevice/help as optional extras). Lazy imports + threaded `load_slow_imports()`.

## Core loop & orchestration
- Chat REPL: user msg → `send()` → LLM streams reply → parse per edit format → apply edits → optional auto-commit → auto-lint → auto-test → **reflection loop** (max 3): errors fed back as new turn so the model fixes itself.
- **Retry ladder**: litellm exceptions classified; retryable retried with exponential backoff (0.125s → 60s cap); context-window-exceeded marked; `FinishReasonLength` → continuation only if model `supports_assistant_prefill`.
- **Modes**: `code` (edit), `ask` (read-only), `architect` (2-model: architect proposes, editor model applies), `help` (RAG over bundled docs), `context` (identify files to edit), plus any raw edit format. Mode switches clone the Coder, re-summarizing history when the edit format changes.
- Interruptible: Ctrl-C keeps partial streamed response in history.
- LLM may **suggest shell commands** (after confirmation); LLM-mentioned files offered for auto-add; URLs auto-detected and offered to scrape.
- One-shot: `--message/-m`, `--commit`, `--lint`, `--test`, `--apply`, `--show-repo-map`, `--show-prompts`.

## Tool catalog
No LLM tool-calling (no MCP, no function tools except experimental `wholefile_func`). User-facing slash commands:
1. `/add` — add files as editable chat context
2. `/read-only` — reference-only files (cached)
3. `/drop`, `/ls`, `/reset` — context management
4. `/architect`, `/ask`, `/code`, `/context`, `/chat-mode`, `/ok` — mode control
5. `/model`, `/models`, `/weak-model`, `/editor-model`, `/editor-edit-format`, `/reasoning-effort`, `/think-tokens` — model control
6. `/diff`, `/commit`, `/undo`, `/git` — git ops (`/undo` = soft `git reset HEAD~1` of an aider commit)
7. `/lint`, `/test`, `/run` (`!`) — shell/lint/test execution
8. `/tokens` — token/cost breakdown
9. `/map`, `/map-refresh` — repo-map display/refresh
10. `/web` — scrape URL → markdown; `/paste`, `/copy`, `/copy-context` — clipboard ops
11. `/voice` — record & transcribe voice input
12. `/save`, `/load` — serialize/replay session file-list setup
13. `/settings`, `/editor`, `/multiline-mode`, `/help`, `/report`, `/exit`

## Context management
- **Repo map**: tree-sitter tags → file-level graph → **PageRank-style ranking** → greedy token-budget packing (`--map-tokens` default 1k; ×2 when no files in chat). SQLite tags cache (`.aider.tags.cache.v4`, mtime/hash invalidation, schema versioning). `--map-refresh auto|always|files|manual`.
- **Chat history summarization**: background thread, weak model; recursive head/tail summarization (depth ≤3) to fit `--max-chat-history-tokens`.
- **Cache-friendly chunk ordering** (`ChatChunks`): system prompt → read-only files → repo map → editable files → done history → current messages → final reminder (as sys or user message per model setting).
- **Prompt caching** (`--cache-prompts`): Anthropic/DeepSeek `cache_control` breakpoints; `--cache-keepalive-pings N` background pings every 5 min.
- Token accounting: per-model tokenizer; **sampled token estimation** for large texts (tokenize every 100th line, scale); image token cost from pixel dimensions; `/tokens` display.
- Repo-map selection biased by mentioned fnames/idents; important-file list prioritizes README/Cargo.toml/package.json.

## Extensibility
- **No MCP/plugin/SDK extension points.** Config-driven: `.aider.model.settings.yml` (per-model settings), `.aider.model.metadata.json`, `--alias ALIAS:MODEL`, `--commit-prompt`, `--lint-cmd`, `--test-cmd`, `--notifications-command`.
- `CONVENTIONS.md` via `--read`/`/read-only`. `/save`+`/load` macro facility.

## Safety & permissions
- Confirmation prompts for: adding files, suggested shell commands, adding /run output, committing dirty files, undo, lint-fix loop. `--yes-always` bypasses all. `--dry-run`.
- Git safety: auto-commit each edit batch (separate commit → easy `/undo`); dirty files committed first so user edits never mix with AI edits; attribution via `(aider)` suffix or `Co-authored-by` trailer; pre-commit hooks skipped by default.
- `.aiderignore` exclusions; `--subtree-only`; `--skip-sanity-check-repo`; >1000-file repo warning.
- Secrets: keys via env/`.env`/`--api-key`; `/settings` output scrubbed; OpenRouter OAuth. Analytics opt-out. No shell sandboxing at all — `/run` executes with user privileges after y/n.

## Model/provider abstraction
- **LiteLLM**: 100+ providers incl. local (Ollama/LM Studio with auto `num_ctx` sizing), OpenRouter, Azure, Bedrock, Vertex, Groq, xAI, any OpenAI-compatible endpoint.
- **Model registry**: `MODEL_ALIASES`; 3128-line `model-settings.yml` with per-model: edit_format, weak_model_name, editor_model_name/editor_edit_format, use_repo_map, prompt tweaks, reminder mode, extra_params, cache_control, reasoning_tag (`<think>` stripping), system_prompt_prefix; user overrides via `.aider.model.settings.yml`; fuzzy model matching.
- **Reasoning controls**: `--reasoning-effort` (mapped per provider), `--thinking-tokens` (Anthropic `thinking.budget_tokens` vs OpenRouter `reasoning.max_tokens`); reasoning_tags strips/summarizes `<think>` from history.
- **Three model roles**: main (edits), weak (commit messages, summarization), editor (architect mode) — independently switchable.
- `ensure_alternating_roles` inserts blank filler for Anthropic alternation; GitHub Copilot token→OpenAI key shim.

## Surfaces
- Terminal chat (prompt_toolkit: emacs/vi, completions, Ctrl-R history, multiline); rich Markdown streaming.
- Browser GUI: `--gui/--browser` → Streamlit app.
- IDE integration without a plugin: `--watch-files`.
- Headless/scripting: `-m` one-shot, Python `Coder` API, `--shell-completions`.
- Copy/paste bridge to web chats: `--copy-paste`, `/copy-context`, `--apply-clipboard-edits`.
- Voice (`/voice`), notifications, web scraping (Playwright optional).

## Session & collaboration
- History files: `.aider.chat.history.md` (markdown transcript), `.aider.input.history`, optional `.aider.llm.history` (raw LLM log); `--restore-chat-history`.
- No fork/branch/share/teams — collaboration model is git itself. Transcripts are plain markdown.

## Config & conventions
- 4 equivalent layers: CLI flags → env (`AIDER_*`) → `.env` → `.aider.conf.yml` (git root, cwd, home).
- File conventions: `.aiderignore`, `.aider.model.settings.yml`, `.aider.model.metadata.json`, `CONVENTIONS.md`. No AGENTS.md-style auto-loaded rules.

## Distinctive features
- **Edit-format system** (core IP): `whole`, `diff` (SEARCH/REPLACE blocks), `diff-fenced` (Gemini), `udiff` (for GPT-4 Turbo laziness), `editor-diff`/`editor-whole`, `patch`, experimental function-calling variants. Fuzzy matching with multiple fence styles and malformed-reply recovery fed back as reflections.
- **Repo map**: ranked, token-budgeted tree-sitter symbol map — the original "map your codebase" feature.
- **Watch mode / IDE-less integration**: `--watch-files` + regex spots one-line `AI`/`ai!`/`AI?` comments in any language; `AI!` triggers code changes, `AI?` a question.
- **Voice-to-code**: sounddevice → whisper-1 → chat message.
- **Copy/paste web-chat bridge**: round-trips code and edits through any web LLM UI — an LLM-free usage mode.
- **Git-native undo/redo**: every edit batch = one commit.
- **Reflection loop**: lint/test/malformed-edit failures auto-fed back (≤3 rounds).
- **Auto lint**: built-in tree-sitter parse-error lint for ~200 languages with no external linter.
- **Help mode RAG**: llama_index + HF embeddings over bundled docs.
- **Onboarding wizard**: auto-picks model from present API keys, OpenRouter OAuth, free-tier detection.
- Leaderboards/benchmark harness (swe-bench) in-repo.

## Canonical workflows
1. Classic pair session: `aider file1.py` → `/add` → describe change → SEARCH/REPLACE → auto-commit → `/undo` if bad.
2. Ask-then-code: `/ask` discuss → `/ok` implement.
3. Architect mode: strong reasoner proposes; cheap editor model applies.
4. Lint/test fix loop: `--auto-lint --test-cmd pytest --auto-test`; failures fed back ≤3 reflections until green.
5. IDE watch flow: `aider --watch-files`; drop `# ... AI!` comments in the editor, save.
6. Batch/scripted refactor: `for f in *.py; do aider -m "add docstrings" --yes "$f"; done`.
7. Web-chat augmentation: `--copy-paste` → `/copy-context` → paste into web LLM → `--apply-clipboard-edits`.
8. Voice coding: `/voice`.
9. Repo onboarding/Q&A: no files → expanded repo map → `/ask "what is this repo?"`.
10. Debug/introspection: `--show-prompts`, `--show-repo-map`, `--apply file` (replay edits without any LLM call).

## Rust / low-footprint notes for Ka
- Provider matrix outsourced to one dependency (litellm) — Ka should mirror with a single thin multi-provider HTTP layer (OpenAI-compat + Anthropic native) instead of N provider crates; the per-model quirks table is pure data (YAML) worth porting.
- Lazy imports dominate aider's startup story; lesson: keep tree-sitter loading, tag DB, help index off the critical path.
- **Repo map maps directly to Rust**: tree-sitter crate → tags → graph (petgraph) → Personalized PageRank → greedy token-budget packing; sqlite cache keyed by file hash+mtime with schema versioning; token estimation by line sampling.
- **Edit formats are plain prompt+parser pairs** — no tool protocol needed; ship `diff` (SEARCH/REPLACE with flexible fences + error reflection) for most of aider's value.
- Process model: single process + daemon threads (summarizer, cache keepalive, file watcher via watchfiles (already Rust), clipboard polling).
- Git via subprocess is fine (`commit --no-verify`, soft `reset HEAD~1` for undo) or git2.
- Cheap-model delegation pattern (weak model for commits/summaries; editor model for mechanical edits).
- History persistence = markdown append (`##### role` headers) — trivially portable, human-readable; restore = parse + re-summarize.
