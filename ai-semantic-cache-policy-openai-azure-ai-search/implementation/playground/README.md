# Playground — `ai-semantic-cache-policy-openai-azure-ai-search`

Local Docker stack to exercise the Azure AI Search variant end-to-end:

- **local-flex** — Omni Replica (`mulesoft/flex-gateway:1.11.0`) on `:8185`

Unlike the Pinecone and Qdrant variants there is no usable local
emulator for Azure AI Search — its index schema, vector profile, and
HNSW parameters can only be exercised against a real cloud service.
The playground therefore points at:

- **OpenAI** (real) for embeddings + chat completions, and
- **Azure AI Search** (real, in Azure) for the semantic layer.

## Run

```sh
make run
```

That command builds the wasm, installs the policy into
`./playground/config/custom-policies`, and starts the gateway.

## Configure

`config/api.yaml` is gitignored so it can hold operator-local overrides.
Copy the template and fill in the four placeholders:

```sh
cp config/api.example.yaml config/api.yaml
$EDITOR config/api.yaml
```

You'll need:
- `<your-openai-api-key>` — used for both the embedder and forwarded
  to OpenAI as `Authorization: Bearer …` for chat completions.
- `<your-search-service>` — the Azure AI Search service name (the
  `https://<service>.search.windows.net` host).
- `<your-azure-search-admin-key>` — admin key from the Azure portal,
  sent to Azure as the `api-key` header.
- The `dimension` MUST equal the index vector field's dimensions and
  any per-request truncation supported by the embedder.

## One-time index setup

Azure AI Search has no auto-schema. The index, every field, and the
vector search profile must all be declared up-front. The policy filters
by `(namespace, model)` and reads back `cached` + `prompt_sha`, so all
of those fields must exist with the right attributes.

Use the bundled idempotent script:

```sh
AZURE_SEARCH_ENDPOINT="https://<service>.search.windows.net" \
AZURE_SEARCH_API_KEY="<your-admin-key>" \
INDEX_NAME="ai-semantic-cache" \
DIMENSION=1536 \
  ./init-azure-ai-search.sh
```

Optional env vars:
- `VECTOR_FIELD` (default `embedding`) — must match `vectordb.vectorField` in `api.yaml`.
- `API_VERSION` (default `2024-07-01`) — must match `vectordb.apiVersion`.

The script does an Azure-style `PUT /indexes/<name>` (upsert): safe to
re-run. Schema-incompatible changes (e.g. dimension change) require
deleting the index by hand first.

## Send a request

```sh
curl -sS -X POST http://localhost:8185/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $OPENAI_KEY" \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"What is the capital of France?"}]}' \
  -i
```

Inspect the response headers for the cache decision:
- `X-Cache: MISS` — first call, policy upserts to Azure AI Search.
- `X-Cache: HIT-EXACT` — identical body served from the platform-KV
  layer (no embedder, no Azure AI Search calls).
- `X-Cache: HIT-SEMANTIC` — paraphrased prompt produced a similar
  embedding; a vector search returned a match above
  `similarityThreshold`.

## Inspect Azure AI Search state

```sh
# Count documents
curl -sS "$AZURE_SEARCH_ENDPOINT/indexes/$INDEX_NAME/docs/\$count?api-version=2024-07-01" \
  -H "api-key: $AZURE_SEARCH_API_KEY"

# List a few
curl -sS "$AZURE_SEARCH_ENDPOINT/indexes/$INDEX_NAME/docs?api-version=2024-07-01&\$top=5" \
  -H "api-key: $AZURE_SEARCH_API_KEY"
```

> **Threshold tuning.** Azure's `@search.score` is reranked
> (BM25 + vector + optional semantic reranker), not raw cosine. A
> `similarityThreshold` that works well for Pinecone or Qdrant is
> unlikely to transfer — start at `0.5` and tune against your corpus.
