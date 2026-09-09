# Azure AI Search emulator — development tasks.
#
# Default target (`make` or `make test`) runs the Python SDK E2E suite. The
# Docker image is built automatically on first use (see conftest.py).

PYTHON_DIR := source/tests/python
VENV       := .venv
RUST_DIR   := source/rust
IMAGE      := aisearch-emulator
MS_SUBMODULE := source/tests/python/ms_samples/upstream
MS_SAMPLES_PATH := sdk/search/azure-search-documents/samples

.DEFAULT_GOAL := test

.PHONY: help
help:
	@echo "Azure AI Search emulator — make targets"
	@echo ""
	@echo "  test     Run the Python SDK E2E tests (default)"
	@echo "  rust     Run Rust fmt, clippy, and unit/contract tests"
	@echo "  docker   Build the emulator Docker image ($(IMAGE))"
	@echo "  setup    Create the Python virtualenv and install dependencies"
	@echo "  ms-samples       Populate the Microsoft samples submodule (sparse, shallow)"
	@echo "  ms-samples-update  Bump the samples submodule to latest upstream main"
	@echo "  test-ms  Run the Microsoft reference-samples compatibility probe"
	@echo "  all      Run rust checks, build the image, and run the E2E tests"
	@echo "  clean    Remove the Python virtualenv and captured fixtures"

.PHONY: setup
setup:
	cd $(PYTHON_DIR) && poetry install

.PHONY: test
test:
	@test -x $(PYTHON_DIR)/$(VENV)/bin/python || (echo "virtualenv missing — run 'make setup' first" && exit 1)
	cd $(PYTHON_DIR) && ./$(VENV)/bin/python -m pytest tests/e2e -v

.PHONY: ms-samples
ms-samples:
	git submodule update --init --filter=blob:none --depth 1 $(MS_SUBMODULE)
	git -C $(MS_SUBMODULE) sparse-checkout set $(MS_SAMPLES_PATH)

.PHONY: ms-samples-update
ms-samples-update:
	git -C $(MS_SUBMODULE) fetch origin main --depth 1 --filter=blob:none
	git -C $(MS_SUBMODULE) checkout origin/main
	git -C $(MS_SUBMODULE) sparse-checkout set $(MS_SAMPLES_PATH)
	@echo "Updated to latest upstream main; stage the new pointer with: git add $(MS_SUBMODULE)"

.PHONY: test-ms
test-ms: ms-samples
	@test -x $(PYTHON_DIR)/$(VENV)/bin/python || (echo "virtualenv missing — run 'make setup' first" && exit 1)
	cd $(PYTHON_DIR) && ./$(VENV)/bin/python -m pytest tests/ms_samples -v

.PHONY: rust
rust:
	cargo fmt --manifest-path $(RUST_DIR)/Cargo.toml -- --check
	cargo clippy --manifest-path $(RUST_DIR)/Cargo.toml --all-targets -- -D warnings
	cargo test --manifest-path $(RUST_DIR)/Cargo.toml --all-targets

.PHONY: docker
docker:
	docker build -t $(IMAGE) $(RUST_DIR)

.PHONY: all
all: rust docker test

.PHONY: clean
clean:
	rm -rf $(PYTHON_DIR)/$(VENV) $(PYTHON_DIR)/fixtures
