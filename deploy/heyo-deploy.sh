#!/usr/bin/env bash
# Build the three images, push them, and deploy them as heyo microVMs.
#
# heyo's firecracker_containerd backend pulls ordinary OCI references, so the
# Dockerfiles in this repo ARE the VM image build — there is no separate
# image format to produce.
#
# Required:
#   REGISTRY           where to push, e.g. ghcr.io/your-org
#   MEILI_MASTER_KEY   Meilisearch admin key
#   MCP_AUTH_TOKEN     bearer token the MCP endpoint will require
#   CONTACT_EMAIL      a real address, required by OSM/Wikimedia/Nominatim
# Optional:
#   TAG (default: git short sha), REGION (US|EU), SIZE_CLASS, CRAWL_SEEDS,
#   PREFIX (default: regional)
#   REGION_FILE  which region config to bake in (default region.colorado.toml)
#
# Secrets should come from HeyoSecret rather than your shell history:
#   MEILI_MASTER_KEY=$(heyo-secret get regional/meili-master) deploy/heyo-deploy.sh
set -euo pipefail

: "${REGISTRY:?set REGISTRY, e.g. ghcr.io/your-org}"
: "${MEILI_MASTER_KEY:?set MEILI_MASTER_KEY}"
: "${MCP_AUTH_TOKEN:?set MCP_AUTH_TOKEN}"
: "${CONTACT_EMAIL:?set CONTACT_EMAIL}"

TAG="${TAG:-$(git rev-parse --short HEAD 2>/dev/null || date +%s)}"
REGION_FILE="${REGION_FILE:-region.colorado.toml}"
REGION="${REGION:-US}"
SIZE_CLASS="${SIZE_CLASS:-small}"
PREFIX="${PREFIX:-regional}"
BACKEND=firecracker_containerd
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

command -v heyvm >/dev/null || { echo "heyvm is not installed"; exit 1; }

say() { printf '\n==> %s\n' "$*"; }

# ---------------------------------------------------------------- build

for svc in meilisearch bot mcp dashboard; do
  say "building ${PREFIX}-${svc}:${TAG} (region: ${REGION_FILE})"
  # meilisearch takes no region config; the build arg is ignored there.
  docker build -f "${ROOT}/${svc}/Dockerfile" \
    --build-arg "REGION_FILE=${REGION_FILE}" \
    -t "${REGISTRY}/${PREFIX}-${svc}:${TAG}" "${ROOT}"
  docker push "${REGISTRY}/${PREFIX}-${svc}:${TAG}"
done

# ---------------------------------------------------------- meilisearch
#
# Deployed first, and privately: only the bot and the MCP server should be
# able to reach the database.
#
# Note on durability: firecracker_containerd does not take `--mount`, and
# heyo's named volumes are host directories rather than cloud disks. The
# index is deliberately reconstructible — the indexer rebuilds it from the
# upstream sources — so losing this VM costs a re-index, not data. `--no-ttl`
# keeps it from being reaped in the meantime.
say "deploying ${PREFIX}-meilisearch"
heyvm create --cloud \
  --name "${PREFIX}-meilisearch" \
  --backend "${BACKEND}" \
  --region "${REGION}" \
  --size-class "${SIZE_CLASS}" \
  --image "${REGISTRY}/${PREFIX}-meilisearch:${TAG}" \
  --env "MEILI_MASTER_KEY=${MEILI_MASTER_KEY}" \
  --port 7700 \
  --private \
  --health-path /health \
  --health-timeout 180s \
  --no-ttl \
  --format json

MEILI_URL=$(heyvm bind "${PREFIX}-meilisearch" 7700 --private --format json \
  | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d.get("url") or d.get("hostname") or "")')

if [ -z "${MEILI_URL}" ]; then
  echo "could not determine the meilisearch URL; check: heyvm list --format json" >&2
  exit 1
fi
say "meilisearch reachable at ${MEILI_URL}"

