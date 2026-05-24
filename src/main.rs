mod ai;
mod app;
mod cache;
mod config;
mod github;
mod models;
mod sorting;

use std::{env, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "wftt",
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

    #[arg(long, value_name = "PATH", help = "Path to a gitnit.toml config file")]
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
        println!("ai.provider = {provider}");
        println!("ai.model = {model}");
        println!("ai.prompt_version = {}", config.ai.prompt_version);
        println!(
            "github.cache_ttl_seconds = {}",
            config.github.cache_ttl_seconds
        );
        println!(
            "github.poll_interval_seconds = {}",
            config.github.poll_interval_seconds
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
            prompt_version: config.ai.prompt_version,
            cache_ttl_seconds: config.github.cache_ttl_seconds,
            poll_interval_seconds: config.github.poll_interval_seconds,
            config_paths: config.loaded_paths,
            watch_paths: config.watch.paths,
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
