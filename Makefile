SHELL := /bin/bash

# This repository is the software. It ships no site of its own: authored
# content lives in its own repository and versions separately. Point SITE_DIR
# at one to build or serve it locally.
SITE_DIR ?=
SITE_BUILD := target/site

.PHONY: ci ci-preflight ci-publish-dryrun install install-check build-release ci-fmt ci-clippy ci-boundaries ci-tests ci-tools ci-docs ci-deps ci-full ci-fuzz-smoke ci-miri ci-miri-core ci-miri-compositor ci-asan test build check allocation-audit effects clean site-builder-check site serve client dev-server dev-client fuzz fuzz-campaign fuzz-check fuzz-scanner fuzz-parser fuzz-pipeline fuzz-protocol fuzz-uri fuzz-protocol-state fuzz-viewer-state

# ─── Local verification gate ─────────────────────────────────
#
# All verification runs locally on macOS. There is no hosted CI: the
# GitHub Actions workflow was removed because the account does not pay
# for Actions minutes, so every run sat queued and no gate was ever
# actually enforced. A local gate that runs is worth more than a hosted
# one that does not.
#
#   make test      fast inner loop — use while working
#   make ci        the full gate — run before every commit
#   make ci-full   ci plus Miri, ASan, and a fuzz smoke run (slow)
#
# Host triple is resolved rather than hardcoded so ASan works on both
# Apple Silicon and Intel macs.
HOST_TRIPLE := $(shell rustc -vV | sed -n 's/^host: //p')
MISE := eval "$$(mise activate bash)" &&

# The stable toolchain is pinned in rust-toolchain.toml. The nightly cannot be
# pinned there, because these targets invoke it explicitly, so it is pinned
# here. Dated rather than floating: `nightly` moves under you, which would make
# the Miri and ASan gates describe a compiler nobody can reproduce.
NIGHTLY := nightly-2026-03-13

# Everything the gate shells out to that cargo does not ship. Checked by
# ci-preflight, which names the install command for whatever is missing.
CARGO_TOOLS := nextest audit deny vet fuzz

ci: ci-preflight ci-fmt ci-clippy ci-boundaries ci-tests ci-tools ci-docs ci-deps install-check
	@echo "── ci: all gates passed ──"

ci-full: ci ci-fuzz-smoke ci-miri ci-asan
	@echo "── ci-full: all gates passed, including Miri/ASan/fuzz ──"

# Everything `make ci-full` needs that is not cargo itself. Fails naming what
# is missing *and* the command that installs it, because "a fresh machine
# cannot run the gate" was true for months and nothing said so.
#
# This is why the pins exist: without them a fresh clone silently runs a
# different compiler than the one the gate table describes.
ci-preflight:
	@echo "── preflight ──"
	@missing=0; \
	pinned=$$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml); \
	mised=$$(sed -n 's/^rust = "\(.*\)"/\1/p' .mise.toml); \
	if [ "$$pinned" != "$$mised" ]; then \
		echo "  toolchain pins disagree: rust-toolchain.toml=$$pinned .mise.toml=$$mised"; \
		missing=1; \
	fi; \
	actual=$$(rustc --version | awk '{print $$2}'); \
	if [ "$$actual" != "$$pinned" ]; then \
		echo "  rustc is $$actual, pinned is $$pinned"; \
		echo "    rustup toolchain install $$pinned"; \
		missing=1; \
	fi; \
	if ! rustup run $(NIGHTLY) rustc --version >/dev/null 2>&1; then \
		echo "  missing nightly $(NIGHTLY)"; \
		echo "    rustup toolchain install $(NIGHTLY)"; \
		missing=1; \
	else \
		for component in miri rust-src; do \
			if ! rustup component list --toolchain $(NIGHTLY) --installed 2>/dev/null \
				| grep -q "^$$component"; then \
				echo "  missing component $$component on $(NIGHTLY)"; \
				echo "    rustup component add $$component --toolchain $(NIGHTLY)"; \
				missing=1; \
			fi; \
		done; \
	fi; \
	if ! rustup target list --installed 2>/dev/null | grep -q '^wasm32-unknown-unknown$$'; then \
		echo "  missing target wasm32-unknown-unknown (make effects)"; \
		echo "    rustup target add wasm32-unknown-unknown"; \
		missing=1; \
	fi; \
	for tool in $(CARGO_TOOLS); do \
		if ! cargo "$$tool" --version >/dev/null 2>&1; then \
			echo "  missing cargo-$$tool"; \
			echo "    cargo install cargo-$$tool --locked"; \
			missing=1; \
		fi; \
	done; \
	if [ "$$missing" -ne 0 ]; then \
		echo "── preflight failed: install the above, then re-run ──"; \
		exit 1; \
	fi; \
	echo "  toolchain $$pinned, nightly $(NIGHTLY), $(words $(CARGO_TOOLS)) cargo tools, all present"

