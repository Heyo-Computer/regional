# regional — a region-scoped search stack.
#
# `make` on its own lists every target. The paths worth knowing:
#
#   make up         build and start the whole stack in Docker
#   make check      fmt + clippy + tests, the pre-commit gate
#   make bot-once   one cycle of every source, then exit
#   make health     probe every service
#
# Anything that builds an image takes REGION_FILE — the one file that makes
# this stack Colorado-specific, baked in at build time:
#
#   make up REGION_FILE=region.vermont.toml

CARGO       ?= cargo
COMPOSE     ?= docker compose
REGION_FILE ?= region.colorado.toml
PREFIX      ?= regional
TAG         ?= dev

SERVICES := meilisearch bot mcp dashboard

# compose reads REGION_FILE from the environment, so pass it down.
export REGION_FILE

# Host-side run targets read .env the same way compose does.
ENV := set -a; [ -f .env ] && . ./.env; set +a;

.DEFAULT_GOAL := help

##@ Development

build: ## Compile the workspace (debug)
	$(CARGO) build --workspace

release: ## Compile the workspace (release, locked)
	$(CARGO) build --workspace --release --locked

test: ## Run the unit tests — no network needed
	$(CARGO) test --workspace

clippy: ## Lint the workspace, warnings are errors
	$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt: ## Format the workspace
	$(CARGO) fmt --all

fmt-check: ## Fail if anything is unformatted
	$(CARGO) fmt --all -- --check

check: fmt-check clippy test ## fmt + clippy + tests — run this before committing

##@ Running on the host
#
# These talk to whatever MEILI_URL points at; `make meili` starts just the
# database in Docker and leaves the binaries to you.

meili: .env ## Start only Meilisearch, for running binaries on the host
	$(COMPOSE) up -d meilisearch

run-mcp: ## Run the MCP server on the host (port 8080)
	$(ENV) REGION_CONFIG=$(REGION_FILE) \
	  MEILI_URL=$${MEILI_URL:-http://127.0.0.1:7700} \
	  MEILI_SEARCH_KEY=$${MEILI_SEARCH_KEY:-$$MEILI_MASTER_KEY} \
	  $(CARGO) run -p mcp

run-bot: ## Run the indexer on the host (port 8081)
	$(ENV) REGION_CONFIG=$(REGION_FILE) \
	  MEILI_URL=$${MEILI_URL:-http://127.0.0.1:7700} \
	  $(CARGO) run -p bot

run-bot-once: ## One cycle of every source on the host, then exit
	$(ENV) REGION_CONFIG=$(REGION_FILE) \
	  MEILI_URL=$${MEILI_URL:-http://127.0.0.1:7700} \
	  $(CARGO) run -p bot -- --once

run-dashboard: ## Run the dashboard on the host (ports 8090/8091)
	$(ENV) REGION_CONFIG=$(REGION_FILE) \
	  MEILI_URL=$${MEILI_URL:-http://127.0.0.1:7700} \
	  $(CARGO) run -p dashboard

##@ The Docker stack

up: .env ## Build and start every service in the background
	$(COMPOSE) up --build -d
	@$(MAKE) --no-print-directory urls

up-fg: .env ## Same, but in the foreground with logs attached
	$(COMPOSE) up --build

down: ## Stop the stack, keep the index
	$(COMPOSE) down

down-hard: ## Stop the stack and delete the index volume — costs a full re-index
	$(COMPOSE) down --volumes

restart: ## Restart the services without rebuilding
	$(COMPOSE) restart

ps: ## What is running
	$(COMPOSE) ps

logs: ## Follow every service's logs
	$(COMPOSE) logs -f --tail=100

logs-%: ## Follow one service's logs (logs-bot, logs-mcp, ...)
	$(COMPOSE) logs -f --tail=100 $*

shell-%: ## Open a shell in a running service (shell-bot, shell-mcp, ...)
	$(COMPOSE) exec $* /bin/sh

bot-once: .env ## One cycle of every source in Docker, then exit
	$(COMPOSE) run --rm bot --once

health: ## Probe every health endpoint
	@fail=0; \
	for probe in "meilisearch 7700 /health" "mcp 8080 /healthz" "bot 8081 /healthz" \
	             "dashboard 8090 /healthz" "public-form 8091 /healthz"; do \
	  set -- $$probe; \
	  if curl -fsS -m 3 "http://127.0.0.1:$$2$$3" >/dev/null 2>&1; then \
	    printf '  ok    %-12s :%s\n' "$$1" "$$2"; \
	  else \
	    printf '  DOWN  %-12s :%s\n' "$$1" "$$2"; fail=1; \
	  fi; \
	done; exit $$fail

urls: ## Print where everything is listening
	@echo
	@echo "  operator dashboard   http://localhost:8090"
	@echo "  public request form  http://localhost:8091"
	@echo "  MCP endpoint         http://localhost:8080/mcp   (Authorization: Bearer \$$MCP_AUTH_TOKEN)"
	@echo "  indexer health       http://localhost:8081/healthz"
	@echo "  meilisearch          http://localhost:7700"
	@echo

##@ Images

images: $(addprefix image-,$(SERVICES)) ## Build all four images, tagged regional-<svc>:dev

image-%: ## Build one image (image-mcp, image-bot, image-dashboard, image-meilisearch)
	docker build -f $*/Dockerfile --build-arg REGION_FILE=$(REGION_FILE) \
	  -t $(PREFIX)-$*:$(TAG) .

##@ Deployment

deploy: ## Build, push and deploy to heyo — needs REGISTRY and the secrets
	deploy/heyo-deploy.sh

search-key: ## Mint (or reuse) the search-only Meilisearch key and print it
	@$(ENV) MEILI_URL=$${MEILI_URL:-http://127.0.0.1:7700} deploy/create-search-key.sh

##@ Housekeeping

# Never overwritten: an existing .env is only touched, so a newer
# .env.example does not cost you your secrets.
.env: .env.example
	@if [ -f $@ ]; then touch $@; else \
	  sed -e "s|^MEILI_MASTER_KEY=.*|MEILI_MASTER_KEY=$$(openssl rand -hex 32)|" \
	      -e "s|^MCP_AUTH_TOKEN=.*|MCP_AUTH_TOKEN=$$(openssl rand -hex 32)|" \
	      .env.example > $@; \
	  echo "wrote .env with fresh secrets — now set CONTACT_EMAIL to a real"; \
	  echo "address you monitor: OSM, Wikimedia and Nominatim all require one"; \
	  echo "and the indexer refuses to start without it."; \
	fi

env: .env ## Create .env from the example, with generated secrets
	@echo "$(CURDIR)/.env is ready"

clean: ## cargo clean
	$(CARGO) clean

clean-all: clean ## cargo clean, plus the stack, its volume and its images
	-$(COMPOSE) down --volumes --rmi local

help: ## List every target
	@awk 'BEGIN { FS = ":.*##" } \
	     /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5); next } \
	     /^[a-zA-Z0-9_%-]+:.*##/ { printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2 } \
	     END { print "" }' $(MAKEFILE_LIST)

.PHONY: build release test clippy fmt fmt-check check \
        meili run-mcp run-bot run-bot-once run-dashboard \
        up up-fg down down-hard restart ps logs bot-once health urls \
        images deploy search-key env clean clean-all help
