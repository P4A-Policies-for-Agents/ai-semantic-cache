# How to deploy `ai-semantic-cache-policy-openai-azure-ai-search`

End-to-end operator runbook for the Azure AI Search variant of the AI Semantic Cache policy. Covers everything from provisioning the Azure resources through observing cache hits.

## 1. Prerequisites

| Item | Required | Notes |
|---|---|---|
| **Azure AI Search service** | yes | Provision via Azure portal, `az search service create`, or Bicep/Terraform. The service name becomes `https://<service>.search.windows.net`. |
| **Admin API key** | yes | Azure portal → Search service → "Settings" → "Keys" → **Primary admin key**. The policy needs admin (write) access. Query keys are not sufficient. |
| **OpenAI (or OpenAI-compatible) embeddings endpoint** | yes | The policy calls `POST /v1/embeddings`. Works with OpenAI, Azure OpenAI, vLLM, HF TEI, etc. |
| **Anypoint Exchange + API Manager** | yes | The policy publishes via `make publish` and is attached to an API instance in API Manager. |
| **`bash` + `curl`** | yes | Used by `init-azure-ai-search.sh`. |
| **Docker Desktop** | optional | Only needed for the local playground. |

## 2. One-time index setup (REQUIRED)

Azure AI Search has **no auto-schema**. The index, every field, and the vector profile must be declared up-front. The policy filters by `(namespace, model)` and reads back `cached` + `prompt_sha`, so all of those fields must exist with the right attributes or the first request 400s with `Index required but not found` or 404s with `index '<name>' was not found`.

The variant ships an idempotent script that handles this:

```sh
cd policies/llm/ai-semantic-cache/ai-semantic-cache-policy-openai-azure-ai-search/implementation/playground

AZURE_SEARCH_ENDPOINT="https://<service>.search.windows.net" \
AZURE_SEARCH_API_KEY="<your-admin-key>" \
INDEX_NAME="ai-semantic-cache" \
DIMENSION=1536 \
  ./init-azure-ai-search.sh
```

Optional env vars:

| Var | Default | Notes |
|---|---|---|
| `VECTOR_FIELD` | `embedding` | Must match `vectordb.vectorField` in `api.yaml`. |
| `API_VERSION` | `2024-07-01` | Must match `vectordb.apiVersion`. |

The script does an Azure-style `PUT /indexes/<name>` (upsert): safe to re-run. **Schema-incompatible** changes (e.g. dimension change) require deleting the index by hand first:

```sh
curl -sS -X DELETE \
  "$AZURE_SEARCH_ENDPOINT/indexes/$INDEX_NAME?api-version=2024-07-01" \
  -H "api-key: $AZURE_SEARCH_API_KEY"
```

Verify the index exists:

```sh
curl -sS \
  "$AZURE_SEARCH_ENDPOINT/indexes/$INDEX_NAME?api-version=2024-07-01" \
  -H "api-key: $AZURE_SEARCH_API_KEY" | jq '.fields | length, .vectorSearch.profiles'
```

You should see 6 fields and a `default-profile` referencing `default-hnsw`.

## 3. Publish the policy to Anypoint Exchange

```sh
cd policies/llm/ai-semantic-cache/ai-semantic-cache-policy-openai-azure-ai-search/definition
make publish
cd ../implementation
make publish
```

`make publish` requires `anypoint-cli-v4` configured with credentials for the target Anypoint Platform org / business group.

## 4. Attach the policy in API Manager

In API Manager, attach `ai-semantic-cache-policy-openai-azure-ai-search` to the API instance fronting your LLM provider. Minimum required configuration:

```yaml
namespace: "production"
embedder:
  baseUrl: "https://api.openai.com/v1"
  model: "text-embedding-3-small"
  apiKey: "<your-openai-api-key>"
  dimension: 1536              # MUST match step 2 DIMENSION
vectordb:
  endpoint: "https://<service>.search.windows.net"
  indexName: "ai-semantic-cache"
  apiKey: "<your-azure-search-admin-key>"
  vectorField: "embedding"     # MUST match step 2 VECTOR_FIELD
  apiVersion: "2024-07-01"     # MUST match step 2 API_VERSION
similarityThreshold: 0.5       # tune; see step 6
```

