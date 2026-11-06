FC = gfortran
FFLAGS ?= -std=legacy -ffixed-line-length-72
FORTRAN_TARGET_DIR ?= target/fortran
FORTRAN_WRAPPER_OBJ = $(FORTRAN_TARGET_DIR)/dfor77_wrapper.o
FORTRAN_EXTENSION_OBJ = $(FORTRAN_TARGET_DIR)/dfor77_extension.o
FORTRAN_SAMPLE_BIN = $(FORTRAN_TARGET_DIR)/sample_moderation
FORTRAN_EXTENSION_BIN = $(FORTRAN_TARGET_DIR)/sample_moderation_extension
PACKAGE_TARGET_DIR ?= target/package-check
PUBLISH_TARGET_DIR ?= target/publish-check
STAGING_CONFIG ?= config/dfor77.sample.toml
STAGING_SESSION_DIR ?= ./staging-sessions
STAGING_RESTART_FILE ?= ./staging-restart
STAGING_AUDIT_LOG ?= staging-audit.jsonl
STAGING_BOT ?= $(FORTRAN_SAMPLE_BIN)
STAGING_LIVE_SECONDS ?= 86400
RUST_POLICY_DIRS = src $(wildcard tests)
TEXT_POLICY_PATHS = Cargo.toml Cargo.lock Makefile README.md DEVELOPMENT.md COPYING COPYING.LESSER \
	CHANGELOG.md SECURITY.md config $(wildcard examples) extension fortran include src \
	$(wildcard .gitignore .github tests)
STAGING_AUDIT_ACTIONS = \
	--audit-require-action D7SND \
	--audit-require-action D7DEL \
	--audit-require-action D7TMO \
	--audit-forbid-action D7UNC \
	--audit-require-live-actions
PACKAGE_REQUIRED_FILES = \
	Cargo.toml \
	Cargo.lock \
	CHANGELOG.md \
	COPYING \
	COPYING.LESSER \
	DEVELOPMENT.md \
	README.md \
	SECURITY.md \
	Makefile \
	config/dfor77.sample.toml \
	extension/dfor77_extension.f \
	fortran/dfor77_wrapper.f \
	include/dfor77.inc \
	src/audit.rs \
	src/audit/text.rs \
	src/config.rs \
	src/config/parse.rs \
	src/constants.rs \
	src/gateway.rs \
	src/gateway/debug.rs \
	src/gateway/limits.rs \
	src/gateway/members.rs \
	src/gateway/threads.rs \
	src/gateway/guild.rs \
	src/gateway/authority.rs \
	src/gateway/payload.rs \
	src/gateway/request_limit.rs \
	src/duration.rs \
	src/gateway_session.rs \
	src/ipc.rs \
	src/json.rs \
	src/lib.rs \
	src/event_queue.rs \
	src/memory.rs \
	src/main.rs \
	src/bot_process.rs \
	src/network.rs \
	src/network/resolver.rs \
	src/permission.rs \
	src/private_file.rs \
	src/rest.rs \
	src/rest/rate_limit.rs \
	src/rest/http.rs \
	src/rest/path.rs \
	src/rest/debug.rs \
	src/runtime.rs \
	src/runtime/permissions.rs \
	src/runtime/mutations.rs \
	src/runtime/reads.rs \
	src/runtime/gateway_send.rs \
	src/snapshot.rs \
	src/snapshot/debug.rs \
	src/snapshot/cache.rs \
	src/snapshot/parse.rs \
	src/snapshot/member.rs \
	src/snapshot/authority.rs \
	src/snapshot/channel.rs \
	src/snapshot/archive.rs \
	src/snapshot/message.rs \
	src/snapshot/revision.rs \
	src/snapshot/webhook.rs \
	src/validation.rs
PACKAGE_ALLOWED_FILES = \
	.cargo_vcs_info.json \
	Cargo.toml.orig \
	$(PACKAGE_REQUIRED_FILES)
PACKAGE_EXCLUDED_FILES = \
	.github/workflows/ci.yml \
	tests/config_samples.rs \
	tests/direct_messages.rs \
	tests/fortran_acceptance.rs \
	tests/fortran_builders.rs \
	tests/license_policy.rs \
	tests/rest_http_integration.rs \
	tests/runtime_rest_integration.rs \
	tests/source_policy.rs \
	tests/supervisor_lifecycle.rs