ci-fmt:
	@echo "── formatting ──"
	$(MISE) cargo fmt --all --check

ci-clippy:
	@echo "── clippy (warnings denied) ──"
	$(MISE) cargo clippy --workspace --all-targets --all-features -- -D warnings

# Proves the crate graph stays partitioned: no root facade crate, core
# depends on neither client nor server, and the production server cannot
# reach the unsupported social example.
ci-boundaries:
	@echo "── feature boundaries ──"
	@test ! -e src/lib.rs
	$(MISE) cargo check -p dustnet-core --all-targets
	$(MISE) cargo check -p dustnet-client --all-targets
	$(MISE) cargo check -p dustnet-server --all-targets
	@$(MISE) ! cargo tree -p dustnet-server | grep -q unsupported-social
	@$(MISE) ! cargo tree -p dustnet-core | grep -Eq 'dustnet-(client|server)'

ci-tests: fuzz-check
	@echo "── workspace tests ──"
	$(MISE) cargo nextest run --workspace --all-features -j 4

# The excluded helper crates: neither is in the workspace, so a plain
# `cargo test` never compiles them and they rot silently.
ci-tools: site-builder-check allocation-audit
	@echo "── excluded tool crates ──"
	$(MISE) cargo clippy --manifest-path tools/prerender-figlet/Cargo.toml --all-targets -- -D warnings
	$(MISE) cargo test --manifest-path tools/allocation-audit/Cargo.toml
	$(MISE) cargo clippy --manifest-path tools/allocation-audit/Cargo.toml --all-targets -- -D warnings

# ─── Installation ────────────────────────────────────────────
#
# One install path, so "installable" means something checkable rather than
# "copy a binary somewhere". Homebrew, nix, deb and cargo-dist are deferred
# with their exposures recorded in docs/guides/production-support.md; crates.io
# is the other path, and it needs no target here.
#
# Completions and man pages are generated by the binaries themselves from the
# same `Command` the parser is built from. Anything maintained separately
# drifts the moment a flag is renamed.
PREFIX ?= /usr/local

install: build-release
	@echo "── install to $(PREFIX) ──"
	@install -d "$(PREFIX)/bin" "$(PREFIX)/share/man/man1" \
		"$(PREFIX)/share/bash-completion/completions" \
		"$(PREFIX)/share/zsh/site-functions" \
		"$(PREFIX)/share/fish/vendor_completions.d"
	@for binary in dustnet dustnetd; do \
		install -m 755 "target/release/$$binary" "$(PREFIX)/bin/$$binary"; \
		"target/release/$$binary" manpage > "$(PREFIX)/share/man/man1/$$binary.1"; \
		"target/release/$$binary" completions bash \
			> "$(PREFIX)/share/bash-completion/completions/$$binary"; \
		"target/release/$$binary" completions zsh \
			> "$(PREFIX)/share/zsh/site-functions/_$$binary"; \
		"target/release/$$binary" completions fish \
			> "$(PREFIX)/share/fish/vendor_completions.d/$$binary.fish"; \
	done
	@echo "── installed ──"

