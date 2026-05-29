# Playground — `ai-semantic-cache-policy-openai-pinecone`

Local Docker stack to exercise the policy end-to-end:

- **local-flex** — Omni Replica (`mulesoft/flex-gateway:1.11.0`) on `:8185`
- **mock-service** — MockServer canned to look like an OpenAI-compatible
  embeddings endpoint and an upstream LLM, on `:8080`
- **pinecone-local** — real
  [Pinecone Local](https://docs.pinecone.io/guides/operations/local-development)
  emulator (`ghcr.io/pinecone-io/pinecone-local:latest`), control plane on
  `:5080`, per-index data plane on `:5081+`

The semantic-similarity backend is **not mocked** — it's the real Pinecone
REST API surface, in-memory.

## Run

```sh
make run
```

That command builds the wasm, installs the policy into
`./playground/config/custom-policies`, and starts the three containers.

## One-time index setup

Pinecone Local boots empty. Create the index the policy expects:

```sh
curl -sS -X POST http://localhost:5080/indexes \
  -H 'Api-Key: any' \
  -H 'Content-Type: application/json' \
  -d '{
    "name": "playground",
    "dimension": 3,
    "metric": "cosine",
    "spec": {"serverless": {"cloud": "aws", "region": "us-east-1"}}
  }'
```

Replace `dimension: 3` with whatever your real embedder returns
(`text-embedding-3-small` → `1536`).

## Send a request

```sh
curl -sS -X POST http://localhost:8185/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}' \
  -i
```

First call → `X-Cache: miss`; the policy upserts a vector to Pinecone Local.
Send the *exact same* body again → `X-Cache: hit-exact` (served from the
platform shared KV; embedder + Pinecone not touched). A *paraphrased* prompt
that produces a similar embedding → `X-Cache: hit-semantic` with
`X-Cache-Score`.

## Inspect Pinecone state

```sh
# How many vectors are in the index
curl -sS -X POST http://localhost:5081/describe_index_stats \
  -H 'Api-Key: any' -H 'Content-Type: application/json' -d '{}'

# Run a query directly
curl -sS -X POST http://localhost:5081/query \
  -H 'Api-Key: any' -H 'Content-Type: application/json' \
  -d '{"vector":[0.1,0.2,0.3],"topK":3,"includeMetadata":true}'
```

## Hit the real APIs instead

Copy `config/api.yaml` to `config/api.local.yaml` (gitignored) and swap
`embedder.baseUrl`/`vectordb.indexHost` to the real OpenAI + Pinecone URLs
plus real API keys. Drop the `pinecone-local` service from the compose
file if you don't need the emulator anymore. Restart with `make run`.
