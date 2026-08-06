# PSXcel -- a gamepad-driven spreadsheet for the PlayStation 1, built on the
# PSoXide Rust SDK (pinned under .psoxide). Mirrors the oot-psx /
# zelda3-psx build flow.

ROOT     := $(CURDIR)
GAME     := $(ROOT)/game
PSOXIDE  := $(ROOT)/.psoxide
MKISOPSX := $(PSOXIDE)/tools/mkisopsx
TARGET   := mipsel-sony-psx
DIST     := $(ROOT)/dist
EXE      := $(GAME)/target/$(TARGET)/release/psxcel.exe
BIN      := $(DIST)/psxcel.bin
CUE      := $(DIST)/psxcel.cue

GAMES_DIR ?= $(HOME)/Downloads/ps1 games
GAME_NAME ?= PSXcel

.PHONY: help build test disc render install release clean psoxide

help:
	@echo "PSXcel targets:"
	@echo "  make build   - build the PSX-EXE -> $(EXE)"
	@echo "  make test    - run the host-side unit tests (formula engine, formatting)"
	@echo "  make disc    - build + pack a burnable .bin/.cue into dist/"
	@echo "  make render  - disc + headless emulator frame dump (dist/frame.png)"
	@echo "  make install - disc + copy into the PSoXide game library ($(GAMES_DIR))"
	@echo "  make clean   - remove build output"

# Which PSoXide this is built against. Cargo owns the pin (psoxide-pin/), and
# psoxide-link copies the resolved checkout to .psoxide so the path
# dependencies and the linker script resolve. PSOXIDE_FROM=/path/to/tree
# overrides it, which is how the demo disc puts every program on one SDK.
PSOXIDE_FROM ?=
psoxide:
	@if [ -n "$(PSOXIDE_FROM)" ]; then \
		cargo run -q --manifest-path $(PSOXIDE_FROM)/tools/psoxide-link/Cargo.toml -- \
			--from "$(PSOXIDE_FROM)" --into $(PSOXIDE); \
	else \
		cargo run -q --manifest-path $(ROOT)/psoxide-pin/Cargo.toml -- $(PSOXIDE); \
	fi

build: psoxide
	cd $(GAME) && cargo build --release

# Host-side unit tests (the sheet.rs evaluator/formatter suite). Run from the
# repo root so game/.cargo/config.toml does not apply: its build-std core
# collides with the prebuilt host std (duplicate lang items), and its default
# target is the PSX. --target <host> keeps the artifacts in their own subdir.
# Single-threaded: recalc's snapshot scratch lives in statics (fine on the
# single-threaded PS1, racy under libtest's parallel runner).
test:
	cargo test --manifest-path $(GAME)/Cargo.toml \
		--target $$(rustc -vV | sed -n 's/host: //p') -- --test-threads=1

disc: build
	@mkdir -p $(DIST)
	cd $(MKISOPSX) && cargo run --release -- \
		--exe "$(EXE)" --out "$(BIN)" --volume PSXCEL

# Headless render of the boot frame, so a change can be eyeballed before burning.
render: disc
	cd $(PSOXIDE)/emu && cargo run -p frontend --release -- launch \
		--path "$(CUE)" --dump-hw "$(DIST)/frame.ppm"
	@sips -s format png "$(DIST)/frame.ppm" --out "$(DIST)/frame.png" >/dev/null 2>&1 \
		&& echo "wrote $(DIST)/frame.png" || echo "wrote $(DIST)/frame.ppm"

# Match the PSoXide game-library convention: a "Title (slug)" folder holding a
# matching <name>.bin / <name>.cue (the cue's FILE line references the bin name).
install: disc
	@mkdir -p "$(GAMES_DIR)/$(GAME_NAME)"
	cp "$(BIN)" "$(GAMES_DIR)/$(GAME_NAME)/$(GAME_NAME).bin"
	printf 'FILE "%s.bin" BINARY\n  TRACK 01 MODE2/2352\n    INDEX 01 00:00:00\n' \
		"$(GAME_NAME)" > "$(GAMES_DIR)/$(GAME_NAME)/$(GAME_NAME).cue"
	@echo "installed -> $(GAMES_DIR)/$(GAME_NAME)/"

clean:
	cd $(GAME) && cargo clean
	rm -rf $(DIST)

# Stage the itch.io payload. CI (deploy.yml) pushes release/ via butler
# whenever it changes on main, versioned from the VERSION file.
release: disc
	@mkdir -p $(ROOT)/release
	cp "$(BIN)" "$(CUE)" $(ROOT)/release/
	@echo "RELEASE -> $(ROOT)/release (commit + push to deploy)"
