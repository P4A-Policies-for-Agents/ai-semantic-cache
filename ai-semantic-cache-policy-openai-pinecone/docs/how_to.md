# How to deploy `ai-semantic-cache-policy-openai-pinecone`

End-to-end operator runbook for the Pinecone variant of the AI Semantic Cache policy. Covers everything from provisioning the Pinecone index through observing cache hits.

## 1. Prerequisites

| Item | Required | Notes |
|---|---|---|
| **Pinecone serverless index** | yes | Create via Pinecone console or REST. Must be `metric: cosine` — the policy assumes cosine similarity. `dotproduct` and `euclidean` are incompatible with the default thresholds. |
| **Pinecone API key** | yes | Sent to Pinecone as the `Api-Key` header on every request. |
| **Per-index host URL** | yes | Returned by Pinecone when the index is created (e.g. `https://my-index-abc123.svc.aws-us-east-1.pinecone.io`). The policy uses this as `vectordb.indexHost`. |
| **OpenAI (or OpenAI-compatible) embeddings endpoint** | yes | The policy calls `POST /v1/embeddings`. Works with OpenAI, Azure OpenAI, vLLM, HF TEI, etc. |
| **Anypoint Exchange + API Manager** | yes | The policy publishes via `make publish` and is attached to an API instance in API Manager. |
| **`bash` + `curl`** | yes | For index creation. |
| **Docker Desktop** | optional | Only needed for the local playground (uses Pinecone Local emulator). |

## 2. One-time index setup (REQUIRED)

Pinecone auto-indexes vector metadata, so unlike Qdrant or Azure no separate "filter index" step is needed. You only need to create the index itself with the right dimension and metric.

```sh
export PINECONE_KEY="<your-pinecone-api-key>"

curl -sS -X POST https://api.pinecone.io/indexes \
  -H "Api-Key: $PINECONE_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "name": "ai-semantic-cache",
    "dimension": 1536,
    "metric": "cosine",
    "spec": {"serverless": {"cloud": "aws", "region": "us-east-1"}}
  }' | jq .
```

Capture the returned `host` field — that is your `vectordb.indexHost`.

`dimension` MUST equal: (a) the embedder model output, (b) `embedder.dimension` in `api.yaml`. All three must agree. For the OpenAI `text-embedding-3-small` / `text-embedding-3-large` models, the policy sends a per-request `dimensions` field so you can target any value ≤ the model's native size (1536 for `-3-small`, 3072 for `-3-large`).

To delete and recreate (e.g. to change dimension):

```sh
curl -sS -X DELETE https://api.pinecone.io/indexes/ai-semantic-cache \
  -H "Api-Key: $PINECONE_KEY"
```

## 3. Publish the policy to Anypoint Exchange

```sh
cd policies/llm/ai-semantic-cache/ai-semantic-cache-policy-openai-pinecone/definition
make publish
cd ../implementation
make publish
```

`make publish` requires `anypoint-cli-v4` configured with credentials for the target Anypoint Platform org / business group.

## 4. Attach the policy in API Manager

In API Manager, attach `ai-semantic-cache-policy-openai-pinecone` to the API instance fronting your LLM provider. Minimum required configuration:

```yaml
namespace: "production"
embedder:
  baseUrl: "https://api.openai.com/v1"
  model: "text-embedding-3-small"
  apiKey: "<your-openai-api-key>"
  dimension: 1536              # MUST match step 2 dimension
vectordb:
  indexHost: "https://ai-semantic-cache-abc123.svc.aws-us-east-1.pinecone.io"
  apiKey: "<your-pinecone-api-key>"
similarityThreshold: 0.92
```

## 5. Verify with a smoke test

See [`../../docs/playground-smoke-tests.md`](../../docs/playground-smoke-tests.md) for the full curl-by-curl walkthrough (shared across all variants). Quick version:

