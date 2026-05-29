TARGET                := wasm32-wasip1
TARGET_DIR            := ./target/$(TARGET)/release
REGISTRATION_TEMPLATE := $(CURDIR)/test-resources/registration.template
RUST_BACKTRACE         = 1

.PHONY: definitions-build
definitions-build: ## Build all 'definition'
	@echo "Building all definitions..."
	@for makefile in $$(find . -type f -path "*/definition/Makefile"); do \
		( cd "$$(dirname $$makefile)" && make build ) || exit 1; \
	done

.PHONY: definitions-publish
definitions-publish: ## Publish all 'definition'
	@for makefile in $$(find . -type f -path "*/definition/Makefile"); do \
		( cd "$$(dirname $$makefile)" && make publish ) || exit 1; \
	done

.PHONY: definitions-publish-local
definitions-publish-local: ## Publish all 'definition' to local fs
	@for makefile in $$(find . -type f -path "*/definition/Makefile"); do \
		( cd "$$(dirname $$makefile)" && make publish-local ) || exit 1; \
	done

.PHONY: implementations-build
implementations-build: ## Build all 'implementation'
	@echo "Building all implementations..."
	@for makefile in $$(find . -type f -path "*/implementation/Makefile"); do \
		( cd "$$(dirname $$makefile)" && make build ) || exit 1; \
	done

.PHONY: implementations-test-setup
implementations-test-setup: ## Copy registration template into each variant's tests/config
	@echo "Setting up integration test fixtures..."
	@for makefile in $$(find . -type f -path "*/implementation/Makefile"); do \
		dir=$$(dirname $$makefile); \
		mkdir -p "$$dir/tests/config"; \
		cp "$(REGISTRATION_TEMPLATE)" "$$dir/tests/config/registration.yaml"; \
	done

.PHONY: implementations-test
implementations-test: implementations-test-setup ## Run tests for all 'implementation'
	@echo "Running all implementation tests..."
	@for makefile in $$(find . -type f -path "*/implementation/Makefile"); do \
		( cd "$$(dirname $$makefile)" && make test ) || exit 1; \
	done

.PHONY: implementations-publish
implementations-publish: ## Publish all 'implementation'
	@for makefile in $$(find . -type f -path "*/implementation/Makefile"); do \
		( cd "$$(dirname $$makefile)" && make publish ) || exit 1; \
	done

.PHONY: build
build: definitions-build implementations-build ## Build everything

.PHONY: test
test: ## Run unit tests for the shared core + all implementation tests
	@cargo test -p ai_semantic_cache_core
	@$(MAKE) implementations-test

.PHONY: check
check: ## Type-check the workspace
	@cargo check --workspace

.PHONY: clean
clean: ## Clean build artifacts
	@cargo clean

.PHONY: setup
setup: ## Install required tools
	@cargo install cargo-anypoint@1.8.0

.PHONY: help
help:
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'
