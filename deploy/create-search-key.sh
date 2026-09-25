#!/usr/bin/env bash
# Create (or reuse) a search-only Meilisearch key for the MCP service.
#
# The MCP server is the one component exposed to the internet, and it only
# ever reads. Giving it the master key would make a compromise a write
# compromise, so it gets a key scoped to search on the three content indexes
# plus `mcp_tokens`, where it checks the per-user tokens minted on the
# dashboard (hashes only — searching it reveals no usable token).
#
# Meilisearch cannot change a key's indexes after creation, so a key from
# before `mcp_tokens` existed is not reused; a new one is created instead,
# and the old one can be deleted once the MCP service runs on the new one.
#
#   MEILI_URL=http://127.0.0.1:7700 MEILI_MASTER_KEY=... deploy/create-search-key.sh
#
# Prints the key to stdout and nothing else, so it can be captured:
#   MEILI_SEARCH_KEY=$(deploy/create-search-key.sh)
set -euo pipefail

: "${MEILI_URL:?set MEILI_URL}"
: "${MEILI_MASTER_KEY:?set MEILI_MASTER_KEY}"

NAME="regional-mcp-search"
AUTH=(-H "Authorization: Bearer ${MEILI_MASTER_KEY}" -H "Content-Type: application/json")

existing=$(curl -fsS "${AUTH[@]}" "${MEILI_URL}/keys?limit=100" \
  | python3 -c "
import json,sys
keys = json.load(sys.stdin).get('results', [])
print(next((k['key'] for k in keys
            if k.get('name') == '${NAME}' and 'mcp_tokens' in k.get('indexes', [])), ''))
")

if [ -n "${existing}" ]; then
  echo >&2 "reusing the existing ${NAME} key"
  echo "${existing}"
  exit 0
fi

curl -fsS -X POST "${AUTH[@]}" "${MEILI_URL}/keys" --data-binary @- <<'JSON' \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["key"])'
{
  "name": "regional-mcp-search",
  "description": "Search-only key for the regional MCP server",
  "actions": ["search"],
  "indexes": ["places", "events", "articles", "mcp_tokens"],
  "expiresAt": null
}
JSON
