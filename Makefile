# abtop — build, test and install.
#
# Wraps cargo for the common workflows: a release build installed into a user
# prefix, and the local, CI and cross-platform checks.
#
# Written for GNU Make 3.81, the version macOS ships, so every recipe line is
# its own shell and there is no .ONESHELL.

SHELL := /bin/bash

CARGO := cargo

# Shared Rust cache, kept outside the checkout so it survives clean builds.
CARGO_TARGET := $(HOME)/.rust/target

# Install prefix. Override on the command line: make install PREFIX=/opt/abtop
PREFIX := $(HOME)/.local
BINDIR := $(PREFIX)/bin

BIN       := abtop
BUILT_BIN := $(CARGO_TARGET)/release/$(BIN)
MSRV      := $(shell sed -n 's/^rust-version = "\(.*\)"/\1/p' Cargo.toml)

# Extra arguments for `make run`, e.g. make run ARGS="--theme dracula"
ARGS :=

# `make cross-check` runs in this image, so it needs no host rustup targets.
DOCKER_IMAGE   := rust:latest
WINDOWS_TARGET := x86_64-pc-windows-gnu

CARGO_ENV := env CARGO_TARGET_DIR="$(CARGO_TARGET)"

# No target produces a file of its own name. Without this, a stray file named
# `build` or `install` in the checkout would make GNU Make skip the recipe.
.PHONY: help prereqs clean build rebuild install uninstall \
	test lint fmt fmt-check check ci cross-check run once demo

.DEFAULT_GOAL := help

help: ## Show this help
	@printf 'abtop — build, test and install\n\n'
	@printf 'Targets:\n'
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  %-12s %s\n", $$1, $$2}'
	@printf '\nFrom-scratch build and install:\n'
	@printf '  1. make prereqs   verify the toolchain\n'
	@printf '  2. make clean     drop abtop build artifacts\n'
	@printf '  3. make build     compile the release binary\n'
	@printf '  4. make install   copy it into $(BINDIR)\n'
	@printf '\n  make rebuild      = clean + build\n'
	@printf '  make check        = lint + test, before pushing\n'
	@printf '\nSettings (override on the command line):\n'
	@printf '  PREFIX        %s\n' '$(PREFIX)'
	@printf '  BINDIR        %s\n' '$(BINDIR)'
	@printf '  CARGO_TARGET  %s\n' '$(CARGO_TARGET)'
	@printf '  DOCKER_IMAGE  %s\n' '$(DOCKER_IMAGE)'
	@printf '\nBack up the binary you are replacing before installing over it:\n'
	@printf '  cp %s/%s %s/%s.backup\n' '$(BINDIR)' '$(BIN)' '$(BINDIR)' '$(BIN)'

prereqs: ## Verify the toolchain this build needs
	@printf 'checking prerequisites\n'
	@test -f Cargo.toml -a -f src/main.rs \
		|| { printf '  FAIL  run make from the repository root\n'; exit 1; }
	@printf '  ok    repository root\n'
	@command -v $(CARGO) >/dev/null \
		|| { printf '  FAIL  cargo not on PATH\n'; exit 1; }
	@printf '  ok    cargo %s\n' "$$($(CARGO) --version | awk '{print $$2}')"
	@have=$$(rustc --version | awk '{print $$2}'); \
	low=$$(printf '%s\n%s\n' '$(MSRV)' "$$have" | sort -t. -k1,1n -k2,2n -k3,3n | head -1); \
	test "$$low" = '$(MSRV)' \
		|| { printf '  FAIL  rustc %s is older than rust-version %s\n' "$$have" '$(MSRV)'; exit 1; }; \
	printf '  ok    rustc %s (rust-version %s)\n' "$$have" '$(MSRV)'
	@mkdir -p $(CARGO_TARGET) 2>/dev/null \
		&& test -w $(CARGO_TARGET) \
		|| { printf '  FAIL  %s is not writable\n' '$(CARGO_TARGET)'; exit 1; }
	@printf '  ok    cargo target dir writable\n'
	@test -d $(BINDIR) -a -w $(BINDIR) \
		&& printf '  ok    install dir writable\n' \
		|| printf '  note  %s is missing or not writable; make install will fail\n' '$(BINDIR)'
	@command -v git >/dev/null \
		|| printf '  note  git not on PATH; the projects panel shows no branch or changes\n'
	@command -v sqlite3 >/dev/null \
		|| printf '  note  sqlite3 not on PATH; OpenCode sessions will not appear\n'
	@printf 'prerequisites satisfied\n'

clean: ## Remove abtop's build artifacts; cached dependencies stay
	@printf 'removing build artifacts\n'
	$(CARGO_ENV) $(CARGO) clean -p $(BIN)
	@printf 'clean\n'

build: prereqs ## Compile the release binary
	$(CARGO_ENV) $(CARGO) build --release --locked
	@test -x $(BUILT_BIN) \
		|| { printf 'FAIL  %s was not produced\n' '$(BUILT_BIN)'; exit 1; }
	@printf 'built %s\n' '$(BUILT_BIN)'
	@printf '  %s\n' "$$($(BUILT_BIN) --version)"

