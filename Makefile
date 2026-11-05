# Meeting Master developer entry points. See README.md.
.PHONY: bootstrap test test-rust test-swift engines engines-release run mockup lint

bootstrap:          ## Install / verify toolchains and resolve dependencies
	scripts/bootstrap.sh

test: test-rust test-swift   ## All unit tests

test-rust:
	cargo test --workspace

test-swift:         ## Dependency-free Swift tests (JSON-RPC server, GCC-PHAT, speaker fusion)
	cd native/NativeKit && swift test

engines:            ## Build mm-meetingd + mm-embedd (Debug) into target/engines/Debug
	scripts/build-engines.sh Debug

engines-release:
	scripts/build-engines.sh Release

run:                ## Launch the GPUI workbench
	cargo run -p mm-app

mockup:             ## Rebuild design/mockups/index.html
	design/mockups/build.sh

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