build-release:
	$(MISE) cargo build --release --locked --all-features --bins

# What `make install` places, asserted rather than described. A packaging
# change that drops a file is otherwise invisible until someone notices a
# missing man page.
install-check: build-release
	@echo "── install file list ──"
	@rm -rf target/install-check
	@$(MAKE) --no-print-directory install PREFIX="$$(pwd)/target/install-check" > /dev/null
	@cd target/install-check && find . -type f | LC_ALL=C sort > ../install-check.txt
	@diff -u verification/install-manifest.txt target/install-check.txt \
		|| { echo "  installed file list differs from verification/install-manifest.txt"; exit 1; }
	@echo "── install file list matches verification/install-manifest.txt ──"

# Publishability, checked before a release depends on it. crates.io is the
# only install path that works on both claimed platforms with no build
# infrastructure, and `publish = false` foreclosed it for months without
# anything saying so.
#
# `--workspace` rather than five separate runs: a per-crate dry run cannot
# resolve a path dependency that is not on crates.io yet, so it fails on
# `dustnet-client` for a reason that has nothing to do with publishability.
# The workspace form stages each package into a temporary registry in
# dependency order and verifies against it.
#
# No `--allow-dirty`, deliberately: a release is cut from a clean tree, so the
# check runs under the same condition the release does.
#
# Not part of `ci`: it packages and rebuilds five crates. Run it before a
# release, and whenever package metadata changes.
ci-publish-dryrun:
	@echo "── publish dry run ──"
	$(MISE) cargo publish --dry-run --locked --workspace
	@echo "── publish dry run: all five package cleanly ──"

ci-docs:
	@echo "── doctests, docs, locked release build ──"
	$(MISE) cargo test --doc --workspace --all-features
	$(MISE) cargo doc --workspace --all-features --no-deps
	$(MISE) cargo build --release --locked --all-features --bins

ci-deps:
	@echo "── advisories, licences, supply chain ──"
	$(MISE) cargo audit
	$(MISE) cargo deny check
	$(MISE) cargo vet --locked

# ─── Slow gates (were schedule-only in the old workflow) ─────

# Every target is given its committed seed directory as a read-only corpus, so
# a fresh clone starts from the same inputs this machine does. Without it the
# run depends on whatever fuzz/corpus/ has accumulated locally, which is
# gitignored and therefore unreproducible.
# A fuzz campaign, as an artifact rather than an instruction.
#
# "Run it for hours" is unrepeatable and so cannot be a closure condition. This
# runs every target for a declared number of seconds and appends one row per
# target to a tracked log, so what the gate claims about fuzzing is a number
# someone can read rather than a memory of an afternoon.
#
# The log records the host triple and toolchain because execution rate is
# meaningless without them: 200k execs on one machine is not 200k on another.
#
# The target is built before it is run, and a build failure exits without
# writing a row. Conflating the two is not a small thing: a `cargo fuzz run`
# that cannot compile exits non-zero exactly as a crashing one does, and a row
# reading `crashes=1` against a build error sends you looking for a bug in the
# code under test rather than in the code that failed to build.
#
# Note the corpus argument order. libFuzzer writes newly discovered units into
# the *first* corpus directory given, so `fuzz/corpus/<target>` (gitignored)
# comes first and `fuzz/seeds/<target>` (tracked) second. Reversing them
# quietly fills the tracked seed set with hundreds of uncurated inputs.
FUZZ_SECONDS ?= 300
# libFuzzer defaults to -rss_limit_mb=2048, which is below this harness's own
# steady-state footprint: ASan shadow memory, 46k coverage counters, PC tables
# and libFuzzer's per-input feature metadata reach ~1.5GB on the parser corpus
# before a single byte of AML is parsed. The default therefore aborts long runs
# with an OOM that says nothing about the code — measured at 752MB for 1000
# corpus files and 1479MB for 11132, sublinear in executions, so it is fixed
# overhead rather than a leak. A single input peaks at 39MB.
FUZZ_RSS_MB ?= 8192
FUZZ_LOG := verification/fuzz-campaign.tsv
FUZZ_TARGETS ?= fuzz_scanner fuzz_parser fuzz_pipeline fuzz_protocol \
                fuzz_protocol_state fuzz_viewer_state fuzz_uri

