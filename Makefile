SHELL := /bin/bash

# This repository is the software. It ships no site of its own: authored
# content lives in its own repository and versions separately. Point SITE_DIR
# at one to build or serve it locally.
SITE_DIR ?=
SITE_BUILD := target/site

.PHONY: release release-check crosscheck-or-explain ci ci-preflight ci-publish-dryrun docker-image docker-run docker-check docker-publish docker-dustnetd docker-dustnetd-check docker-dustnetd-publish install install-check build-release ci-fmt ci-clippy ci-boundaries ci-tests ci-tools ci-docs ci-deps ci-full ci-fuzz-smoke ci-miri ci-miri-core ci-miri-compositor ci-asan test build check allocation-audit fuzz-campaign-check fuzz-periodic effects clean site-builder-check site serve client dev-server dev-client fuzz fuzz-campaign fuzz-check fuzz-scanner fuzz-parser fuzz-pipeline fuzz-protocol fuzz-uri fuzz-protocol-state fuzz-viewer-state

# ─── Local verification gate ─────────────────────────────────
#
# All verification runs locally on macOS. The gate is not hosted: the
# GitHub Actions workflow was removed because the account does not pay
# for Actions minutes, so every run sat queued and no gate was ever
# actually enforced. A local gate that runs is worth more than a hosted
# one that does not.
#
# The repository is public now, so standard runners are unmetered and that
# argument is weaker than it was — but only for Linux. The gate is macOS, and
# macOS runners still bill at 10x, so moving it hosted is a decision in its
# own right rather than a consequence of going public.
#
# One hosted workflow does exist: .github/workflows/publish-image.yml builds
# both container images -- the client and the server base -- on a tag. It is
# Linux, it runs a few times a year, and it produces the one artefact no single
# machine can: a manifest list covering both amd64 and arm64.
#
# The docker-*-publish targets below remain for publishing by hand. They need a
# GHCR login; the workflow does not, because the built-in GITHUB_TOKEN can write
# packages owned by this repository.
#
#   make release        cut a release: checks, crates.io, tag, images
#   make release-check  the same checks, publishing nothing
#
#   make test           fast inner loop — use while working        (~10s)
#   make ci             the full gate — before every commit       (minutes)
#   make ci-full        ci plus Miri, ASan, fuzz smoke            (~20 min)
#   make fuzz-periodic  the fuzz campaign, on its own             (~40 min)
#
# `make test` runs the `quick` nextest profile, which omits the three
# wall-clock deadline tests that are 20.3s of a 22.5s suite; the omission is
# named and argued in .config/nextest.toml. `make ci` runs the default profile
# and omits nothing, so the deadline tests are still gated before every commit
# rather than only before a release.
#
# `fuzz-periodic` is the campaign: eight targets at FUZZ_SECONDS each, 40
# minutes at the default 300. It is in no gate, and the road here is worth
# recording because both previous placements failed the same way.
#
# It first lived in no tier at all while `allocation-audit` — a prerequisite of
# both `ci` and `test` — required its output, so the gate *and* the inner loop
# were unpassable after any edit under crates/ until an undocumented 40-minute
# command had been run. That is how 0.2.0-alpha.3 was cut with a red gate. So
# it moved into ci-full, which made a release cost an hour and, because the
# rows are keyed on a fingerprint of every source under crates/, demanded a
# fresh parser campaign for a change to client session storage that no fuzz
# target can reach.
#
# Both failures are the same failure: a 40-minute requirement invalidated by
# edits it has nothing to do with, sitting in front of something people need to
# run. It is now triggered by judgement — when fuzzed code changes. Note that
# `--check-campaign` still *fails* when a row is missing; what changed is that
# nothing invokes it on your behalf. The rows in
# verification/fuzz-campaign.tsv record which code was fuzzed, so what a
# release skipped is answerable instead of assumed.
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

