# `ai-semantic-cache-policy-openai-azure-ai-search`

Azure AI Search variant of the AI Semantic Cache policy family. Caches LLM responses keyed by **semantic similarity of the prompt** at the gateway boundary, using:

- An **OpenAI-compatible embedder** (`POST /v1/embeddings`) — works with OpenAI, Azure OpenAI, vLLM, HF TEI, Ollama, LocalAI, Together, Fireworks, Anyscale, DeepInfra, Groq.
- **Azure AI Search** as the semantic similarity store (REST: `POST /indexes/{name}/docs/index` for upsert/delete, `POST /indexes/{name}/docs/search` for vector search, `GET /indexes/{name}/docs/{id}` for exact lookup).
- **PDK shared KV** (`pdk::data_storage::DataStorageBuilder.remote(...)`) for the byte-identical-prompt **exact-cache fast path** — zero customer-managed infrastructure for the fast path.

## Configuration reference

All fields live under the policy block in your `ApiInstance`. Required fields are marked **R**.

| Field | Type | Default | Notes |
|---|---|---|---|
| `namespace` **R** | string | — | Logical partition. Stored as Azure AI Search field `namespace` (filterable); isolates tenants/routes. Folded into the cache key. |
| `embedder.baseUrl` **R** | string (url) | `https://api.openai.com/v1` | OpenAI-compatible base. Policy appends `/embeddings`. |
| `embedder.model` **R** | string | `text-embedding-3-small` | Sent in the request body. |
| `embedder.apiKey` | string (secret) | — | Optional for unauthenticated self-hosted servers. |
| `embedder.authHeader` | string | `Authorization` | Use `api-key` for Azure OpenAI; `X-API-Key` for some self-hosted. |
| `embedder.authScheme` | string | `Bearer` | Empty string for raw-key endpoints. |
| `embedder.dimension` **R** | integer | — | Vector dimension. Must match the Azure AI Search vector field's `dimensions`. Sent to OpenAI as the `dimensions` request parameter (per-request truncation for `-3-small` / `-3-large`); response is validated to match. |
| `embedder.timeoutMs` | integer | `5000` | |
| `vectordb.endpoint` **R** | string (url) | — | Azure AI Search service endpoint (e.g. `https://<service>.search.windows.net`). |
| `vectordb.indexName` **R** | string | — | Index name. Must already exist with the schema documented below. |
| `vectordb.apiKey` **R** | string (secret) | — | Azure AI Search admin key. Sent as the `api-key` header on every request. |
| `vectordb.vectorField` | string | `embedding` | Name of the `Collection(Edm.Single)` vector field in the index. |
| `vectordb.apiVersion` | string | `2024-07-01` | Azure AI Search REST API version. |
| `vectordb.timeoutMs` | integer | `5000` | |
| `similarityThreshold` | number | `0.5` (suggested) | Minimum reranked `@search.score` for a semantic hit. **Not raw cosine** — tune against your corpus. Set to `1.01` to disable semantic hits (exact-cache only). |
| `ttlSeconds` | integer | `3600` | TTL for **exact-cache** entries in the platform shared KV. Azure AI Search docs are not auto-expired by this policy. |
| `exactCaching` | boolean | `true` | Enables the platform-KV-backed fast path. |
| `bypassOnStream` | boolean | `true` | If `true`, request bodies with `"stream": true` are passed through (no caching). |
| `promptExtractor` | enum | `openai` | One of `openai`, `anthropic`, `cohere`, `mistral`, `bedrock`, `vertex`, `dataweave`. |
| `promptExpression` | string | — | Required when `promptExtractor=dataweave`. DataWeave expression returning the prompt text. |
| `modelExpression` | string | — | Required when `promptExtractor=dataweave`. DataWeave expression returning the model name. |
| `cacheResponseStatusCodes` | array<int> | `[200]` | Only responses with these status codes get cached. |
| `failureMode` | enum | `fail_open` | `fail_open`: cache-infra error falls through to upstream. `fail_closed`: cache-infra error returns `503` to the client. |

## One-time index setup

Azure AI Search has no auto-schema. The index, every field, and the vector profile must be declared up-front. The policy filters by `(namespace, model)` and reads back `cached` + `prompt_sha`, so all of those fields must exist with the right attributes.

The variant playground ships an idempotent script:

```sh
AZURE_SEARCH_ENDPOINT="https://<service>.search.windows.net" \
AZURE_SEARCH_API_KEY="<your-admin-key>" \
INDEX_NAME="ai-semantic-cache" \
DIMENSION=1536 \
  ./implementation/playground/init-azure-ai-search.sh
```

The script declares:

