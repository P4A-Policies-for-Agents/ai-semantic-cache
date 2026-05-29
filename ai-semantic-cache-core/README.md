# `ai-semantic-cache-core`

Shared Rust library (`rlib`) for the **AI Semantic Cache** policy family. Holds the parts of the policy that are independent of any specific embedder or vector-store backend, so per-pair variant binaries (`ai-semantic-cache-policy-openai-pinecone`, future `-openai-qdrant`, `-openai-azure-ai-search`, …) stay small and only ship the glue they actually need.

For the high-level design see [`../docs/architecture.md`](../docs/architecture.md). For competitive context see [`../docs/competitive_analysis.md`](../docs/competitive_analysis.md).

## What lives here

| Module | Responsibility |
|---|---|
| `extract` | `PromptExtractor` enum: OpenAI / Anthropic / Cohere / Mistral / Bedrock / Vertex / DataWeave shapes → `(prompt, model)`. |
| `key` | Canonical prompt normalization + SHA-256-based composite `CacheKey { namespace, model, prompt_sha }`. |
| `embed` | `Embedder` trait + `OpenAICompatibleEmbedderConfig`. Concrete impl lives in each variant binary (the rlib only ships the trait + config struct). |
| `store` | `VectorStore` trait + `CachedPayload` + `Match`. Concrete impls (Pinecone, Qdrant, Azure AI Search) live in each variant binary. |
| `engine` | `Engine` orchestration: extract → embed → query → decide → replay-or-forward → write-back. |
| `replay` | Build the cached HTTP response, inject `X-Cache*` headers. |
| `config` | Strongly-typed shared types (`Threshold`, `Ttl`, `FailureMode`, `DistanceMetric`). |
| `error` | `EmbedError`, `StoreError`, `CacheError`. |

## Adding a new variant

A "variant" is one `(embedder, vector-store)` pair shipped as its own Anypoint Exchange asset. Each variant is a 4-file copy of the MVP:

```
ai-semantic-cache-policy-<embedder>-<store>/
├── definition/
│   ├── gcl.yaml         # variant-specific config schema (only the secret-refs this backend needs)
│   ├── exchange.json    # group_id, assetId, version, name, description
│   └── Makefile         # mirror of the MVP variant's definition Makefile
└── implementation/
    ├── Cargo.toml       # cdylib; deps on ai_semantic_cache_core, pdk, async-trait
    ├── Makefile         # mirror of the MVP variant's implementation Makefile
    └── src/
        ├── lib.rs       # #[entrypoint] + Pdk*Embedder + Pdk*Store impls + filters
        └── generated/config.rs  # `cargo anypoint config-gen` output
```

Steps:

1. **Copy** the MVP variant directory and rename the crate (`ai_semantic_cache_<embedder>_<store>`), `package.metadata.anypoint.definition_asset_id`, `implementation_asset_id`, `definition/exchange.json` `assetId`/`name`, and the policy registration name in `lib.rs` (`POLICY_NAME`).
2. **Edit `definition/gcl.yaml`** to swap the `vectordb` and (if applicable) `embedder` blocks for the new backend's required fields. Keep the public-facing names (`namespace`, `similarityThreshold`, `ttlSeconds`, `exactCaching`, `bypassOnStream`, `failureMode`) — those are family invariants.
3. **Add the variant** to the workspace in [`../Cargo.toml`](../Cargo.toml) `[workspace] members`.
4. **Add the variant** to the family Makefile fan-out (already automatic — `find -path '*/implementation/Makefile'` picks it up).
5. **Implement `Pdk*Store`** in `lib.rs` against `pdk::hl::HttpClient`. See the patterns below.
6. **Run `make build && make test`** at the family root.

The `ai-semantic-cache-policy-openai-pinecone` variant is the canonical reference — copy it.

## Adding a new `Embedder`

Most embedding endpoints today (OpenAI, Azure OpenAI, vLLM, HF TEI, Ollama, LocalAI, Together, Fireworks, Anyscale, DeepInfra, Groq) speak the OpenAI shape, so `OpenAICompatibleEmbedderConfig` covers them via `auth_header` / `auth_scheme` / `base_url` knobs. **A dedicated embedder variant only makes sense if a provider's wire format actually diverges from OpenAI's** (e.g. a HuggingFace TEI-only variant, only if HF TEI ever stops being OpenAI-compatible).

When you do need a new one:

```rust
#[async_trait(?Send)]
pub trait Embedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
    fn dimension(&self) -> usize;
}
```

Constraints:

- **`?Send`** — PDK runs single-threaded inside the wasm sandbox; `HttpClient` is `!Send + !Sync`. Use `#[async_trait(?Send)]` and store the client as `Rc<HttpClient>`, never `Arc`.
- **Map upstream status to `EmbedError`**: `401 → Unauthorized`, `5xx/timeouts → Upstream { status, body }`, JSON parse failure → `Malformed(...)`, vector length ≠ `dimension()` → `DimensionMismatch { expected, got }`.
- **No raw sockets** — only `HttpClient`. PDK does not expose TCP/UDP egress; raw-protocol clients are out.
- **Return `Vec<f32>`** in cosine-friendly orientation (most providers already do; Pinecone defaults to cosine).

Place the impl in the **variant binary**, not in this rlib — the impl needs `pdk::hl::HttpClient` which a generic rlib can't easily depend on.

## Adding a new `VectorStore`

```rust
#[async_trait(?Send)]
pub trait VectorStore {
    async fn upsert(&self, key: &CacheKey, vector: &[f32], payload: &CachedPayload, ttl: Duration) -> Result<(), StoreError>;
    async fn query_topk(&self, namespace: &str, model: &str, vector: &[f32], k: usize) -> Result<Vec<Match>, StoreError>;
    async fn get_exact(&self, key: &CacheKey) -> Result<Option<CachedPayload>, StoreError>;
    async fn delete(&self, key: &CacheKey) -> Result<(), StoreError>;
}
```

Implementer checklist:

- **`upsert`** — store the full `CachedPayload` (status / headers / body / `usage_total_tokens` / `written_at`) somewhere the store can return on `query_topk`. Pinecone uses metadata; Qdrant uses the payload field; Azure AI Search uses an `Edm.String` field.
- **`query_topk`** — must apply a **`(namespace, model)` metadata filter** server-side, not after retrieval. The `model`-in-key invariant is what prevents `gpt-4o` responses bleeding into `gpt-3.5` traffic on a shared route.
- **`get_exact`** — by composite ID (`namespace:model:prompt_sha`). Return `Ok(None)` on 404; only return `Err` on real infra failures.
- **`delete`** — for the v2 invalidation API; safe to no-op for now if your backend lacks it.
- **Errors** — same shape as the embedder: `Upstream { status, body }`, `Malformed(...)`. The engine maps these to `fail_open` vs `fail_closed` based on policy config; do not swallow errors at the impl layer.

The exact-cache fast-path (byte-identical prompt) does **not** go through `VectorStore::get_exact` in the MVP — it goes through PDK's `DataStorageBuilder.remote(...)` so customers don't need to operate any storage at all. `get_exact` is still defined on the trait for the v2 backlog (single-flight, write-through invalidation).

## Adding a new `PromptExtractor`

The `PromptExtractor` enum lives in [`src/extract.rs`](./src/extract.rs). To add a provider variant:

1. Add a variant to the enum (e.g. `Aleph`).
2. Implement extraction in the `extract` method — pull `prompt` and `model` from the provider's request shape, return `ExtractOutcome::Found(Extracted { prompt, model })` or `ExtractOutcome::Skip` if the body is unparseable / streaming-only.
3. Add unit tests under `#[cfg(test)] mod tests` covering: happy path, missing `model`, missing/empty messages, malformed JSON.
4. Wire it through `build_extractor` in **each variant binary** that should accept it (`lib.rs`).

If the request shape is proprietary, prefer the `DataWeave` extractor — operators supply a DataWeave expression at deploy time without a code change.

## Build & test

Always go through `make` at the family root (`policies/llm/ai-semantic-cache/`):

```sh
make check                  # cargo check --workspace
make test                   # core unit tests + per-variant integration tests
make implementations-build  # build all variant wasm cdylibs
make definitions-build      # build all GCL definitions
```

Per CLAUDE.md, never call `cargo` directly — go through `make`. The Makefile fans out to each `definition/Makefile` and `implementation/Makefile`.

## Constraints worth re-stating

- **Wasm = single-threaded.** Async traits must be `#[async_trait(?Send)]`. Trait objects are `Rc<dyn …>`, not `Arc<dyn …>`.
- **HttpClient is request-scoped.** It is supplied positionally to filter closures and is `!Send + !Sync`. Build a fresh `Engine` per request from the cached `Config` plus the closure-supplied `HttpClient` — do not store `HttpClient` long-term.
- **HTTP egress only.** No TCP, no UDP, no raw Redis. Use OpenAI-compatible HTTP for embedders, REST APIs for vector stores, and PDK's `DataStorageBuilder` for any platform-managed shared state.
- **PDK preferred over std/3rd-party.** When a feature exists in PDK (data storage, JWT, JSON validator, contracts), use it instead of pulling in a crate.

## License

Apache 2.0. See [`../LICENSE`](../LICENSE).