.PHONY: all fortran-sample fortran-extension-sample fortran-wrapper fortran-extension-wrapper require-dev-tests check-comments fmt-check whitespace-check test check dev-check acceptance staging-rehearsal staging-live-preflight staging-live-run staging-live-24h staging-audit-check config-check source-policy lint docs msrv-check license-check advisory-check package-list-check package publish-dry-run release-check publish-check ci-check clean

all: fortran-wrapper fortran-extension-wrapper

fortran-wrapper:
	mkdir -p $(FORTRAN_TARGET_DIR)
	$(FC) $(FFLAGS) -c fortran/dfor77_wrapper.f -o $(FORTRAN_WRAPPER_OBJ)

fortran-extension-wrapper: fortran-wrapper
	$(FC) $(FFLAGS) -c extension/dfor77_extension.f -o $(FORTRAN_EXTENSION_OBJ)

fortran-sample: fortran-wrapper
	$(FC) $(FFLAGS) examples/sample_moderation/sample_moderation.f $(FORTRAN_WRAPPER_OBJ) -o $(FORTRAN_SAMPLE_BIN)

fortran-extension-sample: fortran-wrapper fortran-extension-wrapper
	$(FC) $(FFLAGS) -Iinclude examples/sample_moderation/sample_moderation_extension.f $(FORTRAN_WRAPPER_OBJ) $(FORTRAN_EXTENSION_OBJ) -o $(FORTRAN_EXTENSION_BIN)

require-dev-tests:
	@test -f tests/fortran_acceptance.rs || { \
		echo "development tests are not included in this package"; \
		exit 1; \
	}

check-comments:
	@if command -v rg >/dev/null 2>&1; then \
		! rg -n "^\s*//|^\s*/\*|^\s*///|^\s*//!" $(RUST_POLICY_DIRS) --glob '!target/**'; \
	else \
		find $(RUST_POLICY_DIRS) -type f -print | xargs awk '/^[[:space:]]*\/\// || /^[[:space:]]*\/\*/ { print FILENAME ":" FNR ":" $$0; found=1 } END { exit found ? 1 : 0 }'; \
	fi
	@if command -v rg >/dev/null 2>&1; then \
		! rg -n "^[Cc]\b|^\s*!" extension fortran include --glob '!target/**'; \
	else \
		find extension fortran include -type f -print | xargs awk '/^[Cc]($$|[[:space:]])/ || /^[[:space:]]*!/ { print FILENAME ":" FNR ":" $$0; found=1 } END { exit found ? 1 : 0 }'; \
	fi

fmt-check:
	cargo fmt -- --check

whitespace-check:
	@if test -d .git; then git diff --check; fi
	@if command -v rg >/dev/null 2>&1; then \
		! rg -n "[[:blank:]]+$$" $(TEXT_POLICY_PATHS) --glob '!target/**'; \
	else \
		find $(TEXT_POLICY_PATHS) -type f -print | xargs awk '/[[:blank:]]+$$/ { print FILENAME ":" FNR ":" $$0; found=1 } END { exit found ? 1 : 0 }'; \
	fi

test: check-comments fortran-wrapper fortran-extension-wrapper
	@if command -v cargo >/dev/null 2>&1; then cargo build; cargo test; else echo "cargo not found; skipping Rust tests"; fi
	$(FC) $(FFLAGS) -c fortran/dfor77_wrapper.f -o /tmp/dfor77_wrapper.o

check: test

dev-check: require-dev-tests test acceptance

acceptance: require-dev-tests fortran-sample
	cargo build
	cargo test --test fortran_acceptance sample_fortran_bot_

staging-rehearsal: require-dev-tests fortran-sample
	env DISCORD_BOT_TOKEN=sample-token DFOR77_AUDIT_HASH_KEY=sample-audit-key cargo run -- --validate-config --config $(STAGING_CONFIG) --audit-log $(STAGING_AUDIT_LOG) --gateway-session-dir $(STAGING_SESSION_DIR) --hot-restart-file $(STAGING_RESTART_FILE)
	cargo test --test fortran_acceptance sample_fortran_bot_

staging-live-preflight: fortran-sample
	@case "$${DISCORD_BOT_TOKEN:-}" in ""|sample-token|local-mock-token|"...") echo "DISCORD_BOT_TOKEN must be set to a real staging bot token"; exit 1;; esac
	@case "$${DFOR77_AUDIT_HASH_KEY:-}" in ""|sample-audit-key|"...") echo "DFOR77_AUDIT_HASH_KEY must be set to a real staging audit hash key"; exit 1;; esac
	cargo run -- --validate-config --config $(STAGING_CONFIG) --audit-log $(STAGING_AUDIT_LOG) --gateway-session-dir $(STAGING_SESSION_DIR) --hot-restart-file $(STAGING_RESTART_FILE)
	@test -x "$(STAGING_BOT)"

