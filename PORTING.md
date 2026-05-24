# wftt Porting Status

This repository is a Rust port of `gitnit`.

## Implemented

- CLI flags: `--repo`, `--provider`, `--model`, `--config`, `--show-config`.
- Config loading precedence compatible with the Python app:
  `~/.config/gitnit/gitnit.toml`, `./gitnit.toml`, `./.gitnit.toml`, then explicit `--config`.
- GitHub REST client using `GITHUB_TOKEN`.
- PR list and issue list with paging, refresh, issue sort toggle, and cached list startup.
- PR detail, issue detail, and PR diff screens.
- Provider-aware analysis cache keys compatible with the Python cache naming scheme.
- Gemini analysis via `GEMINI_API_KEY`.
- Clipboard copy for review comments and suggested fixes.
- Watch-path file fetching and PR title badges.
- Unit coverage for JSON extraction, cache key hash shape, comment aliases, and watch-path config filtering.

## Not Implemented Yet

- Claude Code provider. The Python app uses `claude-agent-sdk`; this Rust port currently reports it as unsupported and recommends Gemini.
- OpenAI and OpenRouter providers. These were also not implemented in the Python README.
- Background polling and new-activity modal notifications.
- Full Textual-equivalent visual styling. The Rust app uses a simpler `ratatui` layout.
- Scroll position management inside long detail/diff screens.
- Exhaustive GitHub pagination for PR files/comments beyond the first 100 returned by the REST calls.

## Verification

Current verification:

```text
cargo test
4 passed
```
