# How to deploy `ai-semantic-cache-policy-openai-qdrant`

End-to-end operator runbook for the Qdrant variant of the AI Semantic Cache policy. Covers everything from provisioning the Qdrant collection through observing cache hits.

## 1. Prerequisites

| Item | Required | Notes |
|---|---|---|
| **Qdrant cluster** | yes | Qdrant Cloud (`https://<cluster>.<region>.aws.cloud.qdrant.io:6333`) or self-hosted (Docker, Kubernetes, bare metal). |
| **Qdrant API key** | yes for Cloud, optional self-hosted | Cloud always requires it. Self-hosted requires it only if `QDRANT__SERVICE__API_KEY` is set on the server. Sent as the `api-key` HTTP header. |
| **OpenAI (or OpenAI-compatible) embeddings endpoint** | yes | The policy calls `POST /v1/embeddings`. Works with OpenAI, Azure OpenAI, vLLM, HF TEI, etc. |
| **Anypoint Exchange + API Manager** | yes | The policy publishes via `make publish` and is attached to an API instance in API Manager. |
| **`bash` + `curl`** | yes | Used by `init-qdrant.sh`. |
| **Docker Desktop** | optional | Only needed for the local playground. |

## 2. One-time collection setup (REQUIRED)

Qdrant requires both a **collection** AND **explicit payload indexes** on the fields the policy filters by. Unlike Pinecone, Qdrant does not auto-index payload fields. The policy filters every semantic search by `(namespace, model)`, so both fields need `keyword` payload indexes or Qdrant returns HTTP 400 `Index required but not found` and the policy falls open with `bypass-error`.

The variant ships an idempotent script that handles both:

```sh
cd policies/llm/ai-semantic-cache/ai-semantic-cache-policy-openai-qdrant/implementation/playground

# Local Docker (no auth)
COLLECTION=ai-semantic-cache DIMENSION=1536 \
  ./init-qdrant.sh

# Qdrant Cloud
QDRANT_URL=https://<cluster>.<region>.aws.cloud.qdrant.io \
QDRANT_API_KEY=<your-qdrant-cloud-api-key> \
COLLECTION=ai-semantic-cache DIMENSION=1536 \
  ./init-qdrant.sh

# Self-hosted with API key
QDRANT_URL=http://my-qdrant.internal:6333 \
QDRANT_API_KEY=<your-self-hosted-api-key> \
COLLECTION=ai-semantic-cache DIMENSION=1536 \
  ./init-qdrant.sh
```

What the script does:
1. `GET /collections/<name>` — if 200, skip create; otherwise `PUT /collections/<name>` with `{vectors: {size: DIMENSION, distance: Cosine}}`.
2. `PUT /collections/<name>/index` for `namespace` and `model` with `field_schema: keyword`.

PUT is idempotent on both endpoints — safe to re-run. Qdrant indexes existing points retroactively, so adding the indexes after points already exist still works.

Verify:

```sh
curl -sS "$QDRANT_URL/collections/$COLLECTION" \
  -H "api-key: $QDRANT_API_KEY" | jq '.result.payload_schema'
```

You should see `namespace` and `model` both with `data_type: "keyword"`.

## 3. Publish the policy to Anypoint Exchange

```sh
cd policies/llm/ai-semantic-cache/ai-semantic-cache-policy-openai-qdrant/definition
make publish
cd ../implementation
make publish
```

`make publish` requires `anypoint-cli-v4` configured with credentials for the target Anypoint Platform org / business group.

## 4. Attach the policy in API Manager

In API Manager, attach `ai-semantic-cache-policy-openai-qdrant` to the API instance fronting your LLM provider. Minimum required configuration:

