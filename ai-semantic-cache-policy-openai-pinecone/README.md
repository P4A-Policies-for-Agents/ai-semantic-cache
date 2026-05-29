# `ai-semantic-cache-policy-openai-pinecone`

MVP variant of the AI Semantic Cache policy family. Caches LLM responses keyed by **semantic similarity of the prompt** at the gateway boundary, using:

- An **OpenAI-compatible embedder** (`POST /v1/embeddings`) — works with OpenAI, Azure OpenAI, vLLM, HF TEI, Ollama, LocalAI, Together, Fireworks, Anyscale, DeepInfra, Groq.
- **Pinecone serverless** as the semantic similarity store (REST: `/vectors/upsert`, `/query`, `/vectors/fetch`, `/vectors/delete`).
- **PDK shared KV** (`pdk::data_storage::DataStorageBuilder.remote(...)`) for the byte-identical-prompt **exact-cache fast path** — zero customer-managed infrastructure.

On a hit the gateway replays the cached completion without calling the upstream LLM. On a miss it forwards upstream, captures the response, and writes back to both layers best-effort. See [`../docs/architecture.md`](../docs/architecture.md) for the full design and [`../docs/competitive_analysis.md`](../docs/competitive_analysis.md) for positioning vs Kong / Apigee / Azure APIM / Solo.io.

## Configuration reference

All fields live under the policy block in your `ApiInstance`. Required fields are marked **R**.

| Field | Type | Default | Notes |
|---|---|---|---|
| `namespace` **R** | string | — | Logical partition. Stored as Pinecone metadata; isolates tenants/routes. Folded into the cache key. |
| `embedder.baseUrl` **R** | string (url) | `https://api.openai.com/v1` | OpenAI-compatible base. Policy appends `/embeddings`. |
| `embedder.model` **R** | string | `text-embedding-3-small` | Sent in the request body. |
| `embedder.apiKey` | string (secret) | — | Optional for unauthenticated self-hosted servers. |
| `embedder.authHeader` | string | `Authorization` | Use `api-key` for Azure OpenAI; `X-API-Key` for some self-hosted. |
| `embedder.authScheme` | string | `Bearer` | Empty string for raw-key endpoints (e.g. Azure OpenAI uses raw key in `api-key`). |
| `embedder.dimension` **R** | integer | — | Vector dimension. Must match the Pinecone index dimension. Refused at startup if a probe call returns a different size. |
| `embedder.timeoutMs` | integer | `5000` | |
| `vectordb.indexHost` **R** | string (url) | — | Per-index URL from Pinecone console (e.g. `https://my-index-abc.svc.aws-us-east-1.pinecone.io`). |
| `vectordb.apiKey` **R** | string (secret) | — | Sent as `Api-Key` header on every Pinecone request. |
| `vectordb.timeoutMs` | integer | `5000` | |
| `similarityThreshold` | number | `0.92` | Minimum cosine similarity for a semantic hit (0..1). Set to `1.01` to disable semantic hits and run as exact-cache only. |
| `ttlSeconds` | integer | `3600` | TTL for **exact-cache** entries in the platform shared KV. Pinecone vectors are not auto-expired by this policy. |
| `exactCaching` | boolean | `true` | Enables the platform-KV-backed fast path. Disable only if you want every hit to go through the embedder + Pinecone. |
| `bypassOnStream` | boolean | `true` | If `true`, request bodies with `"stream": true` are passed through (no caching). |
| `promptExtractor` | enum | `openai` | One of `openai`, `anthropic`, `cohere`, `mistral`, `bedrock`, `vertex`, `dataweave`. |
| `promptExpression` | string | — | Required when `promptExtractor=dataweave`. DataWeave expression returning the prompt text. |
| `modelExpression` | string | — | Required when `promptExtractor=dataweave`. DataWeave expression returning the model name. |
| `cacheResponseStatusCodes` | array<int> | `[200]` | Only responses with these status codes get cached. |
| `failureMode` | enum | `fail_open` | `fail_open`: cache-infra error falls through to upstream. `fail_closed`: cache-infra error returns `503` to the client. |

## Deployment

1. **Publish the definition + implementation** to your Anypoint Exchange:

   ```sh
   cd policies/llm/ai-semantic-cache/ai-semantic-cache-policy-openai-pinecone/definition
   make publish
   cd ../implementation
   make publish
   ```

2. **Attach the policy** in API Manager (or via API Manager's REST API) to the API instance fronting your LLM provider, with at minimum `namespace`, `embedder.dimension`, `vectordb.indexHost`, and `vectordb.apiKey` set.

3. **Verify** with a `curl` to your API; the response should carry an `X-Cache` header (see decode table below).

## Response headers

| Header | Always emitted? | Value |
|---|---|---|
| `X-Cache` | yes | One of `miss`, `hit-exact`, `hit-semantic`, `bypass-stream`, `bypass-error`. |
| `X-Cache-Score` | hit-semantic only | Cosine similarity to 4 decimals (e.g. `0.9530`). |
| `X-Cache-Namespace` | yes | The configured `namespace`. |
| `X-Cache-Key` | yes | First 12 hex chars of the prompt SHA-256, for log correlation. |

## `X-Cache` decode table

| Value | Meaning | Where it came from |
|---|---|---|
| `miss` | Prompt not in cache. Forwarded upstream and (if 2xx) written back. | Both KV and Pinecone missed. |
| `hit-exact` | Identical prompt previously cached. Embedder + Pinecone NOT called. | Platform shared KV. |
| `hit-semantic` | Paraphrased prompt similar to a cached one above `similarityThreshold`. | Pinecone `/query` top-1. |
| `bypass-stream` | Request had `"stream": true` and `bypassOnStream=true`. | Short-circuit before any cache call. |
| `bypass-error` | Cache-infra error and `failureMode=fail_open`. Forwarded upstream uncached. | Embedder/Pinecone failure with fallback. |

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| Always `miss`, never hits | Threshold too high, or embedder dimension ≠ Pinecone index dimension | Lower `similarityThreshold` to ~0.85 to debug; verify `embedder.dimension` matches the index. |
| `503` returned to client | `failureMode=fail_closed` and Pinecone or embedder unreachable | Check `vectordb.indexHost` reachable from gateway pods; confirm `vectordb.apiKey` not rotated. |
| Hits leaking across models on the same route | (this should not happen — `model` is auto-folded into the key) | File a bug; include the request body and `X-Cache-Key`. |
| `embedder.dimension` rejected at startup | Embedder model returns a different dimension than configured | Update `dimension` to match the model's actual output (e.g. `text-embedding-3-small` → 1536, `text-embedding-3-large` → 3072). |
| First few requests slow then fast | Cold-start of upstream LLM + cache warming | Expected. Subsequent identical prompts hit `hit-exact` (no embedder, no Pinecone). |
| `bypass-error` after upgrading the policy WASM | Cached entries written before the body-base64 encoding change can't be deserialized | Flush the cache: delete the Pinecone index entries (or recreate the index) and the platform-KV exact-cache namespace, or wait for `ttlSeconds` to expire. |

## Local trial

See [`implementation/playground/README.md`](./implementation/playground/README.md) for a one-command Docker stack (Omni Gateway Replica + MockServer canned to look like OpenAI + Pinecone + the upstream LLM).

## Compatibility

| | |
|---|---|
| Min Omni Gateway version | `1.11.0` |
| PDK version | `1.8.0` |
| Wasm target | `wasm32-wasip1` |

## License

Apache 2.0. See [`../LICENSE`](../LICENSE).