Keep `dimension`, `vectorField`, and `apiVersion` in sync with what `init-azure-ai-search.sh` was invoked with.

## 5. Verify with a smoke test

See [`../../docs/playground-smoke-tests.md`](../../docs/playground-smoke-tests.md) for the full curl-by-curl walkthrough (shared across variants). Quick version:

```sh
export OPENAI_KEY="sk-proj-..."

# 1) MISS — populates the cache
curl -i https://<your-api-host>/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"What is the capital of France?"}]}'

# 2) HIT-EXACT — same body, served from platform-KV
curl -i https://<your-api-host>/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"What is the capital of France?"}]}'

# 3) HIT-SEMANTIC — paraphrased
curl -i https://<your-api-host>/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"Which city is the capital of France?"}]}'
```

Inspect the response headers:
- `X-Cache: MISS` — first call.
- `X-Cache: HIT-EXACT` — second call.
- `X-Cache: HIT-SEMANTIC` + `X-Cache-Score: 0.…` — third call (only if score ≥ `similarityThreshold`).

## 6. Tune `similarityThreshold`

Azure's `@search.score` is **reranked** (BM25 + vector + optional semantic reranker) — it is **not raw cosine** like Pinecone or Qdrant. A threshold that works well there will not transfer.

Workflow:
1. Start at `similarityThreshold: 0.5`.
2. Send your real-traffic paraphrase patterns (or a synthetic eval set).
3. Read `X-Cache-Score` from headers on the hits and the misses.
4. Tighten until false positives stop without losing legitimate paraphrase hits. Typical Azure hit scores fall in the 0.7–0.95 range.

Set `similarityThreshold: 1.01` to **disable** semantic hits entirely and run as exact-cache only — useful for A/B comparison and for routes where paraphrase substitution would be wrong (compliance-sensitive paths, mutating actions).

## 7. Inspect Azure AI Search state

```sh
# Document count
curl -sS "$AZURE_SEARCH_ENDPOINT/indexes/$INDEX_NAME/docs/\$count?api-version=2024-07-01" \
  -H "api-key: $AZURE_SEARCH_API_KEY"

# List a few
curl -sS "$AZURE_SEARCH_ENDPOINT/indexes/$INDEX_NAME/docs?api-version=2024-07-01&\$top=5&\$select=id,namespace,model,prompt_sha" \
  -H "api-key: $AZURE_SEARCH_API_KEY"

# Delete all docs in a namespace (e.g. before A/B tests)
# Build the doc IDs from the listed `id` values, then POST
# /docs/index with @search.action=delete for each.
```

## 8. Troubleshooting

See the [variant README troubleshooting table](../README.md#troubleshooting) for the canonical list. The most common Azure-specific failure is mismatched `dimension` between (a) the embedder model output, (b) the index vector field's `dimensions`, and (c) `embedder.dimension` in `api.yaml`. All three must agree.

## 9. Cache invalidation

The policy does not provide a built-in cache-clear API; operators have two ways to invalidate:

1. **Wait for `ttlSeconds`** — only the platform-KV exact-cache layer auto-expires. Azure documents persist until deleted.
2. **Manual flush** — delete the Azure index (`curl -X DELETE …`) and recreate via `init-azure-ai-search.sh`, then bounce the platform-KV namespace by changing `namespace` in `api.yaml` (the new value gets a fresh KV partition).

After upgrading the policy WASM across the body-base64 boundary, you must flush — pre-upgrade entries are unreadable and will surface as `bypass-error`.

## 10. Related docs

- Variant configuration reference: [`../README.md`](../README.md)
- Smoke tests / playground curl recipes (shared): [`../../docs/playground-smoke-tests.md`](../../docs/playground-smoke-tests.md)
- Family architecture: [`../../docs/architecture.md`](../../docs/architecture.md)
- Competitive positioning: [`../../docs/competitive_analysis.md`](../../docs/competitive_analysis.md)
- Local playground stack: [`../implementation/playground/README.md`](../implementation/playground/README.md)
