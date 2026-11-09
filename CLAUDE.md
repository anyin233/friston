@AGENTS.md

# Meeting Master — project rules for coding agents

Read `docs/architecture.md` and `docs/tech-stack.md` before changing structure or dependencies.

## Build & test

- `make test`: `cargo test --workspace` plus `swift test` in `native/NativeKit`. Run it before reporting work as done.
- `make engines`: builds `mm-meetingd` and `mm-embedd` with **xcodebuild**. Plain `swift build` does not compile MLX's Metal shaders. The binaries and their resource bundles land in `target/engines/<Config>/`.
- `make run`: launches the GPUI app. `make lint`: rustfmt and clippy with `-D warnings`.
- `design/mockups/build.sh` rebuilds `design/mockups/index.html` from `design/mockups/src/`. Edit `src/` only, never `index.html` directly.

## Invariants

- **Two native engines on purpose.** speech-swift pins `mlx-swift-lm` exact 3.31.4, while EmbeddingGemma 2 needs `mlx-swift-lm` main. Never merge the two packages.
- **Protocol names are duplicated.** `crates/mm-engine/src/methods.rs` and `native/NativeKit/Sources/EngineKit/Methods.swift` must change together, along with `docs/engine-protocol.md`.
- **Engine stdout is protocol only.** Logs go to stderr (`EngineKit.log`).
- **EmbeddingGemma 2 applies its own task prompts.** Send the raw text plus a separate `title`; never send pre-formatted `title: … | text:` strings.
- **Vector stamps.** Every stored vector carries `model_stamp(...)`; changing the model, dimensions or prompts must trigger a re-index.
- **Obsidian notes are shared with the user.** Only rewrite the `<!-- mm:NAME:start/end -->` managed blocks, and always use `write_atomic`.
- **Spatial cues are per meeting.** Never store direction of arrival in voiceprints, and treat virtual/aggregate audio devices as non-spatial.
- **Codex integration** must follow the vendored schema `crates/mm-llm/schema/`. Threads are ephemeral and read-only with `approvalPolicy: never`. When bumping the minimum Codex version, regenerate the schema with `codex app-server generate-json-schema`.
- **Pinned dependencies:** `sqlite-vec` stays at =0.1.9 (0.1.10-alpha.4 does not build).

## Conventions

- Every function has a doc comment (purpose, params, return). Comments explain *why*.
- Errors carry context: include the path, method or value.
- UI copy is bilingual (`mm-i18n`): wrap every user-visible string in `tr!("中文", "English")` or `trf!(…)`, sentence-style, active voice in both. Never translate prompts, ASR context, index text, logs or protocol keys; code that parses note text must accept both languages. Tests pin a language with `mm_i18n::with_lang`. Speaker hues are identity, so one person keeps one hue on every surface.