```sh
export OPENAI_KEY="sk-proj-..."

curl -i https://<your-api-host>/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"What is the capital of France?"}]}'
```

Headers to inspect:
- `X-Cache: MISS` — first call, populates Pinecone + KV.
- `X-Cache: HIT-EXACT` — identical body, served from platform-KV.
- `X-Cache: HIT-SEMANTIC` + `X-Cache-Score: 0.…` — paraphrased prompt, cosine ≥ `similarityThreshold`.

## 6. Tune `similarityThreshold`

Pinecone returns **raw cosine similarity** in `[0, 1]`. Default `0.92` is a good starting point.

Workflow:
1. Send your real-traffic paraphrase patterns (or a synthetic eval set).
2. Read `X-Cache-Score` on hits.
3. Lower the threshold to capture more paraphrases; raise it to reduce false positives.

Set `similarityThreshold: 1.01` to **disable** semantic hits entirely (exact-cache only).

## 7. Inspect Pinecone state

```sh
HOST="https://ai-semantic-cache-abc123.svc.aws-us-east-1.pinecone.io"

# Index stats
curl -sS -X POST "$HOST/describe_index_stats" \
  -H "Api-Key: $PINECONE_KEY" -H 'Content-Type: application/json' -d '{}'

# Run a query directly
curl -sS -X POST "$HOST/query" \
  -H "Api-Key: $PINECONE_KEY" -H 'Content-Type: application/json' \
  -d '{
    "vector": [/* 1536 floats */],
    "topK": 3,
    "includeMetadata": true,
    "filter": {"namespace": {"$eq": "production"}, "model": {"$eq": "gpt-4o-mini"}}
  }'

# Delete by metadata filter (e.g. flush a namespace)
curl -sS -X POST "$HOST/vectors/delete" \
  -H "Api-Key: $PINECONE_KEY" -H 'Content-Type: application/json' \
  -d '{"filter": {"namespace": {"$eq": "production"}}}'
```

## 8. Troubleshooting

See the [variant README troubleshooting table](../README.md#troubleshooting) for the canonical list. The most common Pinecone-specific failures:

- `embedder: dimension mismatch: expected N, got M` — the OpenAI model returned a vector of native size M, but `embedder.dimension` is N. Either change `dimension` to M, or rely on per-request truncation by setting `model: text-embedding-3-small`/`-large` (the policy sends `dimensions: N` so OpenAI truncates).
- `store: upstream 401/403` — wrong/rotated `vectordb.apiKey`.
- `store: upstream 404` — `vectordb.indexHost` doesn't exist or doesn't match the live index. Re-create the index and update the host.
- `store: upstream 400 Vector dimension X does not match dimension Y` — the embedder is returning size X but the index was created at Y. Recreate the index at the right dimension.

## 9. Cache invalidation

The policy does not provide a built-in cache-clear API; operators have three options:

1. **Wait for `ttlSeconds`** — only the platform-KV exact-cache layer auto-expires. Pinecone vectors persist until deleted.
2. **Filter delete** — `POST /vectors/delete` with a metadata filter (see step 7).
3. **Drop the index** — `DELETE /indexes/<name>` then recreate. Also bounce the platform-KV namespace by changing `namespace` in `api.yaml` (the new value gets a fresh KV partition).

After upgrading the policy WASM across the body-base64 boundary, you must flush — pre-upgrade entries are unreadable and will surface as `bypass-error`.

## 10. Related docs

- Variant configuration reference: [`../README.md`](../README.md)
- Smoke tests / playground curl recipes (shared): [`../../docs/playground-smoke-tests.md`](../../docs/playground-smoke-tests.md)
- Family architecture: [`../../docs/architecture.md`](../../docs/architecture.md)
- Competitive positioning: [`../../docs/competitive_analysis.md`](../../docs/competitive_analysis.md)
- Local playground stack: [`../implementation/playground/README.md`](../implementation/playground/README.md)
