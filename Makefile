SHELL := /bin/bash
.PHONY: check dev-up run test-s3 test-nx dev-down dev-clean

check:
	cargo fmt --check
	cargo clippy --locked --all-targets -- -D warnings
	cargo test --locked
	python3 tests/release.py

dev-up:
	docker compose up -d --wait minio
	docker compose run --rm init

run:
	set -a; source .env; set +a; cargo run --locked --bin nx-cache-aws

test-s3:
	cargo build --locked --bin nx-cache-aws
	python3 tests/smoke.py s3

test-nx:
	cargo build --locked --bin nx-cache-aws
	npm ci --prefix tests/nx
	python3 tests/smoke.py nx

dev-down:
	docker compose down

# Explicitly destructive, unlike dev-down. Only this Compose project's local data.
dev-clean:
	docker compose down --volumes
