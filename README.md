# wftt

A terminal UI for reviewing GitHub pull requests and issues, with AI-powered analysis.

![wftt screenshot](docs/screenshot.png)

## Install

```bash
cargo install --path .
```

## Requirements

- `GITHUB_TOKEN` environment variable with repo read access
- An AI provider API key (OpenAI, Gemini, Anthropic, or any OpenAI-compatible endpoint)

## Usage

```bash
wftt --repo owner/repo
```

Or set defaults in a config file (see below) and just run `wftt`.

## Keys

| Key | Action |
|-----|--------|
| `Enter` | Open PR / issue detail and run AI analysis |
| `Tab` | Switch between Pull Requests and Issues |
| `Left` / `Right` | Previous / next list page |
| `r` | Refresh from GitHub |
| `s` | Cycle sort mode |
| `f` | Cycle cache filter (all / cached / uncached) |
| `/` | Search current list/detail/diff text |
| `x` | Clear search and filters |
| `o` | Open selected PR / issue in GitHub |
| `d` | View diff (from PR detail) |
| `c` | Copy AI review to clipboard |
| `v` | Copy summary / overview from detail screens |
| `n` / `p` | Next / previous file in diff viewer |
| `]` / `[` | Next / previous hunk in diff viewer |
| `i` | Runtime info |
| `?` | Help |
| `q q` | Quit |

PRs are sorted by reviewability by default — passing CI, small diffs, and recently updated first.

## Config

Create a `gitnit.toml` in your project or home directory:

```toml
[github]
repo = "owner/repo"

[ai]
provider = "gemini"
model = "gemini-2.0-flash"
api_key_env = "GEMINI_API_KEY"

[ui]
columns = ["title", "author", "age", "label"]
```

Supported providers: `openai`, `gemini`, `anthropic`, or any OpenAI-compatible endpoint via `base_url`.

## CLI flags

```
-r, --repo         GitHub repo (owner/repo)
-p, --provider     AI provider
-m, --model        Model name
    --base-url     Override provider base URL
    --api-key-env  Env var holding the API key
    --config       Path to config file
    --show-config  Print resolved config and exit
```
