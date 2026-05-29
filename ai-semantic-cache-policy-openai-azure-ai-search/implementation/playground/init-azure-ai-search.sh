#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Salesforce, Inc.
#
# Idempotent one-shot setup for an Azure AI Search index used by
# ai-semantic-cache-policy-openai-azure-ai-search.
#
# What this does:
#   Creates (or updates) the index with the schema the policy expects:
#     id          Edm.String           — key, retrievable
#     namespace   Edm.String           — filterable, retrievable
#     model       Edm.String           — filterable, retrievable
#     prompt_sha  Edm.String           — retrievable
#     cached      Edm.String           — retrievable (stores payload JSON)
#     <vectorField>  Collection(Edm.Single) — searchable, with vector
#                    profile (Cosine, configurable dimensions)
#
# Why explicit setup:
#   Azure AI Search has NO auto-schema. The index, every field, and the
#   vector search profile must all be declared up-front. The policy
#   filters every search by `(namespace, model)` and reads back
#   `cached` + `prompt_sha`, so all of those fields must exist with
#   the right attributes or the first request 400s.
#
# Usage:
#   AZURE_SEARCH_ENDPOINT="https://<service>.search.windows.net" \
#   AZURE_SEARCH_API_KEY="<your-admin-key>" \
#   INDEX_NAME="ai-semantic-cache" \
#   DIMENSION=1536 \
#   VECTOR_FIELD="embedding" \
#   API_VERSION="2024-07-01" \
#     ./init-azure-ai-search.sh
#
# Defaults match the mock playground (small dim, single-shard).

set -euo pipefail

AZURE_SEARCH_ENDPOINT="${AZURE_SEARCH_ENDPOINT:-}"
AZURE_SEARCH_API_KEY="${AZURE_SEARCH_API_KEY:-}"
INDEX_NAME="${INDEX_NAME:-playground}"
DIMENSION="${DIMENSION:-3}"
VECTOR_FIELD="${VECTOR_FIELD:-embedding}"
API_VERSION="${API_VERSION:-2024-07-01}"

if [[ -z "$AZURE_SEARCH_ENDPOINT" || -z "$AZURE_SEARCH_API_KEY" ]]; then
  echo "ERROR: AZURE_SEARCH_ENDPOINT and AZURE_SEARCH_API_KEY must be set." >&2
  exit 1
fi

echo "==> Target: $AZURE_SEARCH_ENDPOINT/indexes/$INDEX_NAME (dimension=$DIMENSION, api-version=$API_VERSION)"

body=$(cat <<JSON
{
  "name": "$INDEX_NAME",
  "fields": [
    { "name": "id",         "type": "Edm.String", "key": true,
      "searchable": false, "filterable": false, "retrievable": true,
      "sortable": false, "facetable": false },
    { "name": "namespace",  "type": "Edm.String",
      "searchable": false, "filterable": true,  "retrievable": true,
      "sortable": false, "facetable": false },
    { "name": "model",      "type": "Edm.String",
      "searchable": false, "filterable": true,  "retrievable": true,
      "sortable": false, "facetable": false },
    { "name": "prompt_sha", "type": "Edm.String",
      "searchable": false, "filterable": false, "retrievable": true,
      "sortable": false, "facetable": false },
    { "name": "cached",     "type": "Edm.String",
      "searchable": false, "filterable": false, "retrievable": true,
      "sortable": false, "facetable": false },
    { "name": "$VECTOR_FIELD", "type": "Collection(Edm.Single)",
      "searchable": true, "retrievable": false,
      "dimensions": $DIMENSION,
      "vectorSearchProfile": "default-profile" }
  ],
  "vectorSearch": {
    "algorithms": [
      { "name": "default-hnsw", "kind": "hnsw",
        "hnswParameters": { "metric": "cosine", "m": 4,
                            "efConstruction": 400, "efSearch": 500 } }
    ],
    "profiles": [
      { "name": "default-profile", "algorithm": "default-hnsw" }
    ]
  }
}
JSON
)

# PUT is upsert in Azure AI Search — creates if absent, updates if
# present. Schema changes that aren't backward-compatible (e.g. changing
# a field type) will be rejected; in that case delete the index by hand
# and rerun.
status=$(curl -sS -o /tmp/azure-search.json -w '%{http_code}' \
  -X PUT "$AZURE_SEARCH_ENDPOINT/indexes/$INDEX_NAME?api-version=$API_VERSION" \
  -H "api-key: $AZURE_SEARCH_API_KEY" \
  -H 'Content-Type: application/json' \
  -d "$body")

if [[ "$status" =~ ^2[0-9][0-9]$ ]]; then
  echo "==> Done. Index \"$INDEX_NAME\" is ready (HTTP $status)."
else
  echo "==> Azure AI Search returned HTTP $status:" >&2
  cat /tmp/azure-search.json >&2
  echo >&2
  exit 1
fi
