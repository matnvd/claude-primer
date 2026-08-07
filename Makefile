.PHONY: all cli menubar install install-cli install-menubar test clean uninstall

APP        := menubar/build/ClaudePrimer.app
BIN_DIR    := $(HOME)/.local/bin
APPS_DIR   := $(HOME)/Applications
# ~/.local/bin is a stable path. The launchd job stores the binary's absolute path
# permanently, so it must never point into cli/target/.
CLI_BIN    := $(BIN_DIR)/claude-primer

all: cli menubar

cli:
	cd cli && cargo build --release

test:
	cd cli && cargo test --release

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

# Stops the scheduled primes and the menu bar app. Leaves config and logs alone.
uninstall:
	-$(CLI_BIN) menubar disable
	-$(CLI_BIN) uninstall
	rm -rf $(APPS_DIR)/ClaudePrimer.app

clean:
	cd cli && cargo clean
	rm -rf menubar/build
