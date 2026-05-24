use std::{env, fs, path::PathBuf, time::Duration};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    config::CacheConfig,
    models::{IssueData, PrData},
};

const DEFAULT_PROVIDER: &str = "gemini";
const DEFAULT_MODEL: &str = "gemini-2.5-pro";
const DEFAULT_PROMPT_VERSION: &str = "v3";
const DEFAULT_PR_SCHEMA_VERSION: &str = "pr-analysis-v1";
const DEFAULT_ISSUE_SCHEMA_VERSION: &str = "issue-analysis-v1";

pub fn get_cached_pr_analysis(
    config: &CacheConfig,
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    provider: &str,
    model: &str,
    prompt_version: &str,
    schema_version: &str,
) -> Option<Value> {
    if !config.enabled {
        return None;
    }
    let path = pr_cache_path(
        config,
        repo,
        pr_number,
        head_sha,
        provider,
        model,
        prompt_version,
        schema_version,
    );
    read_fresh_analysis(&path, analysis_max_age(config)).or_else(|| {
        if provider == DEFAULT_PROVIDER
            && model == DEFAULT_MODEL
            && prompt_version == DEFAULT_PROMPT_VERSION
            && schema_version == DEFAULT_PR_SCHEMA_VERSION
        {
            read_fresh_analysis(
                &legacy_pr_cache_path(config, repo, pr_number, head_sha),
                analysis_max_age(config),
            )
        } else {
            None
        }
    })
}

pub fn save_pr_analysis(
    config: &CacheConfig,
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    data: &Value,
    provider: &str,
    model: &str,
    prompt_version: &str,
    schema_version: &str,
) {
    if !config.enabled {
        return;
    }
    let mut payload = data.as_object().cloned().unwrap_or_default();
    payload.insert(
        "_metadata".to_string(),
        metadata(provider, model, prompt_version, schema_version),
    );
    let _ = write_json(
        pr_cache_path(
            config,
            repo,
            pr_number,
            head_sha,
            provider,
            model,
            prompt_version,
            schema_version,
        ),
        &Value::Object(payload),
    );
    prune_cache(config);
}

pub fn get_cached_issue_analysis(
    config: &CacheConfig,
    repo: &str,
    issue_number: u64,
    provider: &str,
    model: &str,
    prompt_version: &str,
    schema_version: &str,
) -> Option<Value> {
    if !config.enabled {
        return None;
    }
    let path = issue_cache_path(
        config,
        repo,
        issue_number,
        provider,
        model,
        prompt_version,
        schema_version,
    );
    read_fresh_analysis(&path, analysis_max_age(config)).or_else(|| {
        if provider == DEFAULT_PROVIDER
            && model == DEFAULT_MODEL
            && prompt_version == DEFAULT_PROMPT_VERSION
            && schema_version == DEFAULT_ISSUE_SCHEMA_VERSION
        {
            read_fresh_analysis(
                &legacy_issue_cache_path(config, repo, issue_number),
                analysis_max_age(config),
            )
        } else {
            None
        }
    })
}

pub fn save_issue_analysis(
    config: &CacheConfig,
    repo: &str,
    issue_number: u64,
    data: &Value,
    provider: &str,
    model: &str,
    prompt_version: &str,
    schema_version: &str,
) {
    if !config.enabled {
        return;
    }
    let mut payload = data.as_object().cloned().unwrap_or_default();
    payload.insert(
        "_metadata".to_string(),
        metadata(provider, model, prompt_version, schema_version),
    );
    let _ = write_json(
        issue_cache_path(
            config,
            repo,
            issue_number,
            provider,
            model,
            prompt_version,
            schema_version,
        ),
        &Value::Object(payload),
    );
    prune_cache(config);
}

pub fn get_cached_pr_list(
    config: &CacheConfig,
    repo: &str,
    page: usize,
    max_age: Option<Duration>,
) -> Option<(Vec<PrData>, u64)> {
    if !config.enabled {
        return None;
    }
    read_fresh_list(list_cache_path(config, repo, "prs", page, ""), max_age)
}

pub fn save_pr_list(config: &CacheConfig, repo: &str, page: usize, prs: &[PrData], total: u64) {
    if !config.enabled {
        return;
    }
    let _ = write_list(list_cache_path(config, repo, "prs", page, ""), prs, total);
    prune_cache(config);
}

