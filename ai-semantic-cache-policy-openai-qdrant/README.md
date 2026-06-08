# `ai-semantic-cache-policy-openai-qdrant`

Qdrant variant of the AI Semantic Cache policy family. Caches LLM responses keyed by **semantic similarity of the prompt** at the gateway boundary, using:

- An **OpenAI-compatible embedder** (`POST /v1/embeddings`) — works with OpenAI, Azure OpenAI, vLLM, HF TEI, Ollama, LocalAI, Together, Fireworks, Anyscale, DeepInfra, Groq.
- **Qdrant** (self-hosted Docker / Kubernetes / Qdrant Cloud) as the semantic similarity store (REST: `/points/upsert`, `/points/search`, `/points/<id>`, `/points/delete`).
- **PDK shared KV** (`pdk::data_storage::DataStorageBuilder.remote(...)`) for the byte-identical-prompt **exact-cache fast path** — zero customer-managed infrastructure for the fast path.

## Configuration reference

All fields live under the policy block in your `ApiInstance`. Required fields are marked **R**.

| Field | Type | Default | Notes |
|---|---|---|---|
| `namespace` **R** | string | — | Logical partition. Stored as Qdrant payload field `namespace`; isolates tenants/routes. Folded into the cache key. |
| `embedder.baseUrl` **R** | string (url) | `https://api.openai.com/v1` | OpenAI-compatible base. Policy appends `/embeddings`. |
| `embedder.model` **R** | string | `text-embedding-3-small` | Sent in the request body. |
| `embedder.apiKey` | string (secret) | — | Optional for unauthenticated self-hosted servers. |
| `embedder.authHeader` | string | `Authorization` | Use `api-key` for Azure OpenAI; `X-API-Key` for some self-hosted. |
| `embedder.authScheme` | string | `Bearer` | Empty string for raw-key endpoints (e.g. Azure OpenAI uses raw key in `api-key`). |
| `embedder.dimension` **R** | integer | — | Vector dimension. Must match the Qdrant collection vector size. Sent to OpenAI as the `dimensions` request parameter (per-request truncation for `-3-small` / `-3-large`); response is validated to match. |
| `embedder.timeoutMs` | integer | `5000` | |
| `vectordb.baseUrl` **R** | string (url) | — | Qdrant base URL (e.g. `http://qdrant:6333` or `https://<cluster>.<region>.aws.cloud.qdrant.io:6333`). |
| `vectordb.collection` **R** | string | — | Qdrant collection that stores cached entries. Must already exist with `Cosine` distance and matching vector size. |
| `vectordb.apiKey` | string (secret) | — | API key for Qdrant Cloud (sent as `api-key` header). Omit for unauthenticated self-hosted instances. |
| `vectordb.timeoutMs` | integer | `5000` | |
| `similarityThreshold` | number | `0.92` | Minimum cosine similarity for a semantic hit (0..1). Set to `1.01` to disable semantic hits and run as exact-cache only. |
| `ttlSeconds` | integer | `3600` | TTL for **exact-cache** entries in the platform shared KV. Qdrant points are not auto-expired by this policy. |
| `exactCaching` | boolean | `true` | Enables the platform-KV-backed fast path. |
| `bypassOnStream` | boolean | `true` | If `true`, request bodies with `"stream": true` are passed through (no caching). |
| `promptExtractor` | enum | `openai` | One of `openai`, `anthropic`, `cohere`, `mistral`, `bedrock`, `vertex`, `dataweave`. |
| `promptExpression` | string | — | Required when `promptExtractor=dataweave`. DataWeave expression returning the prompt text. |
| `modelExpression` | string | — | Required when `promptExtractor=dataweave`. DataWeave expression returning the model name. |
| `cacheResponseStatusCodes` | array<int> | `[200]` | Only responses with these status codes get cached. |
| `failureMode` | enum | `fail_open` | `fail_open`: cache-infra error falls through to upstream. `fail_closed`: cache-infra error returns `503` to the client. |

## One-time Qdrant setup

Qdrant requires an **explicit collection** AND **explicit payload indexes** on the fields the policy filters by — unlike Pinecone, it does not auto-index payload. The policy filters every semantic search by `(namespace, model)`, so both fields need `keyword` indexes or Qdrant 400s with `Index required but not found`.