```yaml
namespace: "production"
embedder:
  baseUrl: "https://api.openai.com/v1"
  model: "text-embedding-3-small"
  apiKey: "<your-openai-api-key>"
  dimension: 1536              # MUST match step 2 DIMENSION
vectordb:
  baseUrl: "https://<cluster>.<region>.aws.cloud.qdrant.io:6333"
  collection: "ai-semantic-cache"   # MUST match step 2 COLLECTION
  apiKey: "<your-qdrant-cloud-api-key>"   # omit for unauthenticated self-hosted
similarityThreshold: 0.92
```

Keep `dimension` aligned with: (a) the embedder model output, (b) the Qdrant collection vector size, (c) `embedder.dimension`. All three must agree.

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
- `X-Cache: HIT-SEMANTIC` + `X-Cache-Score: 0.…` — third call (only if cosine ≥ `similarityThreshold`).

## 6. Tune `similarityThreshold`

Qdrant returns **raw cosine similarity** in `[0, 1]` — values from Pinecone-tuned configurations transfer directly. Default `0.92` is a good starting point.

Workflow:
1. Send your real-traffic paraphrase patterns (or a synthetic eval set).
2. Read `X-Cache-Score` on hits.
3. Lower the threshold if you're missing legitimate paraphrases; raise it if you're getting false positives.

Set `similarityThreshold: 1.01` to **disable** semantic hits entirely and run as exact-cache only.

## 7. Inspect Qdrant state

```sh
# Collection info + counts
curl -sS "$QDRANT_URL/collections/$COLLECTION" \
  -H "api-key: $QDRANT_API_KEY"

# Run a search directly (cosine + filter)
curl -sS -X POST "$QDRANT_URL/collections/$COLLECTION/points/search" \
  -H "api-key: $QDRANT_API_KEY" -H 'Content-Type: application/json' \
  -d '{
    "vector": [/* 1536 floats */],
    "filter": { "must": [
      {"key":"namespace","match":{"value":"production"}},
      {"key":"model","match":{"value":"gpt-4o-mini"}}
    ]},
    "limit": 1,
    "with_payload": true
  }'

# Delete all points in a namespace (e.g. before A/B tests)
curl -sS -X POST "$QDRANT_URL/collections/$COLLECTION/points/delete" \
  -H "api-key: $QDRANT_API_KEY" -H 'Content-Type: application/json' \
  -d '{
    "filter": { "must": [{"key":"namespace","match":{"value":"production"}}] }
  }'
```

## 8. Troubleshooting

See the [variant README troubleshooting table](../README.md#troubleshooting) for the canonical list. The most common Qdrant-specific failures:

- `Index required but not found` — payload indexes weren't created. Re-run `init-qdrant.sh`.
- `404 collection not found` — collection name typo or wrong cluster. Re-run `init-qdrant.sh` against the correct `vectordb.baseUrl`.
- `dimension mismatch` — collection vector size ≠ embedder dimension. Drop the collection, then re-run `init-qdrant.sh` with the right `DIMENSION`.

## 9. Cache invalidation

The policy does not provide a built-in cache-clear API; operators have three options:

1. **Wait for `ttlSeconds`** — only the platform-KV exact-cache layer auto-expires. Qdrant points persist until deleted.
2. **Filter delete** — `POST /collections/<name>/points/delete` with a payload filter (see step 7).
3. **Drop the collection** — `DELETE /collections/<name>` then re-run `init-qdrant.sh`. Also bounce the platform-KV namespace by changing `namespace` in `api.yaml` (the new value gets a fresh KV partition).

After upgrading the policy WASM across the body-base64 boundary, you must flush — pre-upgrade entries are unreadable and will surface as `bypass-error`.

## 10. Related docs

- Variant configuration reference: [`../README.md`](../README.md)
- Smoke tests / playground curl recipes (shared): [`../../docs/playground-smoke-tests.md`](../../docs/playground-smoke-tests.md)
- Family architecture: [`../../docs/architecture.md`](../../docs/architecture.md)
- Competitive positioning: [`../../docs/competitive_analysis.md`](../../docs/competitive_analysis.md)
- Local playground stack: [`../implementation/playground/README.md`](../implementation/playground/README.md)