fuzz-campaign:
	@echo "── fuzz campaign ($(FUZZ_SECONDS)s per target) ──"
	@version=$$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1); \
	toolchain=$$(rustup run $(NIGHTLY) rustc --version | awk '{print $$1" "$$2}'); \
	test -s $(FUZZ_LOG) || printf 'version\ttarget\tseconds\texecutions\texecs_per_sec\tcrashes\thost\ttoolchain\n' > $(FUZZ_LOG); \
	for target in $(FUZZ_TARGETS); do \
		echo "   $$target"; \
		mkdir -p fuzz/corpus/$$target; \
		log=$$(mktemp); \
		$(MISE) cargo +$(NIGHTLY) fuzz build "$$target" > "$$log" 2>&1 || \
			{ tail -30 "$$log"; echo "  $$target failed to BUILD; this is not a crash"; exit 1; }; \
		set +e; \
		eval "$$(mise activate bash)" && cargo +$(NIGHTLY) fuzz run "$$target" \
			fuzz/corpus/$$target fuzz/seeds/$$target \
			-- -max_total_time=$(FUZZ_SECONDS) -rss_limit_mb=$(FUZZ_RSS_MB) \
			-print_final_stats=1 > "$$log" 2>&1; \
		status=$$?; \
		set -e; \
		execs=$$(sed -n 's/^stat::number_of_executed_units: *//p' "$$log" | tail -1); \
		rate=$$(sed -n 's/^stat::average_exec_per_sec: *//p' "$$log" | tail -1); \
		if [ "$$status" -eq 0 ]; then crashes=0; else crashes=1; tail -40 "$$log"; fi; \
		printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
			"$$version" "$$target" "$(FUZZ_SECONDS)" "$${execs:-unknown}" \
			"$${rate:-unknown}" "$$crashes" "$(HOST_TRIPLE)" "$$toolchain" \
			>> $(FUZZ_LOG); \
		rm -f "$$log"; \
		test "$$crashes" -eq 0 || { echo "  $$target crashed; record a finding and commit a seed"; exit 1; }; \
	done
	@echo "── fuzz campaign: $(FUZZ_LOG) updated ──"

ci-fuzz-smoke:
	@echo "── fuzz smoke (10s per target) ──"
	@for target in fuzz_scanner fuzz_parser fuzz_pipeline fuzz_protocol \
	               fuzz_protocol_state fuzz_viewer_state fuzz_uri; do \
		echo "   $$target"; \
		test -d fuzz/seeds/$$target || { echo "missing fuzz/seeds/$$target"; exit 1; }; \
		mkdir -p fuzz/corpus/$$target; \
		eval "$$(mise activate bash)" && cargo +$(NIGHTLY) fuzz run "$$target" \
			fuzz/corpus/$$target fuzz/seeds/$$target \
			-- -max_total_time=10 -rss_limit_mb=$(FUZZ_RSS_MB) || exit 1; \
	done

# Core conformance tests read checked-in fixtures, which Miri's isolation
# blocks unless explicitly disabled.
ci-miri: ci-miri-core ci-miri-compositor

ci-miri-core:
	@echo "── miri (dustnet-core) ──"
	$(MISE) MIRIFLAGS=-Zmiri-disable-isolation cargo +$(NIGHTLY) miri test -p dustnet-core

