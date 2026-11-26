# llm-to-rss

Async Rust single-binary service that turns scheduled LLM prompts (with native provider web search and URL/RSS pre-fetchers) into virtual Atom 1.0, RSS 2.0, and OPML 2.0 feeds backed by SQLite.

## Architecture & Module Map

- `src/main.rs` — CLI entrypoint, tracing setup, and server/scheduler/reloader wiring.
- `src/cli.rs` — `clap` CLI subcommands (`serve`, `run`, `validate`, `list`, `opml`).
- `src/config.rs` — YAML config & `feeds/*.yaml` loader, `${VAR:-default}` env expansion, cron normalization (5/6/7 fields), and validation.
- `src/error.rs` — Unified `AppError` enum (`thiserror`) and crate `Result<T>`.
- `src/fetcher.rs` — Pre-fetchers for `type: url` (HTML-to-text + truncation) and `type: rss` (RSS 2.0 / Atom 1.0 parsing).
- `src/llm/` — Provider backends dispatched from `src/llm/mod.rs`:
  - `openai_responses.rs` (`/v1/responses` + `web_search` + `url_citation`)
  - `openai_compatible.rs` (`/v1/chat/completions` for Ollama, OpenRouter, Perplexity, LiteLLM)
  - `gemini.rs` (`:generateContent` + `google_search` + `groundingMetadata`)
  - `anthropic.rs` (`/v1/messages` + `web_search_20250305` + citations)
- `src/render.rs` — Prompt template substitution, Markdown-to-HTML (`pulldown-cmark`), citation rendering, Atom 1.0, RSS 2.0, and OPML 2.0 builders.
- `src/runner.rs` — End-to-end feed execution (`fetch -> prompt -> LLM -> skip/dedupe -> store -> record_run`).
- `src/scheduler.rs` — Per-feed timezone-aware cron scheduler loops.
- `src/server.rs` — Axum HTTP server (`/`, `/healthz`, `/opml.xml`, `/feeds/{id}.xml|.rss`, `POST /feeds/{id}/run`).
- `src/store.rs` — SQLite persistence (`rusqlite` bundled, WAL mode) with retention pruning and run history.
- `tests/integration.rs` — Loopback HTTP integration tests with mock upstream RSS and LLM endpoints.

## Common Commands

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo run -- validate --config config.example.yaml
```

## Design Principles

- **Zero external runtime dependencies**: Single static-friendly Rust binary + bundled SQLite; keep dependency footprint lean so Docker builds and homelab/ARM64 deployments stay fast and reliable.
- **Backwards-compatible schema & config**: Add new YAML fields with sensible `#[serde(default)]` values and apply idempotent SQLite migrations in `Store::open` so existing deployments upgrade without manual intervention.
- **Composable modules**: Prefer small context/config structs over long positional parameter lists; keep shared concerns (HTTP retry/backoff, config watching, concurrency guards) in focused helpers rather than duplicating logic across providers or endpoints.
- **Safe failure modes**: Never crash the server on transient network errors or invalid hot-reloaded YAML; preserve last-known-good state and log structured `tracing` diagnostics.
- **Verification**: Every feature must include unit and/or loopback integration tests and pass `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all-targets`, and `cargo run -- validate --config config.example.yaml`.

## Git Conventions

- Follow Conventional Commits prefixes (`feat:`, `fix:`, `chore:`, `refactor:`, `docs:`, `test:`).
- Use **title-only** commit messages (no commit body/description).
- Keep one focused commit per feature or fix.