staging-live-run: staging-live-preflight
	cargo run -- --config $(STAGING_CONFIG) --audit-log $(STAGING_AUDIT_LOG) --gateway-session-dir $(STAGING_SESSION_DIR) --hot-restart-file $(STAGING_RESTART_FILE) --bot $(STAGING_BOT)

staging-live-24h: staging-live-preflight
	@set -eu; \
	case "$(STAGING_LIVE_SECONDS)" in ""|0|*[!0-9]*) echo "STAGING_LIVE_SECONDS must be a positive integer"; exit 1;; esac; \
	cargo run -- --config $(STAGING_CONFIG) --audit-log $(STAGING_AUDIT_LOG) --gateway-session-dir $(STAGING_SESSION_DIR) --hot-restart-file $(STAGING_RESTART_FILE) --bot $(STAGING_BOT) & \
	pid=$$!; \
	status=0; \
	start=$$(date +%s); \
	end=$$((start + $(STAGING_LIVE_SECONDS))); \
	trap 'kill $$pid >/dev/null 2>&1 || true; wait $$pid >/dev/null 2>&1 || true' INT TERM EXIT; \
	while [ $$(date +%s) -lt $$end ]; do \
		if ! kill -0 $$pid >/dev/null 2>&1; then \
			wait $$pid || status=$$?; \
			echo "staging live run exited before $(STAGING_LIVE_SECONDS) seconds"; \
			if [ "$$status" -eq 0 ]; then status=1; fi; \
			exit $$status; \
		fi; \
		sleep 60; \
	done; \
	kill $$pid >/dev/null 2>&1 || true; \
	wait $$pid || status=$$?; \
	trap - INT TERM EXIT; \
	case "$$status" in 0|143) ;; *) exit "$$status";; esac
	$(MAKE) staging-audit-check

staging-audit-check:
	cargo run -- --validate-audit-log $(STAGING_AUDIT_LOG) $(STAGING_AUDIT_ACTIONS)

config-check: require-dev-tests
	cargo test --test config_samples

source-policy: require-dev-tests
	cargo test --test source_policy

lint:
	cargo clippy --all-targets -- -D warnings

docs:
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

msrv-check:
	cargo +1.85.0 check --all-targets

license-check: require-dev-tests
	DFOR77_LICENSE_CHECK=1 cargo test --test license_policy -- --nocapture

advisory-check:
	cargo audit --deny warnings

package-list-check:
	mkdir -p target
	cargo package --allow-dirty --offline --target-dir $(PACKAGE_TARGET_DIR) --list > target/package-files.txt
	@for path in $(PACKAGE_REQUIRED_FILES); do \
		if ! grep -Fxq "$$path" target/package-files.txt; then \
			echo "missing package file: $$path"; \
			exit 1; \
		fi; \
	done
	@for path in $(PACKAGE_EXCLUDED_FILES); do \
		if grep -Fxq "$$path" target/package-files.txt; then \
			echo "excluded package file present: $$path"; \
			exit 1; \
		fi; \
	done
	@while IFS= read -r path; do \
		found=0; \
		for allowed in $(PACKAGE_ALLOWED_FILES); do \
			if [ "$$path" = "$$allowed" ]; then \
				found=1; \
				break; \
			fi; \
		done; \
		if [ "$$found" -eq 0 ]; then \
			echo "unexpected package file: $$path"; \
			exit 1; \
		fi; \
	done < target/package-files.txt

package:
	cargo package --allow-dirty --offline --target-dir $(PACKAGE_TARGET_DIR)

publish-dry-run:
	cargo publish --dry-run --allow-dirty --target-dir $(PUBLISH_TARGET_DIR)

release-check: require-dev-tests fmt-check whitespace-check dev-check staging-rehearsal config-check source-policy lint docs license-check advisory-check package-list-check package

publish-check: release-check publish-dry-run

ci-check: msrv-check publish-check

clean:
	rm -rf $(FORTRAN_TARGET_DIR) $(PACKAGE_TARGET_DIR) $(PUBLISH_TARGET_DIR)
	rm -f dfor77_wrapper.o sample_moderation includebot sample_moderation_extension /tmp/dfor77_wrapper.o
