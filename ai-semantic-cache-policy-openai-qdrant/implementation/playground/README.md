# Playground — `ai-semantic-cache-policy-openai-qdrant`

Local Docker stack to exercise the Qdrant variant end-to-end:

- **local-flex** — Omni Replica (`mulesoft/flex-gateway:1.11.0`) on `:8185`
- **mock-service** — MockServer canned to look like an OpenAI-compatible
  embeddings endpoint and an upstream LLM, on `:8080`
- **qdrant** — real [Qdrant](https://qdrant.tech/) OSS container, in-memory,
  on `:6333`

The semantic-similarity backend is **not mocked** — it's the real Qdrant
REST surface.

## Run

```sh
make run
```

## One-time collection setup

Qdrant boots empty AND requires explicit payload indexes on filter fields
— unlike Pinecone, it does not auto-index metadata. The policy filters
every semantic query by the `(namespace, model)` payload pair, so both
fields must be indexed before traffic hits Qdrant or `/points/search`
will 400 with `Index required but not found`.

Use the bundled idempotent script:

```sh
# Local Docker (defaults: http://localhost:6333, collection=playground, dimension=3)
./init-qdrant.sh

# Real model (e.g. text-embedding-3-small → 1536)
COLLECTION=ai-semantic-cache DIMENSION=1536 ./init-qdrant.sh

# Qdrant Cloud
QDRANT_URL="https://<cluster>.<region>.aws.cloud.qdrant.io" \
QDRANT_API_KEY="<your-qdrant-cloud-api-key>" \
COLLECTION=ai-semantic-cache \
DIMENSION=1536 \
  ./init-qdrant.sh
```

The script:
1. Creates the collection (Cosine distance, `DIMENSION` size) if absent.
2. Ensures `keyword` payload indexes on `namespace` and `model`.

It's safe to re-run (PUT is idempotent on both endpoints).

## Send a request

```sh
curl -sS -X POST http://localhost:8185/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}' \
  -i
```

First call → `X-Cache: miss`; the policy upserts a point to Qdrant. Send
the *exact same* body again → `X-Cache: hit-exact` (served from the
platform shared KV; embedder + Qdrant not touched). A *paraphrased* prompt
producing a similar embedding → `X-Cache: hit-semantic` with
`X-Cache-Score`.

## Inspect Qdrant state

```sh
# Collection info + counts
curl -sS http://localhost:6333/collections/playground

# Run a search directly
curl -sS -X POST http://localhost:6333/collections/playground/points/search \
  -H 'Content-Type: application/json' \
  -d '{"vector":[0.1,0.2,0.3],"limit":3,"with_payload":true}'
```

## Hit Qdrant Cloud instead

`config/api.yaml` is gitignored so it can hold operator-local overrides.
Copy `config/api.example.yaml` to `config/api.yaml`, set `vectordb.baseUrl`
to `https://<cluster>.<region>.aws.cloud.qdrant.io:6333` and
`vectordb.apiKey` to your Qdrant Cloud key. Drop the `qdrant` service
from the compose file. Run `./init-qdrant.sh` against the Cloud cluster
(see One-time collection setup) so the payload indexes exist there too.
Restart with `make run`.