# The campaign is deliberately *not* here, though a bounded fuzz smoke run is.
#
# It used to be, and it made this target cost about an hour, of which the
# campaign was forty minutes. The reason that was intolerable is not that
# fuzzing is slow — it is that the campaign is keyed on a fingerprint of every
# source under `crates/`, so any edit anywhere invalidates all eight targets at
# once. A change to client-side session storage, which no fuzz target can
# reach, demanded a fresh parser campaign. The requirement was therefore paid
# constantly and told you almost nothing, which is how a gate stops being read
# and starts being worked around.
#
# So it moved to `make fuzz-periodic`, run when the fuzzed code itself changes
# rather than before every release. That is a weaker guarantee, honestly: a
# release can now go out whose parser has not been fuzzed at its exact
# fingerprint. The rows in `verification/fuzz-campaign.tsv` say which code was
# fuzzed and for how long, so what a release is missing is answerable rather
# than assumed.
ci-full: ci ci-fuzz-smoke ci-miri ci-asan
	@echo "── ci-full: all gates passed, including Miri/ASan and a fuzz smoke run ──"
	@echo "   the campaign is separate: make fuzz-periodic"

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

# Default profile, explicitly named: the gate is the one place nothing is
# filtered, and naming it here means a change to the profile default cannot
# quietly narrow what `make ci` runs.
ci-tests: fuzz-check
	@echo "── workspace tests ──"
	$(MISE) cargo nextest run --profile default --workspace --all-features -j 4

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
# Its own target directory rather than the shared `target/`: verification
# builds each package with its path dependencies rewritten as registry
# dependencies, and Cargo treats a registry source as immutable. A
# `dustnet_core` rlib left in the dev cache by an earlier build of the same
# version therefore matches by fingerprint and is reused, so a freshly
# packaged `dustnet-server` compiles against a stale core. That is not
# hypothetical: it presented as eleven E0408/E0599 errors about
# `MessageType::Ping` and `UpdateMessage::validate_update_parts`, items the
# tarball demonstrably contained. Nothing in the output named a cache -- the
# only clue was a missing `Compiling dustnet-core` line under `Verifying
# dustnet-server`. A pre-release gate that can fail for a reason outside the
# tree it is checking is worse than no gate.
#
# Not part of `ci`: it packages and rebuilds five crates. Run it before a
# release, and whenever package metadata changes.
ci-publish-dryrun:
	@echo "── publish dry run ──"
	$(MISE) CARGO_TARGET_DIR=target/publish-verify \
		cargo publish --dry-run --locked --workspace
	@echo "── publish dry run: all five package cleanly ──"

# ─── Container image ─────────────────────────────────────────
#
# The README tells everyone to run the client in Docker, because the client
# renders untrusted content from strangers into your terminal and a container
# is the boundary. That advice only holds while Docker is also the *fastest*
# path. Building from the Dockerfile compiles the workspace inside
# rust:1.94-slim, which is minutes; a published image is one pull. Recommending
# the slowest route is how you get people running it on the host instead.
#
# GHCR rather than Docker Hub. A README command is an anonymous pull, and
# Docker Hub rate-limits anonymous pulls per IP, so the documented command
# fails for readers behind a shared address. GHCR imposes no such limit on
# public images and needs no account that is not already this repository's.
#
# Two architectures because this is a terminal application people run on their
# own machine, and a lot of those machines are Apple Silicon.
REGISTRY   ?= ghcr.io
IMAGE_NAME ?= dustnet-atp/dustnet
PLATFORMS  ?= linux/amd64,linux/arm64
IMAGE      := $(REGISTRY)/$(IMAGE_NAME)

# The sites' base image, published from Dockerfile.dustnetd. A second name
# rather than a second tag on the first: the client image is what someone
# installs to browse Dustnet, and a server sharing its name would be found by
# people who wanted the browser. Same registry, same version, same platforms —
# a site runs on whatever the person deploying it runs on, and half of those
# machines are Apple Silicon too.
IMAGE_DUSTNETD_NAME ?= dustnet-atp/dustnetd
IMAGE_DUSTNETD      := $(REGISTRY)/$(IMAGE_DUSTNETD_NAME)

# Read out of Cargo.toml, never restated. The README carried a hand-written
# `--version 0.2.0-alpha.4` that outlived the bump to 0.2.0, and dustnet-www
# still tells people to `cargo install --path crates/dustnet-cli` for a crate
# that has never existed under that name. Both were copies nobody rebuilt. An
# image tag is a copy too, so it is derived here.
VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

# Single architecture, loaded into the local daemon so it can actually be run.
# `buildx --load` cannot accept a multi-platform result: the daemon's image
# store holds one manifest, not a list. That is why this target and
# docker-check are separate rather than one parameterised target.
docker-image:
	@echo "── build $(IMAGE):$(VERSION) for this machine ──"
	docker buildx build --load -t $(IMAGE):$(VERSION) -t dustnet:latest .
	@echo "── built; run it with: make docker-run ──"

