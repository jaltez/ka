# omp (providers/model abstraction) — Feature Extraction

> Source: omp internal docs (providers.md, models.md, adding-a-provider.md, provider-quirks.md, provider-compat-reference.md, provider-endpoint-constraints.md, provider-streaming-internals.md, local-models.md, ai-schema-normalize.md, auth-broker-gateway.md, gemini-manifest-extensions.md, ERRATA-GPT5-HARMONY.md, toolconv/{anthropic,gemini,harmony}.md). Scout: OmpProviders.

## Identity
- omp coding agent (`@oh-my-pi/*` monorepo); this slice covers `packages/ai` (transports, registry, auth) + `packages/catalog` (bundled providers/models, compat detection, identity classification). TypeScript on Bun runtime.
- Architecture: two-half provider model — **catalog half** (`CATALOG_PROVIDERS` table: id, defaultModel, envVars, discovery factory) + **auth half** (declarative `ProviderDefinition` per file, aggregated in registry). Stream dispatch keys on `model.api` (wire protocol), NOT `model.provider` — one transport serves dozens of providers.
- 10 wire APIs (`KnownApi`): `openai-completions`, `openai-responses`, `openai-codex-responses`, `azure-openai-responses`, `anthropic-messages`, `bedrock-converse-stream`, `google-generative-ai`, `google-gemini-cli`, `google-vertex`, plus pseudo-API `openrouter` (dual dispatch) and non-catalog transport `pi-native`. Plus special transports: `cursor-agent` (Connect RPC/protobuf), `devin-agent` (Connect), `gitlab-duo` (delegating proxy), `gitlab-duo-agent` (WebSocket workflow), `ollama-chat` (native NDJSON), `kimi-code`/`synthetic` (OpenAI↔Anthropic dual-surface shim).
- Binaries: `omp` CLI; server surfaces `omp auth-broker serve`, `omp auth-gateway serve`. Credentials in SQLite `~/.omp/agent/agent.db`.

## Core loop & orchestration
- Provider slice feeds `agentLoop` via `streamSimple()` → provider stream fn → unified `AssistantMessageEventStream` → `agentLoop.streamAssistantResponse()` bridges to `AgentEvent` (`message_start/update/end`) → `AgentSession` handles retry/compaction/TTSR/persistence.
- Model registry pipeline (on refresh): bundled catalog → `models.yml` custom → provider overrides (`baseUrl`, `headers`, `disableStrictTools`) → `modelOverrides` → merge custom models → runtime-discovered (Ollama/llama.cpp/LM Studio/LiteLLM/built-in managers) → re-apply overrides.

## Context management (provider-side)
- **Prompt caching per wire**: Anthropic `cache_control: {type:"ephemeral"}` (+`ttl:"1h"`) on rolling 2-message tail window, never on system/tools; Responses `prompt_cache_key` + OpenAI 5.6+ explicit `prompt_cache_breakpoint` annotations; Grok `x-grok-conv-id` header; OpenRouter Anthropic models get anthropic-style cache markers on Chat Completions; Bedrock explicit cache points (min tokens 512–4096, ≤4 checkpoints, 1h/5m retention); DeepSeek hit/miss token accounting (miss = billed input).
- **Stateful Responses chaining**: `previous_response_id` + delta payload on official OpenAI (default ON); stale-ID error → full replay; 3 consecutive stale failures or ZDR error → chaining disabled for session. Codex chains only over WebSocket, never SSE.
- **Context promotion** (overflow recovery): on `context_length_exceeded`, try `model.contextPromotionTarget` and retry BEFORE compaction; chain links codex-spark→gpt-5.5→gpt-5.4.
- **Remote compaction** configurable per provider (`remoteCompaction`: enabled/api/endpoint/model/v2StreamingEnabled).
- **Thinking/reasoning replay**: `replayReasoningContent` (local backends re-emit `<think>` in `reasoning_content` for KV-cache prefix hits), `requiresReasoningContentForToolCalls` (DeepSeek/Kimi/MiMo — exact replay required), `qwenPreserveThinking`.
- Model-cache SQLite (schema v12) with `static_fingerprint` (hash of static catalog slice; fingerprint match returns cached rows verbatim).

