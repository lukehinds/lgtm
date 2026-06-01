mod ai;
mod app;
mod cache;
mod config;
mod github;
mod models;
mod sorting;
mod worktree;

use std::{env, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "lgtm",
    version,
    about = "AI-powered TUI for reviewing GitHub pull requests and issues"
)]
struct Cli {
    #[arg(short, long, help = "GitHub repository in owner/repo format")]
    repo: Option<String>,

    #[arg(short, long, help = "AI provider to use for analysis")]
    provider: Option<String>,

    #[arg(short, long, help = "Model to use for AI analysis")]
    model: Option<String>,

    #[arg(long, help = "Override the provider base URL")]
    base_url: Option<String>,

    #[arg(long, help = "Environment variable that contains the provider API key")]
    api_key_env: Option<String>,

    #[arg(long, value_name = "PATH", help = "Path to a lgtm.toml config file")]
    config: Option<PathBuf>,

    #[arg(long, help = "Print resolved config values and exit")]
    show_config: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config =
        config::load_config(cli.config.as_deref(), None).context("failed to load config")?;

    let repo = cli.repo.or_else(|| non_empty(config.github.repo.clone()));
    let provider = cli
        .provider
        .unwrap_or_else(|| config.ai.provider.clone())
        .to_lowercase();
    let model = cli.model.unwrap_or_else(|| config.ai.model.clone());
    let base_url = cli.base_url.unwrap_or_else(|| config.ai.base_url.clone());
    let api_key_env = cli
        .api_key_env
        .unwrap_or_else(|| config.ai.api_key_env.clone());
    let resolved_ai = ai::resolve_provider_config(&provider, &model, &base_url, &api_key_env);

    if cli.show_config {
        let loaded = if config.loaded_paths.is_empty() {
            "(none)".to_string()
        } else {
            config
                .loaded_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!("config_paths = {loaded}");
        println!("github.repo = {}", repo.clone().unwrap_or_default());
        println!("ai.provider = {}", resolved_ai.provider);
        println!("ai.model = {}", resolved_ai.model);
        println!("ai.base_url = {}", resolved_ai.base_url);
        println!("ai.api_key_env = {}", resolved_ai.api_key_env);
        println!("ai.prompt_version = {}", config.ai.prompt_version);
        println!("cache.enabled = {}", config.cache.enabled);
        println!("cache.dir = {}", config.cache.dir);
        println!(
            "cache.analysis_ttl_days = {}",
            config.cache.analysis_ttl_days
        );
        println!(
            "cache.review_input_ttl_days = {}",
            config.cache.review_input_ttl_days
        );
        println!("cache.max_size_mb = {}", config.cache.max_size_mb);
        println!(
            "github.cache_ttl_seconds = {}",
            config.github.cache_ttl_seconds
        );
        println!(
            "github.poll_interval_seconds = {}",
            config.github.poll_interval_seconds
        );
        println!("ui.columns = {}", config.ui.columns.join(", "));
        println!("review.enabled = {}", config.review.enabled);
        println!("review.repo_path = {}", config.review.repo_path);
        println!(
            "review.system_prompt_file = {}",
            if config.review.system_prompt_file.trim().is_empty() {
                "(none)"
            } else {
                &config.review.system_prompt_file
            }
        );
        println!(
            "review.system_prompt = {}",
            if config.review.system_prompt.trim().is_empty() {
                "(none)"
            } else {
                "(set)"
            }
        );
        println!("review.min_tool_calls = {}", config.review.min_tool_calls);
        println!("review.max_tool_calls = {}", config.review.max_tool_calls);
        println!(
            "review.max_tool_output_bytes = {}",
            config.review.max_tool_output_bytes
        );
        return Ok(());
    }

    let repo = repo.context("--repo is required unless github.repo is set in config")?;
    if !repo.contains('/') {
        bail!("--repo must be in owner/repo format");
    }
    if env::var("GITHUB_TOKEN")
        .ok()
        .filter(|v| !v.is_empty())
        .is_none()
    {
        bail!("GITHUB_TOKEN environment variable is required");
    }

    let client = github::GitHubClient::new(repo.clone())?;
    app::run(
        app::AppConfig {
            repo,
            provider,
            model,
            base_url,
            api_key_env,
            prompt_version: config.ai.prompt_version,
            cache: config.cache,
            cache_ttl_seconds: config.github.cache_ttl_seconds,
            poll_interval_seconds: config.github.poll_interval_seconds,
            config_paths: config.loaded_paths,
            watch_paths: config.watch.paths,
            columns: config.ui.columns,
            review: config.review,
        },
        client,
    )
    .await
}

fn non_empty(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}