docker-run: docker-image
	docker run --rm -it -v "$(HOME)/.config/dustnet:/root/.config/dustnet" \
		dustnet:latest connect atp://dustnet.io

# Both architectures, discarded. This is the pre-release check: a cross build
# breaks for reasons a native build never shows, and finding that out during a
# tag push means a half-published manifest.
docker-check:
	@echo "── cross build $(PLATFORMS), no push ──"
	docker buildx build --platform $(PLATFORMS) --output=type=cacheonly .
	@echo "── both architectures build ──"

# Publishing by hand, for when the workflow is not the thing doing it.
#
# The clean-tree requirement is not decoration. `docker build` copies the
# working tree, not a commit, so an image built from a dirty tree corresponds
# to no revision anyone can check out — and it would carry a version tag
# claiming otherwise. `ci-publish-dryrun` refuses `--allow-dirty` for the same
# reason; this is that stance applied to the other publish path.
#
# `latest` moves only for a real release. Cargo treats a hyphen as a
# pre-release and skips it for a bare requirement; `latest` has no such rule,
# so pointing it at 0.3.0-alpha.1 would hand every README reader a pre-release
# they did not ask for.
docker-publish:
	@test -z "$$(git status --porcelain)" \
		|| { echo "  working tree is dirty; an image built from it matches no commit"; exit 1; }
	@echo "── push $(IMAGE):$(VERSION) for $(PLATFORMS) ──"
	@case "$(VERSION)" in \
		*-*) echo "  pre-release: tagging $(VERSION) only, not latest"; \
		     docker buildx build --platform $(PLATFORMS) --push \
		       -t $(IMAGE):$(VERSION) . ;; \
		*)   docker buildx build --platform $(PLATFORMS) --push \
		       -t $(IMAGE):$(VERSION) -t $(IMAGE):latest . ;; \
	esac
	@echo "── pushed $(IMAGE):$(VERSION) ──"

# ─── The sites' base image ───────────────────────────────────
#
# Same three shapes as the client image above and for the same reasons: one
# architecture loaded locally so it can be run, both architectures discarded as
# a pre-release check, both architectures pushed to publish.

docker-dustnetd:
	@echo "── build $(IMAGE_DUSTNETD):$(VERSION) for this machine ──"
	docker buildx build --load -f Dockerfile.dustnetd \
		-t $(IMAGE_DUSTNETD):$(VERSION) -t dustnetd:latest .
	@echo "── built; a site can now build FROM dustnetd:latest ──"

docker-dustnetd-check:
	@echo "── cross build $(PLATFORMS), no push ──"
	docker buildx build --platform $(PLATFORMS) -f Dockerfile.dustnetd \
		--output=type=cacheonly .
	@echo "── both architectures build ──"

# The clean-tree requirement carries further here than it does for the client.
# An image built from a dirty tree matches no revision, and this one is a *base*:
# every site built on it inherits that, and a site image is the thing actually
# serving when someone asks which version is running.
docker-dustnetd-publish:
	@test -z "$$(git status --porcelain)" \
		|| { echo "  working tree is dirty; an image built from it matches no commit"; exit 1; }
	@echo "── push $(IMAGE_DUSTNETD):$(VERSION) for $(PLATFORMS) ──"
	@case "$(VERSION)" in \
		*-*) echo "  pre-release: tagging $(VERSION) only, not latest"; \
		     docker buildx build --platform $(PLATFORMS) --push \
		       -f Dockerfile.dustnetd -t $(IMAGE_DUSTNETD):$(VERSION) . ;; \
		*)   docker buildx build --platform $(PLATFORMS) --push \
		       -f Dockerfile.dustnetd \
		       -t $(IMAGE_DUSTNETD):$(VERSION) -t $(IMAGE_DUSTNETD):latest . ;; \
	esac
	@echo "── pushed $(IMAGE_DUSTNETD):$(VERSION) ──"

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
                fuzz_protocol_state fuzz_viewer_state fuzz_uri fuzz_serialize