# ~23 minutes. Covers the compositor's synchronous surface — scene building,
# layout, the WASM host, animation construction — which is the project's
# largest hostile-input surface. Tests that build a tokio runtime are marked
# `#[cfg_attr(miri, ignore)]`: Miri emulates no foreign functions, so `kqueue`
# aborts the whole run rather than failing one test.
ci-miri-compositor:
	@echo "── miri (dustnet-client compositor) ──"
	$(MISE) MIRIFLAGS=-Zmiri-disable-isolation \
		cargo +$(NIGHTLY) miri test -p dustnet-client --lib -- compositor::

# The explicit --target keeps ASan off host proc-macros, which otherwise
# fail to load into the instrumented build.
ci-asan:
	@echo "── address sanitizer (dustnet-client) ──"
	$(MISE) RUSTFLAGS=-Zsanitizer=address RUSTDOCFLAGS=-Zsanitizer=address \
		cargo +$(NIGHTLY) test -p dustnet-client --lib -Zbuild-std --target $(HOST_TRIPLE)

test: fuzz-check site-builder-check allocation-audit
	eval "$$(mise activate bash)" && cargo test

allocation-audit:
	eval "$$(mise activate bash)" && cargo fmt --manifest-path tools/allocation-audit/Cargo.toml -- --check
	eval "$$(mise activate bash)" && cargo run --quiet --manifest-path tools/allocation-audit/Cargo.toml -- --check

site-builder-check:
	eval "$$(mise activate bash)" && cargo test --manifest-path tools/prerender-figlet/Cargo.toml

# Expand {{figlet:}} markers from the authored site into a clean served tree,
# then validate every emitted page. Fails loudly if SITE_DIR is not there,
# rather than serving an empty directory.
site:
	@test -n "$(SITE_DIR)" || { echo "set SITE_DIR to a site directory, e.g. make site SITE_DIR=../my-site"; exit 1; }
	@test -d "$(SITE_DIR)" || { echo "SITE_DIR not found: $(SITE_DIR)"; exit 1; }
	$(MISE) cargo run --quiet --manifest-path tools/prerender-figlet/Cargo.toml -- $(SITE_DIR) $(SITE_BUILD)
	@find $(SITE_BUILD) -name '*.aml' -type f -print0 | xargs -0 -n1 cargo run --quiet --bin dustnet -- check

# Type-check the fuzz targets (stable toolchain, no fuzzing, no nightly).
# The fuzz/ crate is excluded from the workspace, so `cargo test`/`cargo build`
# never compile it — a target referencing a renamed/removed API rots silently
# until someone runs a fuzz session. This guard fails fast on that drift.
fuzz-check:
	eval "$$(mise activate bash)" && cargo check --manifest-path fuzz/Cargo.toml

build:
	eval "$$(mise activate bash)" && cargo build

release:
	eval "$$(mise activate bash)" && cargo build --release

