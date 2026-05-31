# lgtm Porting Status

This repository is a Rust port of `lgtm`.

## Implemented

- CLI flags: `--repo`, `--provider`, `--model`, `--base-url`, `--api-key-env`, `--config`, `--show-config`.
- Config loading precedence compatible with the Python app:
  `~/.config/lgtm/lgtm.toml`, `./lgtm.toml`, `./.lgtm.toml`, then explicit `--config`.
- GitHub REST client using `GITHUB_TOKEN`.
- PR list and issue list with paging, refresh, issue sort toggle, and cached list startup.
- PR list loading from GitHub's lightweight pull-list endpoint, with detail-only count fields defaulted until PR detail is opened.
- PR detail, issue detail, and PR diff screens.
- Provider-aware analysis cache keys compatible with the Python cache naming scheme.
- REST provider interface with Gemini, Ollama, OpenAI, OpenRouter, LM Studio, and vLLM-compatible providers.
- Provider/model resolution from either `provider = "gemini"` plus `model = "gemini-2.5-pro"` or namespaced models like `model = "ollama/llama3.1"`.
- Config-driven cache behavior via `[cache]`: `enabled`, `dir`, `analysis_ttl_days`, `review_input_ttl_days`, and `max_size_mb`.
- Analysis cache TTL checks and max-size pruning.
- Clipboard copy for review comments and suggested fixes.
- Watch-path file fetching and PR title badges.
- Unit coverage for JSON extraction, provider resolution, cache config, cache TTL, cache disable, cache key hash shape, comment aliases, and watch-path config filtering.

## Not Implemented Yet

- Review-input cache behavior. The config key is loaded, but no separate review-input artifact is cached yet.
- Background polling and new-activity modal notifications.
- Full Textual-equivalent visual styling. The Rust app uses a simpler `ratatui` layout.
- Scroll position management inside long detail/diff screens.
- Exhaustive GitHub pagination for PR files/comments beyond the first 100 returned by the REST calls.

## Verification

Current verification:

```text
cargo test
13 passed
```