fuzz-campaign:
	@echo "── fuzz campaign ($(FUZZ_SECONDS)s per target) ──"
	@version=$$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1); \
	toolchain=$$(rustup run $(NIGHTLY) rustc --version | awk '{print $$1" "$$2}'); \
	code=$$($(MISE) cargo run --quiet --manifest-path tools/allocation-audit/Cargo.toml -- --fuzz-fingerprint); \
	test -s $(FUZZ_LOG) || printf 'version\ttarget\tseconds\texecutions\texecs_per_sec\tcrashes\thost\ttoolchain\tcode\n' > $(FUZZ_LOG); \
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
		printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
			"$$version" "$$target" "$(FUZZ_SECONDS)" "$${execs:-unknown}" \
			"$${rate:-unknown}" "$$crashes" "$(HOST_TRIPLE)" "$$toolchain" "$$code" \
			>> $(FUZZ_LOG); \
		rm -f "$$log"; \
		test "$$crashes" -eq 0 || { echo "  $$target crashed; record a finding and commit a seed"; exit 1; }; \
	done
	@echo "── fuzz campaign: $(FUZZ_LOG) updated ──"

ci-fuzz-smoke:
	@echo "── fuzz smoke (10s per target) ──"
	@for target in $(FUZZ_TARGETS); do \
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

# nextest rather than `cargo test` so the inner loop reuses the same test
# binaries the gate builds instead of rebuilding them under a second harness.
# The cost is doctests, which nextest cannot run; `ci-docs` owns those.
#
# -j 8 rather than the gate's 4, on 8 performance cores. The gate stays at 4
# because its extra tests are the wall-clock ones: signal_shutdown.rs already
# allows 15s for a debug binary to reach its listener "while the all-workspace
# checkpoint is also running CPU-heavy TLS/server tests", so loading the
# machine harder is how that assertion gets flaky. The quick profile omits
# every test with a wall-clock assertion in it, so it has no such deadline to
# miss and can use the cores.
test: fuzz-check site-builder-check allocation-audit
	$(MISE) cargo nextest run --profile quick --workspace --all-features -j 8

allocation-audit:
	eval "$$(mise activate bash)" && cargo fmt --manifest-path tools/allocation-audit/Cargo.toml -- --check
	eval "$$(mise activate bash)" && cargo run --quiet --manifest-path tools/allocation-audit/Cargo.toml -- --check

# Deliberately not a prerequisite of `test`, `ci` or `ci-full`: satisfying it
# costs a 40-minute campaign, and no gate anyone runs on a schedule can ask for
# that. Reached through `fuzz-periodic`, or run by hand after a campaign.
# The periodic tier: run this when the code a fuzz target exercises has
# changed — the parser, scanner, protocol, URI or serializer — rather than on a
# calendar or before a release.
#
# Order matters: fuzz-campaign-check runs *after* fuzz-campaign, so it does not
# merely restate what the campaign just wrote. What it catches is an edit to
# crates/ that landed while the campaign was running — the campaign fingerprints
# the tree once at the start, so a mid-run commit leaves rows that were already
# stale when they were written. That is not hypothetical; it is why this is two
# steps and not one.
fuzz-periodic: fuzz-campaign fuzz-campaign-check
	@echo "── fuzz campaign complete and accounted for ──"

fuzz-campaign-check:
	@echo "── fuzz campaign coverage ──"
	$(MISE) cargo run --quiet --manifest-path tools/allocation-audit/Cargo.toml -- --check-campaign
	@echo "  every fuzz target has a row for the code now in the tree"

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

# ─── Releasing ───────────────────────────────────────────────
#
# One sequence, because it used to be three and only two of them were joined up.
# A tag push published the images and stopped; crates.io was a `cargo publish`
# somebody had to remember, and `ci-publish-dryrun` only ever *checked* that it
# would work. So a release could half-happen with nothing failing: images and a
# tag at the new version, the registry still serving the old one, and `cargo
# install dustnet` handing out a version that no image matched.
#
# Safe to run by accident. Everything up to the point of no return runs on a bare
# `make release` and then stops, because `release` was previously an alias for
# `cargo build --release` and muscle memory should not be able to publish. The
# irreversible half needs CONFIRM=1: crates.io versions can be yanked but never
# replaced or reused, so a mistake there is permanent in a way a bad tag is not.
#
# crates.io goes first and the tag second. Both are recoverable in one direction
# only: a tag can be deleted and re-pushed, a version cannot be re-uploaded. So
# the order puts the unrecoverable step where a failure still leaves the tree
# untagged and the attempt repeatable, rather than leaving published images
# pointing at a release the registry never received.
release: release-check
	@test "$(CONFIRM)" = 1 || { \
		echo ""; \
		echo "  checks passed for $(VERSION). Nothing has been published."; \
		echo "  crates.io uploads are permanent -- a version can be yanked but"; \
		echo "  never replaced. To go ahead:"; \
		echo ""; \
		echo "      make release CONFIRM=1"; \
		echo ""; \
		exit 1; }
	@echo "── publish $(VERSION) to crates.io ──"
	$(MISE) cargo publish --locked --workspace
	@echo "── tag v$(VERSION); the push is what builds the images ──"
	git tag -a "v$(VERSION)" -m "$(VERSION)"
	git push origin "v$(VERSION)"
	@echo "── released $(VERSION): crates.io done, images building on the tag ──"