## Extensibility
- `models.yml` custom providers (`~/.omp/agent/models.yml|yaml`): provider-level `baseUrl/api/apiKey/headers/authHeader/auth(:apiKey|none|oauth)/disableStrictTools/transport: pi-native/imageInputDecoder/discovery/modelOverrides/models/compat/remoteCompaction`.
- Discovery types: `ollama`, `llama.cpp`, `lm-studio`, `openai-models-list`, `proxy` (anthropic+openai dual-wire auto-detect), `litellm` (rich metadata).
- Extensions can `pi.registerProvider(...)` at runtime: model replace/append, custom stream handlers for new API IDs, custom OAuth providers.
- Adding a provider = 1 catalog entry + 1 registry def file + 1 `ALL` array line for wire-reuse providers.
- `models.yml` `apiKey` value = env-var-name-or-literal; `!<cmd>` prefix runs shell command (10s timeout, cached per process); same for `headers` values.

## Safety & permissions (credential ladder, first-match-wins)
1. runtime `--api-key` (never persisted) → 2. `models.yml` config key → 3. stored OAuth (refreshed; multi-account ranked+rotated; Anthropic/ChatGPT per-org/workspace = separate accounts) → 4. `/login`-saved API key → 5. provider env var (incl. `.env` files) → 6. other stored key (broker-migrated) → 7. models.yml fallback resolver.
- `.env` precedence: process env > `<cwd>/.env` > `~/.omp/agent/.env` > `~/.omp/.env` > `~/.env`; `OMP_` prefix mirrored to `PI_`.
- **Auth broker/gateway** (opt-in): broker holds refresh tokens in SQLite (only writer), serves redacted snapshots, server-side refresh; gateway is a forward proxy exposing `/v1/{chat/completions,messages,responses,pi/stream}` — no raw passthrough, all routes re-enter pi-ai so quirks/refresh stay centralized. SSE snapshot stream + generation-based long-poll; client snapshot cache AES-256-GCM; account pools; usage caching 5min TTL ±25% jitter + 24h last-good.
- Secrets hygiene: token files `0600` in `0700` dirs.

## Model/provider abstraction (the focus)
**Providers** (~75+ ids): core — anthropic, openai, openai-codex, google, google-vertex, google-gemini-cli, google-antigravity, azure, amazon-bedrock, bedrock-mantle, groq, openrouter, mistral, xai, xai-oauth, github-copilot, cursor; hosted long tail (deepseek, fireworks, together, cerebras, groq, novita, nvidia, siliconflow, moonshot/kimi-code, minimax, zai, zhipu-coding-plan, qwen-portal, xiaomi, huggingface, litellm, vercel-ai-gateway, cloudflare-ai-gateway, opencode-zen, kilo, devin, venice, etc.); local — ollama, ollama-cloud, llama.cpp, lm-studio, vllm.

**Selection/roles**: `provider/model-id` selectors, fuzzy/glob, `:thinkingLevel` suffix (`off|minimal|low|medium|high|xhigh|max`); roles `default/smol/slow/vision/plan/designer/commit/tiny/task/advisor` via `modelRoles` (`@smol` etc.); path-scoped `enabledModels`/`disabledProviders`.

**Compat flag system** (normalization core): two-phase — catalog-build-time auto-detection from provider/baseUrl/model-id → request-time `resolveOpenAICompatPolicy` merges per-request options into `OpenAICompatPolicy` (reasoning/tools/messages/stream sub-policies). `whenThinking` = pre-built complete alternate compat object, pointer-swapped when thinking active. ~60 flags: message shaping (`supportsDeveloperRole`, `requiresToolResultName`, `requiresAssistantAfterToolResult`, `requiresThinkingAsText`, `requiresMistralToolIds`...), reasoning wire format (`thinkingFormat`: openai|openrouter|zai|qwen|qwen-chat-template; effort maps/ladders), tool-choice interactions, sampling/tokens (`supportsSamplingParams` off for o1/o3/gpt-5+, `maxTokensField`, `alwaysSendMaxTokens`), gateway routing (`openRouterRouting`, `wireModelIdMode`), stream parsing (`reasoningDeltasMayBeCumulative`, `stripDeepseekSpecialTokens`, `streamMarkupHealingPattern`, `emptyLengthFinishIsContextError`, watchdog floors per host).

