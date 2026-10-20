# claude-gateway — run with Docker (recommended) or as a systemd user service.
#   make up        build the image and start the container (needs .env)
#   make install   build and start the systemd user service instead

SERVICE   := claude-gateway
UNIT_DIR  := $(HOME)/.config/systemd/user
UNIT      := $(UNIT_DIR)/$(SERVICE).service
BIN       := $(CURDIR)/target/release/claude-gateway
WORKDIR   := $(CURDIR)/workspace
SYSTEMCTL := systemctl --user

# Admin API helpers: read GATEWAY_ADMIN_KEY from .env.
ADMIN_KEY = $$(grep '^GATEWAY_ADMIN_KEY=' .env | cut -d= -f2-)
GW_URL   ?= http://127.0.0.1:8443
ADMIN    = curl -sS -H "x-api-key: $(ADMIN_KEY)" -H 'content-type: application/json'
PRETTY   = | python3 -m json.tool --no-ensure-ascii

.PHONY: chat key-create key-list key-disable key-enable key-delete
.PHONY: help build check-env run install uninstall start stop restart status logs \
        up down docker-build docker-restart docker-logs docker-status

help:
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  make %-15s %s\n", $$1, $$2}'

build: ## Build the release binary
	cargo build --release

check-env:
	@test -f .env || { echo "Missing .env. Run: cp .env.example .env, then edit it"; exit 1; }

run: build check-env ## Run in the foreground with .env (Ctrl-C to stop)
	set -a; GATEWAY_WORKDIR=$(WORKDIR); . ./.env; set +a; exec $(BIN)

install: build check-env ## Install the systemd user service and start it
	mkdir -p $(UNIT_DIR)
	printf '%s\n' \
	  '[Unit]' \
	  'Description=claude-gateway (Messages API over the local Claude CLI)' \
	  'After=network-online.target' \
	  '' \
	  '[Service]' \
	  'Environment=GATEWAY_WORKDIR=$(WORKDIR)' \
	  'Environment=PATH=$(HOME)/.local/bin:/usr/local/bin:/usr/bin:/bin' \
	  'EnvironmentFile=$(CURDIR)/.env' \
	  'ExecStart=$(BIN)' \
	  'Restart=on-failure' \
	  'RestartSec=5' \
	  '' \
	  '[Install]' \
	  'WantedBy=default.target' > $(UNIT)
	$(SYSTEMCTL) daemon-reload
	$(SYSTEMCTL) enable --now $(SERVICE)
	@echo "Installed. To keep it running after logout: loginctl enable-linger $$USER"

uninstall: ## Stop the service and remove the unit
	-$(SYSTEMCTL) disable --now $(SERVICE)
	rm -f $(UNIT)
	$(SYSTEMCTL) daemon-reload

start: ## Start the service
	$(SYSTEMCTL) start $(SERVICE)

stop: ## Stop the service
	$(SYSTEMCTL) stop $(SERVICE)

restart: build ## Rebuild and restart the service
	$(SYSTEMCTL) restart $(SERVICE)

status: ## Show service status
	-$(SYSTEMCTL) status $(SERVICE) --no-pager

logs: ## Follow service logs
	journalctl --user -u $(SERVICE) -f

# ---- Docker ----
COMPOSE := docker compose

docker-build: ## Docker: build the image
	$(COMPOSE) build

up: check-env ## Docker: build and start the container
	$(COMPOSE) up -d --build

down: ## Docker: stop and remove the container
	$(COMPOSE) down

docker-restart: check-env ## Docker: rebuild and restart the container
	$(COMPOSE) up -d --build --force-recreate

docker-logs: ## Docker: follow container logs
	$(COMPOSE) logs -f

docker-status: ## Docker: show container status
	$(COMPOSE) ps

# ---- API keys (gateway must be running) ----
key-create: check-env ## Create a client key: make key-create NAME=my-app
	@test -n "$(NAME)" || { echo "usage: make key-create NAME=my-app"; exit 1; }
	@$(ADMIN) -X POST $(GW_URL)/admin/keys -d '{"name":"$(NAME)"}' $(PRETTY)
	@echo "Save the \"key\" value now: it is not shown again."

key-list: check-env ## List client keys and their usage
	@$(ADMIN) $(GW_URL)/admin/keys $(PRETTY)

key-disable: check-env ## Disable a key: make key-disable ID=key_...
	@$(ADMIN) -X PATCH $(GW_URL)/admin/keys/$(ID) -d '{"status":"disabled"}' $(PRETTY)

key-enable: check-env ## Re-enable a key: make key-enable ID=key_...
	@$(ADMIN) -X PATCH $(GW_URL)/admin/keys/$(ID) -d '{"status":"active"}' $(PRETTY)

key-delete: check-env ## Delete a key for good: make key-delete ID=key_...
	@$(ADMIN) -X DELETE $(GW_URL)/admin/keys/$(ID) $(PRETTY)

# ---- Browser chat example ----
chat: ## Browser chat UI on http://127.0.0.1:8765 (RAG over examples/docs/)
	uv run examples/web.py
