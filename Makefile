.PHONY: all cli menubar install install-cli install-menubar test clean uninstall check-cargo

APP        := menubar/build/ClaudePrimer.app
BIN_DIR    := $(HOME)/.local/bin
APPS_DIR   := $(HOME)/Applications
# ~/.local/bin is a stable path. The launchd job stores the binary's absolute path
# permanently, so it must never point into cli/target/.
CLI_BIN    := $(BIN_DIR)/claude-primer

# Find cargo without depending on the caller's PATH. Homebrew's rustup keeps its shims
# in an opt/ directory it does not add to PATH, so `make` failed with "command not
# found" for anyone who had not edited their shell profile — including on this machine.
CARGO := $(shell command -v cargo 2>/dev/null \
                 || ls /opt/homebrew/opt/rustup/bin/cargo 2>/dev/null \
                 || ls /usr/local/opt/rustup/bin/cargo 2>/dev/null \
                 || ls $(HOME)/.cargo/bin/cargo 2>/dev/null)

# Putting the directory on PATH, rather than calling cargo by absolute path: rustup's
# shims locate rustc *by name*, so an absolute cargo still dies with
# "could not execute process `rustc -vV`".
CARGO_PATH := $(dir $(CARGO)):$(PATH)

# Fail with something actionable rather than a bare "command not found".
check-cargo:
ifeq ($(strip $(CARGO)),)
	@echo "error: cargo not found."; \
	 echo; \
	 echo "  Install Rust:   brew install rustup && rustup-init -y"; \
	 echo "  Already have it? Add it to your PATH:"; \
	 echo "    echo 'export PATH=\"/opt/homebrew/opt/rustup/bin:\$$PATH\"' >> ~/.zshrc"; \
	 exit 1
endif

all: cli menubar

cli: check-cargo
	cd cli && PATH="$(CARGO_PATH)" cargo build --release

test: check-cargo
	cd cli && PATH="$(CARGO_PATH)" cargo test --release

# swiftc against the Command Line Tools SDK — no Xcode project, no full Xcode install.
# The bundle is assembled by hand and ad-hoc signed, which is sufficient for a locally
# built personal app (no quarantine flag, so Gatekeeper does not prompt).
menubar:
	rm -rf $(APP)
	mkdir -p $(APP)/Contents/MacOS
	swiftc -O -o $(APP)/Contents/MacOS/ClaudePrimer menubar/main.swift
	cp menubar/Info.plist $(APP)/Contents/Info.plist
	codesign --force --sign - $(APP)
	@echo "built $(APP)"

install: install-cli install-menubar
	@echo
	@echo "Next:"
	@echo "  claude setup-token       # then paste it into:"
	@echo "  claude-primer install    # schedules the primes"
	@echo "  claude-primer menubar enable"

install-cli: cli
	mkdir -p $(BIN_DIR)
	cp cli/target/release/claude-primer $(CLI_BIN)
	@echo "installed $(CLI_BIN)"

install-menubar: menubar
	mkdir -p $(APPS_DIR)
	rm -rf $(APPS_DIR)/ClaudePrimer.app
	cp -R $(APP) $(APPS_DIR)/
	@echo "installed $(APPS_DIR)/ClaudePrimer.app"
# Replacing the bundle does not touch the running process — macOS keeps executing the
# already-loaded code, so a reinstall silently appeared to do nothing. Restart it here.
# Only when it was already running: installing should not start an app you had quit.
	@if pgrep -f 'ClaudePrimer.app/Contents/MacOS/ClaudePrimer' >/dev/null 2>&1; then \
	   pkill -f 'ClaudePrimer.app/Contents/MacOS/ClaudePrimer' 2>/dev/null || true; \
	   sleep 1; \
	   if [ -f "$(HOME)/Library/LaunchAgents/com.claude-primer.menubar.plist" ]; then \
	     $(CLI_BIN) menubar enable >/dev/null 2>&1 || open -a $(APPS_DIR)/ClaudePrimer.app; \
	   else \
	     open -a $(APPS_DIR)/ClaudePrimer.app; \
	   fi; \
	   echo "restarted the menu bar app"; \
	 fi

# Stops the scheduled primes and the menu bar app. Leaves config and logs alone.
uninstall:
	-$(CLI_BIN) menubar disable
	-$(CLI_BIN) uninstall
	rm -rf $(APPS_DIR)/ClaudePrimer.app

clean:
	cd cli && PATH="$(CARGO_PATH)" cargo clean
	rm -rf menubar/build