**Tool-call format normalization**:
1. Neutral `toolWireSchema(tool)`; per-provider dispatcher normalizes: OpenAI `adaptSchemaForStrict`, Responses `sanitizeSchemaForOpenAIResponses` (oneOf→anyOf, strip regex lookarounds), Moonshot, grammar hosts, Ollama, Google/Vertex/CLI `normalizeSchemaForGoogle`, MCP, Anthropic keyword whitelist. One option-driven walker (`normalizeSchema`): JSON Schema 2020-12 upgrade, deref, snake→camel, spills unsupported keys into `description`, local `$ref` inlining, `allOf` collapse, type-array branch emission, enum-const inference (fail-open → `strict:false`).
2. Streaming tool args: Anthropic `input_json_delta` partial JSON; OpenAI `arguments` fragments (MiniMax streams JSON objects — deep-merged; Mistral array `delta.content` normalized); Responses composite IDs; Google delivers complete args + synthetic delta + synthesized IDs (Vertex strips `id` fields); Bedrock `delta.toolUse.input` fragments grouped into single user `toolResult` array, `NO_TOOLS_SENTINEL`; Mistral IDs forced to 9 alnum chars; deterministic UUIDs for id-mangling providers.
3. **Text-based tool-call dialects** (`src/dialect/`) when native tools unavailable or history re-encoded cross-model: `harmony, gemini, qwen3, deepseek, kimi, glm, gemma, hermes, minimax, xml, anthropic`. `StreamMarkupHealing` reconstructs tool calls/thinking leaked into visible text (Kimi `<|tool_calls_section_begin|>` tags, DeepSeek DSML, generic thinking fences). `wrapLeakedThinkingStream` converts in-band ```thinking/`<think>` fences live. `ToolCallLoopGuard`; `withThinkingLoopGuard` kills runaway reasoning (verbatim tail ≥180 chars, trigram Jaccard ≥0.8, progress-lexicon stall, Gemini header-runaway ≥24).
4. Tool-choice unified type (`auto|none|any|required|{function name}|{computer}`) mapped per wire; downgrades for string-only hosts (llama.cpp/LM Studio/Ollama → pinned tool + `required`); `SoftToolRequirement` (reminder text first, hard pin only on failure — protects prompt cache); compaction runs `toolChoice: "none"`.
5. Harmony GPT-5 leak defense: escape reserved `<|...|>` spellings in untrusted text; response leak detection via co-signals (channel adjacency, glitch tokens, script-mismatch spam, cascade, fake-result framing); discard+retry ≤2 then escalate.

**Streaming internals**: unified `AssistantMessageEvent` contract — `start`, block lifecycle triplets (`text_start/delta/end`, `thinking_start/delta/end`, `toolcall_start/delta/end`), `image_end`, terminal `done(stop|length|toolUse)` / `error(aborted|error)`; immediate in-order push delivery; `parseStreamingJsonThrottled` (re-parse only after ≥256 new bytes); RelaxedJson repairing fallback; idle watchdogs (host-configurable floors 0ms local → 600s GLM); post-finish grace window 2500ms for trailing usage chunks; stop-reason mapping tables per provider; empty-completion retry wrappers; Codex extras: WebSocket transport (reuse ≤30s idle, 10s ping/60s timeout, SSE fallback), 300s watchdogs, ≤5 retries transient, 429 backoff, whitespace tool-arg loop breaker, attestation header, zstd body compression, residency pinning.

**Auth summary**: API-key providers; OAuth with PKCE/device flows — Anthropic (claude.ai, 30-day grant TTL, quota windows, error classes QUOTA_EXHAUSTED/RATE_LIMIT/CONCURRENT/MODEL_CAPACITY with distinct backoffs), ChatGPT/Codex (PKCE port 1455 + device-code flow, account rotation ranking), Google CCA (2 flows), GitHub Copilot (device flow, premium-request multipliers), Kimi (device OAuth + fingerprint), Cursor (PKCE deep-link + poll), xAI SuperGrok (RFC 8628), GitLab, Devin, Kilo/Zai; Azure (deployment maps); Bedrock (SigV4 via WebCrypto — no AWS SDK; 5-tier credential chain env→web identity→SSO/profile→ECS→IMDSv2, cache + single-flight + 401 invalidation); Vertex (ADC ladder, token cache with skew+in-flight dedup); Alibaba coding plan (JSON keys).

## Local models
- Keyless implicit engines: `ollama` (native `/api/tags` + `/api/show` for context_length/capabilities; fallback ctx 128k), `llama.cpp` (port 8080, openai-responses), `lm-studio` (port 1234, `/api/v0/models`, `loaded_context_length` preferred), vLLM (`max_model_len` preserved). `replayReasoningContent` + `qwenPreserveThinking` auto-enabled for KV-cache prefix reuse; watchdog floors: first-event 0 (unbounded prefill), idle 300s.
- **Embedded local tiny models** (transformers.js + onnxruntime-node, CPU, q4 quant, warm loads <3s for 1–1.7B): `providers.tinyModel` (session titles; LFM2-350M), `providers.memoryModel` (memory extraction/consolidation; lfm2-1.2b), `providers.autoThinkingModel`. Worker subprocess inference.

## Surfaces
- `/login`, `/logout`, `/model`, `/models` slash commands; `omp models [find]`, `omp auth-broker|auth-gateway` CLIs; gateway HTTP surface for external clients.

## Config & conventions
- `~/.omp/agent/models.yml` (+`.yaml`), `<project>/.omp/config.yml` vs `~/.omp/agent/config.yml` (arrays replaced not merged), `disabledProviders`/`enabledModels` shared ID namespace gating model providers AND discovery providers.

## Distinctive features
- Compat-flag architecture: per-model wire behavior as data (auto-detected defaults + deep-merged overrides + `whenThinking` pointer-swap), avoiding provider-name branches.
- Auth broker + gateway pair (refresh tokens off laptops; foreign-wire gateway re-enters pi-ai for every request).
- One schema normalizer for ALL providers with strict-mode fail-open.
- In-band tool-call dialect engine with 11 dialects + cross-model history re-encoding + stream markup healing + Harmony control-token leak defense (grounded in 1.05M-call corpus study).
- Codex websocket transport with full resilience stack; Gemini-CLI Cloud Code Assist protocol reimplementation.
- Bedrock without AWS SDK: WebCrypto SigV4 + hand-rolled eventstream binary decoder (CRC32-verified frames).
- Tool-choice soft requirements to preserve prompt-cache prefixes; effort→model-id routing for providers without wire effort fields.

## Canonical workflows
1. Add custom gateway via models.yml (baseUrl, api, apiKey or `!cmd`) → validate → select via `/model`.
2. Login provider via OAuth browser/device flow; multiple accounts ranked/rotated.
3. Local-first: run Ollama → implicit keyless discovery → `/model ollama/<id>`.
4. Headless/remote team: auth-broker serve on shared host; clients via `OMP_AUTH_BROKER_URL/TOKEN`; optional auth-gateway for foreign-wire clients.
5. Route per-project: path-scoped disabledProviders for sensitive directories.
6. Outage/quota: credential rotation; reasoning-effort 400 → transparent fallback; strict-tool 400 → retry non-strict; stale Responses chain → full replay then disable chaining.
7. Model switch mid-session: thinking/tool history re-rendered via dialect engine.
8. Context overflow: promotion to larger-context sibling → retry; else auto-compaction.
9. Dual-surface providers: runtime Anthropic-vs-OpenAI wire selection per model compat.
10. Add new provider to omp itself: catalog entry + registry def + ALL-array line.

## Rust / low-footprint notes (for Ka)
- Registry/compat resolution is pure data flow: build-time detect + request-time policy merge with pre-built `whenThinking` variants and pointer-swap — cheap to port; avoid per-request schema spreading.
- Perf tricks worth stealing: throttled streaming-JSON reparse (≥256-byte gate), fingerprinted model cache, lazy provider module loading, single-flight token/credential resolution with abort-race isolation, per-credential cache with refresh skew.
- Vendored transports instead of SDKs: in-repo SSE client, custom HTTP clients, WebCrypto SigV4 + hand-rolled AWS eventstream decoder — replaceable with reqwest/eventsource + ring; binary framing parsers are small self-contained modules.
- Unified stream event enum maps onto Rust `Stream<Item=AssistantMessageEvent>`; tool-call partial-JSON accumulation with repairing parser per provider block.
- Footprint-relevant state: provider registry is declarative tables; compat flags resolve to plain structs; schema normalizer = one `Schema` type + `NormalizeOptions` enum.
- Local engines need: unbounded first-event timeout, long idle floors, KV-cache-preserving reasoning replay (byte-exact history), capability probing.
- Harmony/dialect defense: escaping reserved token spellings + co-signal-based leak detection is pure string analysis; glitch-token list is a small static set.
