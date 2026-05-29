ifdef ANYPOINT_ORG
ORG_ARG := --org-id $(ANYPOINT_ORG)
endif

TARGET                := wasm32-wasip1
TARGET_DIR            := ../../target/$(TARGET)/release
CARGO_ANYPOINT        := cargo-anypoint
CRATE_NAME             = $(shell cargo anypoint get-name)
DEFINITION_GAV         = $(shell cargo anypoint get-policy-definition-gav $(ORG_ARG))
POLICY_REF_NAME        = $(shell cargo anypoint get-policy-implementation-name)
DEFINITION_NAMESPACE  := default
MIN_FLEX_VERSION      := 1.11.0

.PHONY: setup
setup: ## Fetch dependencies
	@cargo fetch

.PHONY: config-gen
config-gen: ## Regenerate src/generated/config.rs from ../definition/gcl.yaml
	@cargo anypoint config-gen --manifest ../definition/gcl.yaml --output ./src/generated/config.rs $(ORG_ARG)

.PHONY: build
build: ## Build the policy implementation (wasm)
	@$(MAKE) config-gen
	@cargo build --target $(TARGET) --release
	@cargo anypoint build-policy --target=$(TARGET_DIR) --wasm=$(TARGET_DIR)/$(CRATE_NAME).wasm $(ORG_ARG) --min-runtime-version=$(MIN_FLEX_VERSION)

.PHONY: run
run: build ## Run the policy in local Flex
	@anypoint-cli-v4 pdk install-policy --target ./playground/config/custom-policies --path $(TARGET_DIR)/${CRATE_NAME}/implementation/
	-docker compose -f ./playground/docker-compose.yaml down
	docker compose -f ./playground/docker-compose.yaml up

.PHONY: test
test: build ## Run integration tests
	@anypoint-cli-v4 pdk install-policy --target ./policies_config --path $(TARGET_DIR)/${CRATE_NAME}/implementation/
	@export POLICY_REF_NAME=$(POLICY_REF_NAME); cargo test -- --nocapture --test-threads=1

.PHONY: test-ci
test-ci: build ## Run integration tests for CI (no anypoint-cli-v4 dependency)
	@mkdir -p ./policies_config
	@cp -r $(TARGET_DIR)/${CRATE_NAME}/implementation/* ./policies_config/
	@export POLICY_REF_NAME=$(POLICY_REF_NAME); cargo test -- --nocapture --test-threads=1

.PHONY: publish
publish: build ## Publish a development version
	@anypoint-cli-v4 pdk policy-wasm publish --wasm=$(TARGET_DIR)/$(CRATE_NAME).wasm --target=$(TARGET_DIR)/$(CRATE_NAME)/ --dev

.PHONY: publish-local
publish-local: build ## Publish to local fs
	@anypoint-cli-v4 pdk policy-wasm publish-local --wasm=$(TARGET_DIR)/$(CRATE_NAME).wasm --target=$(TARGET_DIR)/$(CRATE_NAME)/ --dev

.PHONY: release
release: build ## Publish a release version
	@anypoint-cli-v4 pdk policy-wasm publish --wasm=$(TARGET_DIR)/$(CRATE_NAME).wasm --target=$(TARGET_DIR)/$(CRATE_NAME)/

.PHONY: clean
clean: ## Clean build artifacts
	@cargo clean
