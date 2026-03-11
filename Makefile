.PHONY: build test fmt lint check release release-server release-client \
        docker-build docker-run docker-run-monitoring docker-stop docker-logs \
        deploy deploy-client clean help

# ── configuration ─────────────────────────────────────────────────────────────

BINARY_SERVER := rustunnel-server
BINARY_CLIENT := rustunnel
IMAGE         := rustunnel-server
TAG           ?= latest

# ── development ───────────────────────────────────────────────────────────────

## build        Compile all workspace crates (debug).
build:
	cargo build --workspace

## test         Run the full test suite (unit + integration).
test:
	cargo test --workspace

## fmt          Format all source files in place.
fmt:
	cargo fmt --all

## lint         Run clippy with warnings-as-errors.
lint:
	cargo clippy --workspace --all-targets -- -D warnings

## check        fmt check + lint — mirrors what CI runs.
check:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

# ── release ───────────────────────────────────────────────────────────────────

## ui-install   Install dashboard-ui npm dependencies.
ui-install:
	cd dashboard-ui && npm install

## ui-dev       Start the Vite dev server (proxies API to localhost:8443).
ui-dev:
	cd dashboard-ui && npm run dev

## ui-build     Build the dashboard and embed assets into the server crate.
ui-build:
	cd dashboard-ui && npm run build

## release      Build optimised release binaries (runs ui-build first).
release: ui-build
	cargo build --release -p rustunnel-server -p rustunnel-client
	@echo "Binaries: target/release/$(BINARY_SERVER)  target/release/$(BINARY_CLIENT)"

## release-server  Build only the server in release mode.
release-server:
	cargo build --release -p rustunnel-server

## release-client  Build only the client in release mode.
release-client:
	cargo build --release -p rustunnel-client

# ── docker ────────────────────────────────────────────────────────────────────

## docker-build  Build the Docker image (deploy/Dockerfile).
docker-build:
	docker build -f deploy/Dockerfile -t $(IMAGE):$(TAG) .

## docker-run   Start the server container (requires deploy/server.toml).
docker-run:
	docker compose -f deploy/docker-compose.yml up -d rustunnel-server

## docker-run-monitoring  Start server + Prometheus + Grafana.
docker-run-monitoring:
	docker compose -f deploy/docker-compose.yml --profile monitoring up -d

## docker-stop  Stop and remove all containers.
docker-stop:
	docker compose -f deploy/docker-compose.yml down

## docker-logs  Tail server container logs.
docker-logs:
	docker compose -f deploy/docker-compose.yml logs -f rustunnel-server

# ── deployment ────────────────────────────────────────────────────────────────

## deploy       Install server binary + systemd unit (requires root/sudo).
deploy: release-server
	install -Dm755 target/release/$(BINARY_SERVER) /usr/local/bin/$(BINARY_SERVER)
	install -Dm644 deploy/rustunnel.service /etc/systemd/system/rustunnel.service
	@id -u rustunnel > /dev/null 2>&1 || \
	    useradd --system --no-create-home --shell /usr/sbin/nologin rustunnel
	@mkdir -p /etc/rustunnel /var/lib/rustunnel
	@chown rustunnel:rustunnel /var/lib/rustunnel
	systemctl daemon-reload
	systemctl enable --now rustunnel.service
	@echo "rustunnel deployed and started."

## deploy-client  Install the client binary to /usr/local/bin.
deploy-client: release-client
	install -Dm755 target/release/$(BINARY_CLIENT) /usr/local/bin/$(BINARY_CLIENT)
	@echo "Installed: /usr/local/bin/$(BINARY_CLIENT)"

# ── housekeeping ──────────────────────────────────────────────────────────────

## clean        Remove the cargo target directory.
clean:
	cargo clean

## help         Show available targets.
help:
	@grep -E '^## ' Makefile | sed 's/^## /  /'