# Sequential sub-makes, so `make -j rebuild` cannot clean during the build.
rebuild: ## Clean, then build
	@$(MAKE) --no-print-directory clean
	@$(MAKE) --no-print-directory build

install: ## Install the built binary into BINDIR (shown under Settings)
	@test -x $(BUILT_BIN) \
		|| { printf 'FAIL  %s does not exist; run make build first\n' '$(BUILT_BIN)'; exit 1; }
	@test -d $(BINDIR) -a -w $(BINDIR) \
		|| { printf 'FAIL  %s does not exist or is not writable\n' '$(BINDIR)'; exit 1; }
	@# `install` replaces the inode, so running abtop processes keep the old
	@# image until restarted. Writing through the inode instead, with cp, can
	@# leave a signed binary that macOS kills on its next launch.
	@count=$$(pgrep -u "$$(id -u)" -x $(BIN) 2>/dev/null | wc -l | tr -d ' '); \
	if [ "$$count" != "0" ]; then \
		printf 'note  %s %s process(es) keep the previous binary until restarted\n' "$$count" '$(BIN)'; \
	fi
	install -m 755 $(BUILT_BIN) $(BINDIR)/$(BIN)
	@printf 'installed %s\n' '$(BINDIR)/$(BIN)'
	@printf '  version   %s\n' "$$($(BINDIR)/$(BIN) --version)"
	@sum=$$(shasum -a 256 $(BINDIR)/$(BIN) 2>/dev/null || sha256sum $(BINDIR)/$(BIN)); \
	printf '  sha256    %s\n' "$${sum%% *}"
	@if [ "$$(uname -s)" = Darwin ]; then \
		codesign -v $(BINDIR)/$(BIN) 2>/dev/null \
			&& printf '  signature ok\n' \
			|| printf '  signature could not be verified\n'; \
	fi
	@# Another abtop earlier on PATH (Homebrew, cargo install) would win.
	@first=$$(command -v $(BIN)); \
	if [ "$$first" != '$(BINDIR)/$(BIN)' ]; then \
		printf 'note  %s on PATH resolves to %s, which shadows this install\n' '$(BIN)' "$${first:-nothing}"; \
	fi; \
	type -ap $(BIN) | awk '!seen[$$0]++' | grep -vxF '$(BINDIR)/$(BIN)' | while read -r other; do \
		printf 'note  another copy is on PATH: %s (%s)\n' "$$other" "$$("$$other" --version 2>/dev/null)"; \
	done

uninstall: ## Remove the binary from BINDIR
	@if [ -e $(BINDIR)/$(BIN) ]; then \
		rm -f $(BINDIR)/$(BIN) && printf 'removed %s\n' '$(BINDIR)/$(BIN)'; \
	else \
		printf 'nothing installed at %s\n' '$(BINDIR)/$(BIN)'; \
	fi

test: ## Run the test suite
	$(CARGO_ENV) $(CARGO) test --locked

lint: ## Run clippy on every target, warnings denied
	$(CARGO_ENV) $(CARGO) clippy --all-targets --locked -- -D warnings

fmt: ## Format the code with rustfmt
	$(CARGO) fmt

# Not part of `check` until the tree is rustfmt-clean.
fmt-check: ## Check formatting without changing files
	$(CARGO) fmt --check

check: lint test ## Lint and test, before pushing

ci: ## Run the steps of .github/workflows/ci.yml, unchanged
	$(CARGO_ENV) $(CARGO) clippy -- -D warnings
	$(CARGO_ENV) $(CARGO) test
	$(CARGO_ENV) $(CARGO) build --release

# The checkout is mounted read-only and copied in; named volumes cache the
# registry and the container's build between runs.
cross-check: ## Lint and test on Linux and lint the Windows target, in Docker
	@command -v docker >/dev/null && docker info >/dev/null 2>&1 \
		|| { printf 'FAIL  docker is not running or not on PATH\n'; exit 1; }
	docker run --rm \
		-v "$(CURDIR)":/src:ro \
		-v abtop-cargo-registry:/usr/local/cargo/registry \
		-v abtop-cross-target:/work/target \
		$(DOCKER_IMAGE) bash -euc '\
			rustup component add clippy >/dev/null 2>&1; \
			rustup target add $(WINDOWS_TARGET) >/dev/null 2>&1; \
			tar -C /src --exclude=./target --exclude=./.jj --exclude=./.git -cf - . | tar -C /work -xf -; \
			cd /work; \
			echo "== linux: clippy"; cargo clippy --all-targets --locked -- -D warnings; \
			echo "== linux: test"; cargo test --locked; \
			echo "== windows: clippy"; cargo clippy --target $(WINDOWS_TARGET) --all-targets --locked -- -D warnings'

run: ## Run the TUI from source (pass flags with ARGS="...")
	$(CARGO_ENV) $(CARGO) run --release --locked -- $(ARGS)

once: ## Print one snapshot of live sessions and exit
	$(CARGO_ENV) $(CARGO) run --release --locked -- --once

demo: ## Print the demo snapshot (no live data)
	$(CARGO_ENV) $(CARGO) run --release --locked -- --demo --once