# A search-only key for the one service that faces the internet.
export MEILI_URL MEILI_MASTER_KEY
MEILI_SEARCH_KEY="$("${ROOT}/deploy/create-search-key.sh")"

# ------------------------------------------------------------------ bot

say "deploying ${PREFIX}-bot"
heyvm create --cloud \
  --name "${PREFIX}-bot" \
  --backend "${BACKEND}" \
  --region "${REGION}" \
  --size-class "${SIZE_CLASS}" \
  --image "${REGISTRY}/${PREFIX}-bot:${TAG}" \
  --env "MEILI_URL=${MEILI_URL}" \
  --env "MEILI_MASTER_KEY=${MEILI_MASTER_KEY}" \
  --env "CONTACT_EMAIL=${CONTACT_EMAIL}" \
  --env "CRAWL_SEEDS=${CRAWL_SEEDS:-}" \
  --env "RUST_LOG=${RUST_LOG:-info}" \
  --port 8081 \
  --private \
  --health-path /healthz \
  --no-ttl \
  --format json

# ------------------------------------------------------------------ mcp

say "deploying ${PREFIX}-mcp"
heyvm create --cloud \
  --name "${PREFIX}-mcp" \
  --backend "${BACKEND}" \
  --region "${REGION}" \
  --size-class "${SIZE_CLASS}" \
  --image "${REGISTRY}/${PREFIX}-mcp:${TAG}" \
  --env "MEILI_URL=${MEILI_URL}" \
  --env "MEILI_SEARCH_KEY=${MEILI_SEARCH_KEY}" \
  --env "MCP_AUTH_TOKEN=${MCP_AUTH_TOKEN}" \
  --env "RUST_LOG=${RUST_LOG:-info}" \
  --port 8080 \
  --health-path /healthz \
  --no-ttl \
  --auto-bind \
  --format json

# ------------------------------------------------------------ dashboard
#
# Two ports, and the split is the whole access policy: 8090 is the operator
# dashboard and is bound private, 8091 is the public request form and is
# bound openly. Neither has authentication of its own — that is the load
# balancer's job, which is exactly why they are separate ports.
say "deploying ${PREFIX}-dashboard"
heyvm create --cloud \
  --name "${PREFIX}-dashboard" \
  --backend "${BACKEND}" \
  --region "${REGION}" \
  --size-class "${SIZE_CLASS}" \
  --image "${REGISTRY}/${PREFIX}-dashboard:${TAG}" \
  --env "MEILI_URL=${MEILI_URL}" \
  --env "MEILI_MASTER_KEY=${MEILI_MASTER_KEY}" \
  --env "BOT_HEALTH_URL=${BOT_URL:-http://${PREFIX}-bot:8081}/healthz" \
  --env "PUBLIC_TITLE=${PUBLIC_TITLE:-}" \
  --env "PUBLIC_CONTACT=${PUBLIC_CONTACT:-}" \
  --env "SUBMIT_PER_HOUR=${SUBMIT_PER_HOUR:-10}" \
  --env "RUST_LOG=${RUST_LOG:-info}" \
  --port 8090 \
  --port 8091 \
  --health-path /healthz \
  --no-ttl \
  --format json

# Bind each port explicitly: the admin one restricted to account members,
# the request form open to the public.
say "binding dashboard ports"
heyvm bind "${PREFIX}-dashboard" 8090 --private --format json
heyvm bind "${PREFIX}-dashboard" 8091 --format json

say "done"
cat <<NOTE

Find every URL with:

  heyvm list --format json

  MCP endpoint      <mcp-url>/mcp        header: Authorization: Bearer \$MCP_AUTH_TOKEN
  Operator dashboard <dashboard-url:8090>  bound private — put your load
                                           balancer's auth in front of it
  Public request form <dashboard-url:8091>  open on purpose

The dashboard has no authentication of its own. Port 8090 is bound
--private so only account members reach it; if you want stronger control,
that is what the app-lb sits in front of. Port 8091 must stay open — it is
the page people use to ask for changes.

NOTE