pub fn get_cached_issue_list(
    config: &CacheConfig,
    repo: &str,
    page: usize,
    direction: &str,
    max_age: Option<Duration>,
) -> Option<(Vec<IssueData>, u64)> {
    if !config.enabled {
        return None;
    }
    read_fresh_list(
        list_cache_path(config, repo, "issues", page, direction),
        max_age,
    )
}

pub fn save_issue_list(
    config: &CacheConfig,
    repo: &str,
    page: usize,
    issues: &[IssueData],
    total: u64,
    direction: &str,
) {
    if !config.enabled {
        return;
    }
    let _ = write_list(
        list_cache_path(config, repo, "issues", page, direction),
        issues,
        total,
    );
    prune_cache(config);
}

fn read_fresh_analysis(path: &PathBuf, max_age: Option<Duration>) -> Option<Value> {
    let value = read_json(path)?;
    if analysis_is_fresh(&value, max_age) {
        Some(value)
    } else {
        None
    }
}

fn read_fresh_list<T: DeserializeOwned>(
    path: PathBuf,
    max_age: Option<Duration>,
) -> Option<(Vec<T>, u64)> {
    let value = read_json(&path)?;
    if !is_fresh(&value, max_age) {
        return None;
    }
    let items = serde_json::from_value(value.get("items")?.clone()).ok()?;
    let total = value.get("total")?.as_u64()?;
    Some((items, total))
}

fn write_list<T: Serialize>(path: PathBuf, items: &[T], total: u64) -> Result<()> {
    write_json(
        path,
        &json!({
            "items": items,
            "total": total,
            "saved_at": Utc::now().to_rfc3339(),
        }),
    )
}

fn is_fresh(value: &Value, max_age: Option<Duration>) -> bool {
    let Some(max_age) = max_age else { return true };
    let Some(saved_at) = value.get("saved_at").and_then(Value::as_str) else {
        return false;
    };
    let Ok(saved) = DateTime::parse_from_rfc3339(saved_at) else {
        return false;
    };
    Utc::now()
        .signed_duration_since(saved.with_timezone(&Utc))
        .to_std()
        .map(|age| age <= max_age)
        .unwrap_or(false)
}

fn analysis_is_fresh(value: &Value, max_age: Option<Duration>) -> bool {
    let Some(max_age) = max_age else { return true };
    let Some(saved_at) = value
        .pointer("/_metadata/saved_at")
        .or_else(|| value.get("saved_at"))
        .and_then(Value::as_str)
    else {
        return false;
    };
    saved_at_is_fresh(saved_at, max_age)
}

fn saved_at_is_fresh(saved_at: &str, max_age: Duration) -> bool {
    let Ok(saved) = DateTime::parse_from_rfc3339(saved_at) else {
        return false;
    };
    Utc::now()
        .signed_duration_since(saved.with_timezone(&Utc))
        .to_std()
        .map(|age| age <= max_age)
        .unwrap_or(false)
}

fn metadata(provider: &str, model: &str, prompt_version: &str, schema_version: &str) -> Value {
    json!({
        "provider": provider,
        "model": model,
        "prompt_version": prompt_version,
        "schema_version": schema_version,
        "saved_at": Utc::now().to_rfc3339(),
    })
}

fn read_json(path: &PathBuf) -> Option<Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

fn write_json(path: PathBuf, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec(value)?)?;
    Ok(())
}

fn pr_cache_path(
    config: &CacheConfig,
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    provider: &str,
    model: &str,
    prompt_version: &str,
    schema_version: &str,
) -> PathBuf {
    let key = format!(
        "pr:{repo}:{pr_number}:{head_sha}:{provider}:{model}:{prompt_version}:{schema_version}"
    );
    cache_dir(config).join(format!("pr-{}.json", key_hash(&key)))
}

fn legacy_pr_cache_path(
    config: &CacheConfig,
    repo: &str,
    pr_number: u64,
    head_sha: &str,
) -> PathBuf {
    cache_dir(config).join(format!(
        "pr-{}.json",
        key_hash(&format!("pr:{repo}:{pr_number}:{head_sha}"))
    ))
}

