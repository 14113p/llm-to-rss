# llm-to-rss

Async Rust single-binary service that turns scheduled LLM prompts (with native provider web search and URL/RSS pre-fetchers) into virtual Atom 1.0, RSS 2.0, and OPML 2.0 feeds backed by SQLite.

## Architecture & Module Map

- `src/main.rs` — CLI entrypoint, tracing setup, and server/scheduler/reloader wiring.
- `src/cli.rs` — `clap` CLI subcommands (`serve`, `run`, `validate`, `list`, `opml`).
- `src/config.rs` — YAML config & `feeds/*.yaml` loader, `${VAR:-default}` env expansion, cron normalization (5/6/7 fields), and validation.
- `src/error.rs` — Unified `AppError` enum (`thiserror`) and crate `Result<T>`.
- `src/http.rs` — Shared outbound HTTP retry/backoff helper (`429`/`408`/`5xx` + transient transport errors, honoring `Retry-After`).
- `src/llm/` — Provider backends dispatched from `src/llm/mod.rs`:
  - `openai_responses.rs` (`/v1/responses` + `web_search` + `url_citation`)
  - `openai_compatible.rs` (`/v1/chat/completions` for Ollama, OpenRouter, Perplexity, LiteLLM)
  - `gemini.rs` (`:generateContent` + `google_search` + `groundingMetadata`)
  - `anthropic.rs` (`/v1/messages` + `web_search_20250305` + citations)
- `src/reload.rs` — `ConfigHandle` (`tokio::sync::watch`) hot-reload state, content-fingerprinting file watcher, and `SIGHUP` handler.
- `src/render.rs` — `PromptContext` template substitution (`{{sources}}`, `{{previous_entry}}`, etc.), Markdown-to-HTML (`pulldown-cmark`), citation rendering, Atom 1.0, RSS 2.0, and OPML 2.0 builders.
- `src/runner.rs` — `AppContext` shared service handle, end-to-end feed execution (`fetch -> prompt -> LLM -> skip/dedupe -> store -> mark_seen -> record_run`), and `RunTracker` per-feed concurrency guard.
- `src/scheduler.rs` — Dynamic supervisor managing per-feed timezone-aware cron loops that reconcile on config hot-reload.
- `src/server/` — Axum HTTP server split into `mod.rs` (router), `api.rs` (`/healthz`, `POST /feeds/{id}/run`, `POST /reload`), `feeds.rs` (`/opml.xml`, `/feeds/{id}.xml|.rss`), and `dashboard.rs` + embedded `dashboard.html` (`/`).
- `src/sources/` — Concurrent (`JoinSet`) pre-fetchers dispatched from `src/sources/mod.rs` (`url.rs`, `rss.rs` with SQLite seen-item filtering, and `html.rs` text cleanup/truncation) with `fail_on_source_error` tolerance.
- `src/store.rs` — SQLite persistence (`rusqlite` bundled, WAL mode) with idempotent schema migrations, `entries` (including `raw_markdown`), `runs`, and `seen_source_items`.
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