The variant playground ships an idempotent script that handles both:

```sh
# Local Qdrant Docker (no auth)
COLLECTION=ai-semantic-cache DIMENSION=1536 \
  ./implementation/playground/init-qdrant.sh

# Qdrant Cloud
QDRANT_URL=https://<cluster>.<region>.aws.cloud.qdrant.io \
QDRANT_API_KEY=<your-qdrant-cloud-api-key> \
COLLECTION=ai-semantic-cache DIMENSION=1536 \
  ./implementation/playground/init-qdrant.sh
```

The script ensures the collection exists (Cosine distance, your dimension) and the two `keyword` payload indexes (`namespace`, `model`).

## Deployment

1. **Publish the definition + implementation** to your Anypoint Exchange:

   ```sh
   cd policies/llm/ai-semantic-cache/ai-semantic-cache-policy-openai-qdrant/definition
   make publish
   cd ../implementation
   make publish
   ```

2. **Attach the policy** in API Manager (or via API Manager's REST API) to the API instance fronting your LLM provider, with at minimum `namespace`, `embedder.dimension`, `vectordb.baseUrl`, and `vectordb.collection` set (`vectordb.apiKey` for Qdrant Cloud).

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
| `miss` | Prompt not in cache. Forwarded upstream and (if 2xx) written back. | Both KV and Qdrant missed. |
| `hit-exact` | Identical prompt previously cached. Embedder + Qdrant NOT called. | Platform shared KV. |
| `hit-semantic` | Paraphrased prompt similar to a cached one above `similarityThreshold`. | Qdrant `/points/search` top-1. |
| `bypass-stream` | Request had `"stream": true` and `bypassOnStream=true`. | Short-circuit before any cache call. |
| `bypass-error` | Cache-infra error and `failureMode=fail_open`. Forwarded upstream uncached. | Embedder/Qdrant failure with fallback. |

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `bypass-error: vector query failed: store: upstream 400: ... Index required but not found` | Qdrant collection missing `keyword` payload indexes on `namespace` / `model` | Run `init-qdrant.sh` against the cluster. Indexing is retroactive — existing points stay valid. |
| `bypass-error: vector query failed: store: upstream 404` | Collection doesn't exist on the configured `vectordb.baseUrl` | Run `init-qdrant.sh`, or verify `vectordb.collection` matches the actual collection name. |
| `bypass-error: vector query failed: store: upstream 401/403` (Cloud only) | Missing or invalid `vectordb.apiKey` | Confirm Qdrant Cloud key is present and not rotated. |
| Always `miss`, never hits | Threshold too high, or embedder dimension ≠ collection vector size | Lower `similarityThreshold` to ~0.85 to debug; verify `embedder.dimension` matches collection. |
| `503` returned to client | `failureMode=fail_closed` and Qdrant or embedder unreachable | Check `vectordb.baseUrl` reachable from gateway pods. |
| Hits leaking across models on the same route | (this should not happen — `model` is auto-folded into the key and filtered) | File a bug; include the request body and `X-Cache-Key`. |
| `embedder.dimension` rejected at startup | Embedder returned a different size than configured | Update `dimension` to match the model + index (e.g. `text-embedding-3-small` → 1536, `text-embedding-3-large` → 3072). |
| `bypass-error` after upgrading the policy WASM | Cached entries written before the body-base64 encoding change can't be deserialized | Flush the cache: drop the Qdrant collection (or `delete` matching the policy's points) and the platform-KV exact-cache namespace, or wait for `ttlSeconds` to expire. |

## Local trial

See [`implementation/playground/README.md`](./implementation/playground/README.md) for a one-command Docker stack (Flex Replica + MockServer canned to look like OpenAI + a real Qdrant Docker container).

## Compatibility

| | |
|---|---|
| Min Flex Gateway version | `1.11.0` |
| PDK version | `1.8.0` |
| Wasm target | `wasm32-wasip1` |
| Qdrant API surface | v1 REST (`/collections`, `/points`) |

## License

Apache 2.0. See [`../LICENSE`](../LICENSE).