fn issue_cache_path(
    config: &CacheConfig,
    repo: &str,
    issue_number: u64,
    provider: &str,
    model: &str,
    prompt_version: &str,
    schema_version: &str,
) -> PathBuf {
    let key =
        format!("issue:{repo}:{issue_number}:{provider}:{model}:{prompt_version}:{schema_version}");
    cache_dir(config).join(format!("issue-{}.json", key_hash(&key)))
}

fn legacy_issue_cache_path(config: &CacheConfig, repo: &str, issue_number: u64) -> PathBuf {
    cache_dir(config).join(format!(
        "issue-{}.json",
        key_hash(&format!("issue:{repo}:{issue_number}"))
    ))
}

fn list_cache_path(
    config: &CacheConfig,
    repo: &str,
    kind: &str,
    page: usize,
    extra: &str,
) -> PathBuf {
    let key = format!("list:{kind}:{repo}:{page}:{extra}");
    cache_dir(config).join(format!("list-{}.json", key_hash(&key)))
}

pub fn cache_dir(config: &CacheConfig) -> PathBuf {
    expand_home(&config.dir)
}

fn default_cache_dir() -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(xdg).join("gitnit")
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".cache")
            .join("gitnit")
    }
}

fn expand_home(path: &str) -> PathBuf {
    if path == "~" {
        dirs::home_dir().unwrap_or_else(default_cache_dir)
    } else if let Some(stripped) = path.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(default_cache_dir)
            .join(stripped)
    } else if path.trim().is_empty() {
        default_cache_dir()
    } else {
        PathBuf::from(path)
    }
}

fn analysis_max_age(config: &CacheConfig) -> Option<Duration> {
    if config.analysis_ttl_days == 0 {
        None
    } else {
        Some(Duration::from_secs(
            config.analysis_ttl_days.saturating_mul(86_400),
        ))
    }
}

pub fn prune_cache(config: &CacheConfig) {
    if !config.enabled || config.max_size_mb == 0 {
        return;
    }
    let dir = cache_dir(config);
    let max_bytes = config.max_size_mb.saturating_mul(1024 * 1024);
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut files = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            let modified = metadata.modified().ok();
            Some((path, metadata.len(), modified))
        })
        .collect::<Vec<_>>();
    let mut total = files.iter().map(|(_, len, _)| *len).sum::<u64>();
    if total <= max_bytes {
        return;
    }
    files.sort_by_key(|(_, _, modified)| *modified);
    for (path, len, _) in files {
        if total <= max_bytes {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

fn key_hash(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;

    #[test]
    fn hash_uses_first_16_hex_chars() {
        assert_eq!(key_hash("abc").len(), 16);
    }

    #[test]
    fn disabled_cache_never_returns_analysis() {
        let config = CacheConfig {
            enabled: false,
            dir: temp_cache_dir("disabled"),
            ..CacheConfig::default()
        };
        save_issue_analysis(
            &config,
            "owner/repo",
            1,
            &json!({"overview": "cached"}),
            "gemini",
            "gemini-2.5-pro",
            "v3",
            "issue-analysis-v1",
        );
        assert!(
            get_cached_issue_analysis(
                &config,
                "owner/repo",
                1,
                "gemini",
                "gemini-2.5-pro",
                "v3",
                "issue-analysis-v1",
            )
            .is_none()
        );
    }

    #[test]
    fn stale_analysis_is_ignored() {
        let config = CacheConfig {
            dir: temp_cache_dir("stale"),
            analysis_ttl_days: 1,
            ..CacheConfig::default()
        };
        let path = issue_cache_path(
            &config,
            "owner/repo",
            2,
            "gemini",
            "gemini-2.5-pro",
            "v3",
            "issue-analysis-v1",
        );
        let stale = Utc::now() - TimeDelta::days(2);
        write_json(
            path,
            &json!({
                "overview": "old",
                "_metadata": {
                    "saved_at": stale.to_rfc3339(),
                }
            }),
        )
        .unwrap();
        assert!(
            get_cached_issue_analysis(
                &config,
                "owner/repo",
                2,
                "gemini",
                "gemini-2.5-pro",
                "v3",
                "issue-analysis-v1",
            )
            .is_none()
        );
    }

    fn temp_cache_dir(name: &str) -> String {
        env::temp_dir()
            .join(format!("wftt-cache-test-{}-{name}", std::process::id()))
            .display()
            .to_string()
    }
}
