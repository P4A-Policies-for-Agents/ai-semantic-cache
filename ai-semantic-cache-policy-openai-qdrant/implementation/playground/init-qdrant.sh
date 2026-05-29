#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Salesforce, Inc.
#
# Idempotent one-shot setup for a Qdrant collection used by
# ai-semantic-cache-policy-openai-qdrant.
#
# What this does:
#   1. Creates the collection if it doesn't exist (Cosine distance, the
#      vector size you supply).
#   2. Creates two `keyword` payload indexes — `namespace` and `model` —
#      so the policy's `(namespace, model)` filter on `/points/search`
#      doesn't 400 with "Index required but not found".
#
# Why payload indexes:
#   Qdrant does NOT auto-index payload fields the way Pinecone auto-indexes
#   metadata. Filters against unindexed keyword fields are rejected. The
#   ai-semantic-cache policy filters every semantic query by the
#   `namespace` and `model` payload fields it writes alongside each
#   vector, so both fields must be indexed before traffic hits Qdrant.
#
# Usage:
#   QDRANT_URL=https://<cluster>.<region>.aws.cloud.qdrant.io \
#   QDRANT_API_KEY=<your-qdrant-key> \
#   COLLECTION=ai-semantic-cache \
#   DIMENSION=1536 \
#     ./init-qdrant.sh
#
# Defaults work for the local Docker container used by docker-compose.yaml
# (no auth, http://localhost:6333, dimension 3 for the mock playground).

set -euo pipefail

QDRANT_URL="${QDRANT_URL:-http://localhost:6333}"
QDRANT_API_KEY="${QDRANT_API_KEY:-}"
COLLECTION="${COLLECTION:-playground}"
DIMENSION="${DIMENSION:-3}"

auth=()
if [[ -n "$QDRANT_API_KEY" ]]; then
  auth=(-H "api-key: $QDRANT_API_KEY")
fi

echo "==> Target: $QDRANT_URL/collections/$COLLECTION (dimension=$DIMENSION)"

# 1. Create collection if missing.
status=$(curl -sS -o /tmp/qdrant-coll.json -w '%{http_code}' \
  "$QDRANT_URL/collections/$COLLECTION" "${auth[@]}")

if [[ "$status" == "200" ]]; then
  echo "    collection already exists — skipping create"
else
  echo "    creating collection (got $status from GET)"
  curl -sS -X PUT "$QDRANT_URL/collections/$COLLECTION" \
    "${auth[@]}" -H 'Content-Type: application/json' \
    -d "{\"vectors\":{\"size\":$DIMENSION,\"distance\":\"Cosine\"}}" >/dev/null
fi

# 2. Create the two payload indexes the policy filters on. PUT is
#    idempotent — Qdrant returns 200 OK whether or not the index exists.
for field in namespace model; do
  echo "==> Ensuring payload index on \"$field\" (keyword)"
  curl -sS -X PUT "$QDRANT_URL/collections/$COLLECTION/index" \
    "${auth[@]}" -H 'Content-Type: application/json' \
    -d "{\"field_name\":\"$field\",\"field_schema\":\"keyword\"}" >/dev/null
done

echo "==> Done. Collection \"$COLLECTION\" is ready."
