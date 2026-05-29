# AI Semantic Cache Policy Family

Cache LLM responses keyed by **semantic similarity of the prompt** rather than exact-string match, at the gateway boundary.

See [docs/architecture.md](./docs/architecture.md) for design and [docs/competitive_analysis.md](./docs/competitive_analysis.md) for positioning.

## Variants

| Variant | Embedder | Vector store | Exact-cache | Local dev | Status |
|---|---|---|---|---|---|
| [`ai-semantic-cache-policy-openai-pinecone`](./ai-semantic-cache-policy-openai-pinecone/) | OpenAI-compatible (`/v1/embeddings`) | Pinecone (serverless REST) | PDK shared KV (`DataStorageBuilder.remote`) | Pinecone Local emulator | MVP |
| [`ai-semantic-cache-policy-openai-qdrant`](./ai-semantic-cache-policy-openai-qdrant/) | OpenAI-compatible | Qdrant (OSS or Cloud) | PDK shared KV | Qdrant OSS in Docker | shipped |
| [`ai-semantic-cache-policy-openai-azure-ai-search`](./ai-semantic-cache-policy-openai-azure-ai-search/) | OpenAI-compatible | Azure AI Search | PDK shared KV | MockServer (no local emulator from Azure) | shipped |

## Prerequisites

- **Docker Desktop** (mac/Windows). Integration tests for the
  Pinecone and Qdrant variants run a host-side vector-DB container
  (`pinecone-local` / `qdrant`) alongside the `pdk-test` composite, and
  rely on Docker Desktop's auto-resolved `host.docker.internal` for the
  gateway container to reach it. Plain Linux Docker is **not supported**
  for these tests because `pdk-test` does not expose a way to add
  `--add-host=host.docker.internal:host-gateway` to the gateway
  container.
- `cargo-anypoint` 1.8.0 (installed via `make setup`).
- `anypoint-cli-v4` for `make test` (installs the wasm into
  `policies_config/` so the gateway can load it).

## Build

Always go through `make` — the Makefile fans out to each `definition/` and `implementation/` sub-Makefile, the standard layout for split-model PDK policies.

```sh
cd policies/llm/ai-semantic-cache
make setup                  # one-time: install cargo-anypoint
make build                  # build all definitions + implementations (wasm)
make test                   # core unit tests + implementation integration tests
make implementations-build  # only the wasm artifacts
make definitions-build      # only the policy definitions
make help                   # list all targets
```