# The cross build, when this machine can do one.
#
# Building both architectures before a tag is the check that stops a cross-only
# failure becoming a half-published manifest, so it belongs in the release gate.
# But it needs a buildx driver that can do multi-platform, and the default
# `docker` driver cannot -- so on a machine without one, requiring it would block
# every release for a reason that has nothing to do with the code. That is how a
# check gets routed around instead of fixed.
#
# So the driver is probed, and the two outcomes are kept apart: where a cross
# build is possible it runs for real and a failure is fatal; where it is not, the
# release says so loudly rather than implying the check passed. CI builds both
# architectures natively on the tag either way, which is the backstop -- it is
# just later than here.
crosscheck-or-explain:
	@if [ "$$(docker buildx inspect 2>/dev/null | sed -n 's/^Driver: *//p')" = docker ]; then \
		echo ""; \
		echo "  !! cross build NOT verified: this machine's buildx uses the"; \
		echo "     single-platform \"docker\" driver. CI builds both architectures"; \
		echo "     natively on the tag, so a cross-only failure would surface"; \
		echo "     there rather than here. To check locally instead:"; \
		echo "         docker buildx create --use --driver docker-container"; \
		echo ""; \
	else \
		$(MAKE) --no-print-directory docker-check docker-dustnetd-check; \
	fi

# Everything that can fail without consequence, so it all fails before anything
# is published. Each check exists because its absence has cost something.
release-check: ci ci-publish-dryrun crosscheck-or-explain
	@echo "── release checks for $(VERSION) ──"
	@test -n "$(VERSION)" || { echo "  no version in Cargo.toml"; exit 1; }
	@test -z "$$(git status --porcelain)" \
		|| { echo "  working tree is dirty; a release must match a commit"; exit 1; }
# The version lives in thirteen places -- five crates carry their own and
# seven pin each other exactly -- and they have to agree or `cargo publish`
# refuses the workspace halfway through, after some crates are already up.
# Matched on the path too, so this sees only the workspace's own pins. A bare
# search for `version = "="` also catches third-party deps pinned exactly --
# triomphe is one -- and a check that fails on every release is a check people
# learn to skip.
	@bad=$$(grep -rhE 'path = "\.\./dustnet[^"]*".*version = "=[^"]*"' crates/*/Cargo.toml \
		| grep -oE 'version = "=[^"]*"' | grep -v '"=$(VERSION)"' | sort -u); \
		test -z "$$bad" || { \
			echo "  internal pins disagree with $(VERSION): $$bad"; exit 1; }
# A CHANGELOG still saying "Unreleased" means the notes were never dated, and
# the version people read about is not the one they installed.
	@grep -q '^## $(VERSION)' CHANGELOG.md \
		|| { echo "  CHANGELOG.md has no '## $(VERSION)' section"; exit 1; }
	@! grep -q '^## Unreleased' CHANGELOG.md \
		|| { echo "  CHANGELOG.md still has an Unreleased section"; exit 1; }
# A tag that already exists means this version went out once. Publishing over
# it is impossible on crates.io and misleading everywhere else.
	@! git rev-parse -q --verify "refs/tags/v$(VERSION)" >/dev/null \
		|| { echo "  tag v$(VERSION) already exists"; exit 1; }
	@! git ls-remote --exit-code --tags origin "v$(VERSION)" >/dev/null 2>&1 \
		|| { echo "  tag v$(VERSION) already on the remote"; exit 1; }
	@echo "── $(VERSION) is releasable ──"

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
