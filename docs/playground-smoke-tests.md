# AI Semantic Cache — Playground Smoke Tests

Curl recipes for exercising any variant of the AI Semantic Cache policy
(Pinecone / Qdrant / Azure AI Search) against a local Omni Gateway Replica. The
behaviour is identical across variants; only the prereqs differ.

## Prereqs

Per-variant setup must be done first — see the variant's `docs/how_to.md`:

- **Pinecone** — [`../ai-semantic-cache-policy-openai-pinecone/docs/how_to.md`](../ai-semantic-cache-policy-openai-pinecone/docs/how_to.md)
- **Qdrant** — [`../ai-semantic-cache-policy-openai-qdrant/docs/how_to.md`](../ai-semantic-cache-policy-openai-qdrant/docs/how_to.md) (run `init-qdrant.sh`)
- **Azure AI Search** — [`../ai-semantic-cache-policy-openai-azure-ai-search/docs/how_to.md`](../ai-semantic-cache-policy-openai-azure-ai-search/docs/how_to.md) (run `init-azure-ai-search.sh`)

Common prereqs:

- `make run` is up in the variant's `implementation/playground/` directory (gateway listening on `:8185`).
- `playground/config/api.yaml` is filled in with a real OpenAI key and the variant-specific vector store credentials.
- `embedder.dimension` matches the vector store's index/collection/field dimension.

Export the OpenAI key so it stays out of shell history:

```bash
export OPENAI_KEY="sk-proj-..."
```

The `Authorization: Bearer $OPENAI_KEY` header is what gets proxied to
`https://api.openai.com/v1/chat/completions`. The policy itself uses the
`embedder.apiKey` from `api.yaml` to call `/v1/embeddings` — those are
independent. You may use the same key for both.

## 1. First call — MISS (populates cache)

```bash
curl -i http://localhost:8185/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role":"user","content":"What is the capital of France?"}]
  }'
```

Expected response headers:
- `X-Cache: MISS`
- `X-Cache-Namespace: local-dev`
- `X-Cache-Key: <sha256>`

## 2. Identical request — EXACT HIT

Same body as #1; served from the platform-shared KV without an embedding call
or a vector search.

```bash
curl -i http://localhost:8185/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role":"user","content":"What is the capital of France?"}]
  }'
```

Expected: `X-Cache: HIT-EXACT`.

## 3. Semantically similar — SEMANTIC HIT

Different wording, same intent. Hits when score ≥ `similarityThreshold`.

> **Threshold note.** Pinecone and Qdrant return raw cosine similarity in
> `[0, 1]` (default `0.92` works well). Azure AI Search returns a reranked
> `@search.score` that is **not** raw cosine — start at `0.5` for Azure
> and tune from observed `X-Cache-Score` values.

```bash
curl -i http://localhost:8185/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role":"user","content":"Which city is the capital of France?"}]
  }'
```

Expected: `X-Cache: HIT-SEMANTIC`.

## 4. Unrelated prompt — MISS

```bash
curl -i http://localhost:8185/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role":"user","content":"Explain the lifecycle of a butterfly."}]
  }'
```

Expected: `X-Cache: MISS`.

## 5. Streaming — BYPASS

`bypassOnStream: true` in `api.yaml` skips cache logic for streaming requests.

```bash
curl -i http://localhost:8185/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_KEY" \
  -H 'Content-Type: application/json' \
  -d '{
    "model": "gpt-4o-mini",
    "messages": [{"role":"user","content":"Tell me a joke."}],
    "stream": true
  }'
```

Expected: `X-Cache: BYPASS-STREAM`.

## Header-only inspection

Append `-D - -o /dev/null -s` to any of the above and grep:

```bash
curl -s -D - -o /dev/null http://localhost:8185/v1/chat/completions \
  -H "Authorization: Bearer $OPENAI_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"What is the capital of France?"}]}' \
  | grep -i '^x-cache'
```

## X-Cache values reference

| Value           | Meaning                                                      |
| --------------- | ------------------------------------------------------------ |
| `MISS`          | No exact or semantic match; upstream called, response stored |
| `HIT-EXACT`     | Identical request body found in KV                           |
| `HIT-SEMANTIC`  | Vector search returned a match ≥ `similarityThreshold` (raw cosine for Pinecone / Qdrant; reranked `@search.score` for Azure AI Search) |
| `BYPASS-STREAM` | `bypassOnStream=true` and request had `stream: true`         |
| `BYPASS`        | `similarityThreshold = 1.01` (semantic disabled)             |