check:
	@eval "$$(mise activate bash)" && for f in tests/fixtures/aml/*.aml; do \
		echo -n "$$f: "; \
		cargo run --quiet --bin dustnet -- check "$$f" 2>&1 | tail -1; \
	done

# Build every WASM effect. Feature variants of one crate go to their own
# target directory so each variant has a stable, predictable path -- the
# animation tests load them from here, and a site repository copies whichever
# it serves. Nothing is written outside this repository.
PROCEDURAL_VARIANTS := starfield plasma lava aurora vortex caustics orbitals kaleidoscope
PARTICLE_VARIANTS := materialise atomise prompt lifecycle

effects:
	@for variant in $(PROCEDURAL_VARIANTS); do \
		echo "   procedural_backgrounds:$$variant"; \
		(cd effects/procedural_backgrounds && \
			cargo build --quiet --release --target wasm32-unknown-unknown \
				--target-dir target-$$variant --no-default-features --features $$variant) || exit 1; \
	done
	@for variant in $(PARTICLE_VARIANTS); do \
		echo "   content_particles:$$variant"; \
		(cd effects/content_particles && \
			cargo build --quiet --release --target wasm32-unknown-unknown \
				--target-dir target-$$variant --no-default-features --features $$variant) || exit 1; \
	done
	@for crate in static_noise matrix_rain line_draw typewriter; do \
		echo "   $$crate"; \
		(cd effects/$$crate && cargo build --quiet --release --target wasm32-unknown-unknown) || exit 1; \
	done
	@echo "   line_draw:top-down"
	@cd effects/line_draw && cargo build --quiet --release --target wasm32-unknown-unknown \
		--target-dir target-top-down --features top-down

# Serve any site directory locally. SITE_DIR is required: this repository is
# the software and ships no site of its own beyond examples/.
#
#   make serve SITE_DIR=../some-site
serve: site
	$(MISE) cargo run --bin dustnetd -- $(SITE_BUILD) --port 1985 --plaintext-loopback --log-format human

client:
	eval "$$(mise activate bash)" && cargo run --bin dustnet -- connect atp://127.0.0.1:1985/ --no-tls

dev-server: serve

dev-client: client

clean:
	eval "$$(mise activate bash)" && cargo clean

# Fuzz testing — run until interrupted with Ctrl-C.
# Seeds in fuzz/seeds/ are committed; corpus/ and artifacts/ are gitignored.
# cargo fuzz uses fuzz/corpus/<target> as main corpus (auto-created),
# and reads fuzz/seeds/<target> as a read-only seed directory.
fuzz:
	@echo "Run individual targets: make fuzz-scanner, fuzz-parser, fuzz-pipeline, fuzz-protocol, fuzz-uri, fuzz-protocol-state, fuzz-viewer-state"
	@echo "Or run one directly: cargo +$(NIGHTLY) fuzz run fuzz_scanner -- -max_len=4096"

fuzz-scanner:
	@mkdir -p fuzz/corpus/fuzz_scanner
	eval "$$(mise activate bash)" && cargo +$(NIGHTLY) fuzz run fuzz_scanner fuzz/corpus/fuzz_scanner fuzz/seeds/fuzz_scanner

fuzz-parser:
	@mkdir -p fuzz/corpus/fuzz_parser
	eval "$$(mise activate bash)" && cargo +$(NIGHTLY) fuzz run fuzz_parser fuzz/corpus/fuzz_parser fuzz/seeds/fuzz_parser

fuzz-pipeline:
	@mkdir -p fuzz/corpus/fuzz_pipeline
	eval "$$(mise activate bash)" && cargo +$(NIGHTLY) fuzz run fuzz_pipeline fuzz/corpus/fuzz_pipeline fuzz/seeds/fuzz_pipeline

fuzz-protocol:
	@mkdir -p fuzz/corpus/fuzz_protocol
	eval "$$(mise activate bash)" && cargo +$(NIGHTLY) fuzz run fuzz_protocol fuzz/corpus/fuzz_protocol fuzz/seeds/fuzz_protocol

fuzz-uri:
	@mkdir -p fuzz/corpus/fuzz_uri
	eval "$$(mise activate bash)" && cargo +$(NIGHTLY) fuzz run fuzz_uri fuzz/corpus/fuzz_uri fuzz/seeds/fuzz_uri

fuzz-protocol-state:
	@mkdir -p fuzz/corpus/fuzz_protocol_state
	eval "$$(mise activate bash)" && cargo +$(NIGHTLY) fuzz run fuzz_protocol_state fuzz/corpus/fuzz_protocol_state fuzz/seeds/fuzz_protocol_state

fuzz-viewer-state:
	@mkdir -p fuzz/corpus/fuzz_viewer_state
	eval "$$(mise activate bash)" && cargo +$(NIGHTLY) fuzz run fuzz_viewer_state fuzz/corpus/fuzz_viewer_state fuzz/seeds/fuzz_viewer_state
