use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::Deserialize;
use toml::Value;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AiConfig {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    pub prompt_version: String,
}

impl Default for AiConfig {
    fn default() -> Self {
        Self {
            provider: "gemini".to_string(),
            model: "gemini-2.5-pro".to_string(),
            base_url: String::new(),
            api_key_env: String::new(),
            prompt_version: "v3".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GitHubConfig {
    pub repo: String,
    pub cache_ttl_seconds: u64,
    pub poll_interval_seconds: u64,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            repo: String::new(),
            cache_ttl_seconds: 600,
            poll_interval_seconds: 300,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CacheConfig {
    pub enabled: bool,
    pub dir: String,
    pub analysis_ttl_days: u64,
    pub review_input_ttl_days: u64,
    pub max_size_mb: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: "~/.cache/lgtm".to_string(),
            analysis_ttl_days: 30,
            review_input_ttl_days: 14,
            max_size_mb: 2048,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WatchedPathConfig {
    pub path: String,
    pub label: String,
    pub color: String,
}

impl Default for WatchedPathConfig {
    fn default() -> Self {
        Self {
            path: String::new(),
            label: String::new(),
            color: "red".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct WatchConfig {
    pub paths: Vec<WatchedPathConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub columns: Vec<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            columns: vec![
                "title".to_string(),
                "author".to_string(),
                "age".to_string(),
                "label".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ReviewConfig {
    pub enabled: bool,
    pub repo_path: String,
    pub system_prompt: String,
    pub system_prompt_file: String,
    pub min_tool_calls: usize,
    pub max_tool_calls: usize,
    pub max_tool_output_bytes: usize,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            repo_path: ".".to_string(),
            system_prompt: String::new(),
            system_prompt_file: String::new(),
            min_tool_calls: 3,
            max_tool_calls: 8,
            max_tool_output_bytes: 12_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LgtmConfig {
    pub ai: AiConfig,
    pub github: GitHubConfig,
    pub cache: CacheConfig,
    pub watch: WatchConfig,
    pub ui: UiConfig,
    pub review: ReviewConfig,
    #[serde(skip)]
    pub loaded_paths: Vec<PathBuf>,
}

impl Default for LgtmConfig {
    fn default() -> Self {
        Self {
            ai: AiConfig::default(),
            github: GitHubConfig::default(),
            cache: CacheConfig::default(),
            watch: WatchConfig::default(),
            ui: UiConfig::default(),
            review: ReviewConfig::default(),
            loaded_paths: Vec::new(),
        }
    }
}

pub fn load_config(config_path: Option<&Path>, cwd: Option<&Path>) -> Result<LgtmConfig> {
    let cwd_buf;
    let cwd = match cwd {
        Some(path) => path,
        None => {
            cwd_buf = env::current_dir().context("could not resolve current directory")?;
            cwd_buf.as_path()
        }
    };

    let mut merged = Value::Table(BTreeMap::new().into_iter().collect());
    let mut loaded_paths = Vec::new();
    for path in default_config_paths(cwd) {
        if path.exists() {
            let value = read_toml(&path)?;
            deep_merge(&mut merged, value);
            loaded_paths.push(path);
        }
    }
    if let Some(path) = config_path {
        let value = read_toml(path)?;
        deep_merge(&mut merged, value);
        loaded_paths.push(path.to_path_buf());
    }

    normalize_legacy_top_level(&mut merged);
    let mut config: LgtmConfig = merged.try_into().context("invalid config shape")?;
    config.loaded_paths = loaded_paths;
    config.watch.paths.retain(|p| !p.path.trim().is_empty());
    load_review_system_prompt_file(&mut config, cwd)?;
    Ok(config)
}

fn load_review_system_prompt_file(config: &mut LgtmConfig, cwd: &Path) -> Result<()> {
    let prompt_file = config.review.system_prompt_file.trim();
    if prompt_file.is_empty() {
        return Ok(());
    }
    let path = PathBuf::from(prompt_file);
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let file_prompt = fs::read_to_string(&path).with_context(|| {
        format!(
            "unable to read review.system_prompt_file {}",
            path.display()
        )
    })?;
    if config.review.system_prompt.trim().is_empty() {
        config.review.system_prompt = file_prompt;
    } else if !file_prompt.trim().is_empty() {
        config.review.system_prompt = format!(
            "{}\n\n{}",
            config.review.system_prompt.trim_end(),
            file_prompt.trim_start()
        );
    }
    Ok(())
}

fn default_config_paths(cwd: &Path) -> Vec<PathBuf> {
    vec![
        user_config_path(),
        cwd.join("lgtm.toml"),
        cwd.join(".lgtm.toml"),
    ]
}

fn user_config_path() -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("lgtm").join("lgtm.toml")
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".config")
            .join("lgtm")
            .join("lgtm.toml")
    }
}

fn read_toml(path: &Path) -> Result<Value> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("unable to read config file {}", path.display()))?;
    content
        .parse::<Value>()
        .with_context(|| format!("invalid TOML in config file {}", path.display()))
}

fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Table(base), Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) if existing.is_table() && value.is_table() => {
                        deep_merge(existing, value);
                    }
                    _ => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn normalize_legacy_top_level(value: &mut Value) {
    let Some(table) = value.as_table_mut() else {
        return;
    };
    let repo = table.get("repo").cloned();
    let provider = table.get("provider").cloned();
    let model = table.get("model").cloned();

    if let Some(repo) = repo {
        table
            .entry("github".to_string())
            .or_insert_with(|| Value::Table(Default::default()))
            .as_table_mut()
            .expect("github inserted as table")
            .entry("repo".to_string())
            .or_insert(repo);
    }
    if provider.is_some() || model.is_some() {
        let ai = table
            .entry("ai".to_string())
            .or_insert_with(|| Value::Table(Default::default()))
            .as_table_mut()
            .expect("ai inserted as table");
        if let Some(provider) = provider {
            ai.entry("provider".to_string()).or_insert(provider);
        }
        if let Some(model) = model {
            ai.entry("model".to_string()).or_insert(model);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watched_paths_without_path_are_removed() {
        let dir = tempfile_path();
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lgtm.toml");
        fs::write(
            &path,
            r#"
            [watch]
            paths = [{ path = "src", label = "src" }, { label = "bad" }]
            "#,
        )
        .unwrap();
        let config = load_config(Some(&path), Some(&dir)).unwrap();
        assert_eq!(config.watch.paths.len(), 1);
        assert_eq!(config.watch.paths[0].path, "src");
    }

    #[test]
    fn loads_cache_config() {
        let dir = tempfile_path().join("cache-config");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lgtm.toml");
        fs::write(
            &path,
            r#"
            [cache]
            enabled = false
            dir = "/tmp/lgtm-cache-test"
            analysis_ttl_days = 7
            review_input_ttl_days = 3
            max_size_mb = 128
            "#,
        )
        .unwrap();
        let config = load_config(Some(&path), Some(&dir)).unwrap();
        assert!(!config.cache.enabled);
        assert_eq!(config.cache.dir, "/tmp/lgtm-cache-test");
        assert_eq!(config.cache.analysis_ttl_days, 7);
        assert_eq!(config.cache.review_input_ttl_days, 3);
        assert_eq!(config.cache.max_size_mb, 128);
    }

    #[test]
    fn loads_review_system_prompt() {
        let dir = tempfile_path().join("review-system-prompt");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lgtm.toml");
        fs::write(
            &path,
            r#"
            [review]
            system_prompt = "Focus on auth boundaries."
            "#,
        )
        .unwrap();
        let config = load_config(Some(&path), Some(&dir)).unwrap();
        assert_eq!(config.review.system_prompt, "Focus on auth boundaries.");
    }

    #[test]
    fn loads_review_system_prompt_file() {
        let dir = tempfile_path().join("review-system-prompt-file");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("review.md"), "Focus on sandbox boundaries.\n").unwrap();
        let path = dir.join("lgtm.toml");
        fs::write(
            &path,
            r#"
            [review]
            system_prompt_file = "review.md"
            "#,
        )
        .unwrap();
        let config = load_config(Some(&path), Some(&dir)).unwrap();
        assert_eq!(
            config.review.system_prompt,
            "Focus on sandbox boundaries.\n"
        );
        assert_eq!(config.review.system_prompt_file, "review.md");
    }

    #[test]
    fn combines_inline_and_file_system_prompts() {
        let dir = tempfile_path().join("combined-review-system-prompt");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("review.md"), "Then focus on compatibility.\n").unwrap();
        let path = dir.join("lgtm.toml");
        fs::write(
            &path,
            r#"
            [review]
            system_prompt = "First focus on auth."
            system_prompt_file = "review.md"
            "#,
        )
        .unwrap();
        let config = load_config(Some(&path), Some(&dir)).unwrap();
        assert_eq!(
            config.review.system_prompt,
            "First focus on auth.\n\nThen focus on compatibility.\n"
        );
    }

    fn tempfile_path() -> PathBuf {
        env::temp_dir().join(format!("lgtm-test-{}", std::process::id()))
    }
}