| Field | Type | Attributes | Notes |
|---|---|---|---|
| `id` | `Edm.String` | key, retrievable | Composite cache key (namespace_model_promptSha) |
| `namespace` | `Edm.String` | filterable, retrievable | Filtered on every search |
| `model` | `Edm.String` | filterable, retrievable | Filtered on every search |
| `prompt_sha` | `Edm.String` | retrievable | For log correlation |
| `cached` | `Edm.String` | retrievable | Holds the JSON-encoded `CachedPayload` (body is base64) |
| `embedding` (or `vectorField`) | `Collection(Edm.Single)` | searchable, dimensions=N | HNSW Cosine vector profile |

`PUT /indexes/<name>` is upsert: safe to re-run. Schema-incompatible changes (e.g. dimension change) require deleting the index first.

## Deployment

1. **Publish the definition + implementation** to your Anypoint Exchange:

   ```sh
   cd policies/llm/ai-semantic-cache/ai-semantic-cache-policy-openai-azure-ai-search/definition
   make publish
   cd ../implementation
   make publish
   ```

2. **Attach the policy** in API Manager (or via API Manager's REST API) to the API instance fronting your LLM provider, with at minimum `namespace`, `embedder.dimension`, `vectordb.endpoint`, `vectordb.indexName`, and `vectordb.apiKey` set.

3. **Verify** with a `curl` to your API; the response should carry an `X-Cache` header (see decode table below).

## Response headers

| Header | Always emitted? | Value |
|---|---|---|
| `X-Cache` | yes | One of `miss`, `hit-exact`, `hit-semantic`, `bypass-stream`, `bypass-error`. |
| `X-Cache-Score` | hit-semantic only | Reranked `@search.score` to 4 decimals (e.g. `0.9120`). **Not cosine.** |
| `X-Cache-Namespace` | yes | The configured `namespace`. |
| `X-Cache-Key` | yes | First 12 hex chars of the prompt SHA-256, for log correlation. |

## `X-Cache` decode table

| Value | Meaning | Where it came from |
|---|---|---|
| `miss` | Prompt not in cache. Forwarded upstream and (if 2xx) written back. | Both KV and Azure missed. |
| `hit-exact` | Identical prompt previously cached. Embedder + Azure NOT called. | Platform shared KV. |
| `hit-semantic` | Paraphrased prompt similar to a cached one above `similarityThreshold`. | Azure `/docs/search` top-1. |
| `bypass-stream` | Request had `"stream": true` and `bypassOnStream=true`. | Short-circuit before any cache call. |
| `bypass-error` | Cache-infra error and `failureMode=fail_open`. Forwarded upstream uncached. | Embedder/Azure failure with fallback. |

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `bypass-error: vector query failed: store: upstream 404: ... index 'X' for service 'Y' was not found` | Index doesn't exist | Run `init-azure-ai-search.sh` against the configured `vectordb.endpoint`. |
| `bypass-error: vector query failed: store: upstream 403/401` | Wrong/rotated admin key | Confirm `vectordb.apiKey` matches the current admin key in the Azure portal. |
| Always `miss`, never `hit-semantic` | Threshold tuned for cosine, not Azure's reranked score | Lower `similarityThreshold` substantially (try `0.5` first); inspect `X-Cache-Score` on hits to learn the actual range for your corpus. |
| `503` returned to client | `failureMode=fail_closed` and Azure or embedder unreachable | Check `vectordb.endpoint` reachable from gateway pods; confirm admin key. |
| `embedder.dimension` rejected at startup | Embedder returned a different size than configured | Align `dimension` with both the model AND the index vector field's `dimensions`. |
| Hits leaking across models on the same route | (this should not happen — `model` is auto-folded into the key and filtered) | File a bug; include the request body and `X-Cache-Key`. |
| `bypass-error` after upgrading the policy WASM | Cached entries written before the body-base64 encoding change can't be deserialized | Delete the index and recreate with `init-azure-ai-search.sh`; flush the platform-KV exact-cache namespace or wait for `ttlSeconds`. |

## Local trial

See [`implementation/playground/README.md`](./implementation/playground/README.md) for the playground stack. Unlike Pinecone/Qdrant there is no usable local emulator for Azure AI Search — the playground points at a real Azure AI Search service.

## Compatibility

| | |
|---|---|
| Min Flex Gateway version | `1.11.0` |
| PDK version | `1.8.0` |
| Wasm target | `wasm32-wasip1` |
| Azure AI Search REST API | `2024-07-01` (default; configurable via `vectordb.apiVersion`) |

## License

Apache 2.0. See [`../LICENSE`](../LICENSE).
