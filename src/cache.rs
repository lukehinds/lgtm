use std::{env, fs, path::PathBuf, time::Duration};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::models::{IssueData, PrData};

const DEFAULT_PROVIDER: &str = "claude-code";
const DEFAULT_MODEL: &str = "sonnet";
const DEFAULT_PROMPT_VERSION: &str = "v3";
const DEFAULT_PR_SCHEMA_VERSION: &str = "pr-analysis-v1";
const DEFAULT_ISSUE_SCHEMA_VERSION: &str = "issue-analysis-v1";

pub fn get_cached_pr_analysis(
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    provider: &str,
    model: &str,
    prompt_version: &str,
    schema_version: &str,
) -> Option<Value> {
    let path = pr_cache_path(
        repo,
        pr_number,
        head_sha,
        provider,
        model,
        prompt_version,
        schema_version,
    );
    read_json(&path).or_else(|| {
        if provider == DEFAULT_PROVIDER
            && model == DEFAULT_MODEL
            && prompt_version == DEFAULT_PROMPT_VERSION
            && schema_version == DEFAULT_PR_SCHEMA_VERSION
        {
            read_json(&legacy_pr_cache_path(repo, pr_number, head_sha))
        } else {
            None
        }
    })
}

pub fn save_pr_analysis(
    repo: &str,
    pr_number: u64,
    head_sha: &str,
    data: &Value,
    provider: &str,
    model: &str,
    prompt_version: &str,
    schema_version: &str,
) {
    let mut payload = data.as_object().cloned().unwrap_or_default();
    payload.insert(
        "_metadata".to_string(),
        metadata(provider, model, prompt_version, schema_version),
    );
    let _ = write_json(
        pr_cache_path(
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
}

pub fn get_cached_issue_analysis(
    repo: &str,
    issue_number: u64,
    provider: &str,
    model: &str,
    prompt_version: &str,
    schema_version: &str,
) -> Option<Value> {
    let path = issue_cache_path(
        repo,
        issue_number,
        provider,
        model,
        prompt_version,
        schema_version,
    );
    read_json(&path).or_else(|| {
        if provider == DEFAULT_PROVIDER
            && model == DEFAULT_MODEL
            && prompt_version == DEFAULT_PROMPT_VERSION
            && schema_version == DEFAULT_ISSUE_SCHEMA_VERSION
        {
            read_json(&legacy_issue_cache_path(repo, issue_number))
        } else {
            None
        }
    })
}

pub fn save_issue_analysis(
    repo: &str,
    issue_number: u64,
    data: &Value,
    provider: &str,
    model: &str,
    prompt_version: &str,
    schema_version: &str,
) {
    let mut payload = data.as_object().cloned().unwrap_or_default();
    payload.insert(
        "_metadata".to_string(),
        metadata(provider, model, prompt_version, schema_version),
    );
    let _ = write_json(
        issue_cache_path(
            repo,
            issue_number,
            provider,
            model,
            prompt_version,
            schema_version,
        ),
        &Value::Object(payload),
    );
}

pub fn get_cached_pr_list(
    repo: &str,
    page: usize,
    max_age: Option<Duration>,
) -> Option<(Vec<PrData>, u64)> {
    read_fresh_list(list_cache_path(repo, "prs", page, ""), max_age)
}

pub fn save_pr_list(repo: &str, page: usize, prs: &[PrData], total: u64) {
    let _ = write_list(list_cache_path(repo, "prs", page, ""), prs, total);
}

pub fn get_cached_issue_list(
    repo: &str,
    page: usize,
    direction: &str,
    max_age: Option<Duration>,
) -> Option<(Vec<IssueData>, u64)> {
    read_fresh_list(list_cache_path(repo, "issues", page, direction), max_age)
}

pub fn save_issue_list(repo: &str, page: usize, issues: &[IssueData], total: u64, direction: &str) {
    let _ = write_list(
        list_cache_path(repo, "issues", page, direction),
        issues,
        total,
    );
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
    cache_dir().join(format!("pr-{}.json", key_hash(&key)))
}

fn legacy_pr_cache_path(repo: &str, pr_number: u64, head_sha: &str) -> PathBuf {
    cache_dir().join(format!(
        "pr-{}.json",
        key_hash(&format!("pr:{repo}:{pr_number}:{head_sha}"))
    ))
}

fn issue_cache_path(
    repo: &str,
    issue_number: u64,
    provider: &str,
    model: &str,
    prompt_version: &str,
    schema_version: &str,
) -> PathBuf {
    let key =
        format!("issue:{repo}:{issue_number}:{provider}:{model}:{prompt_version}:{schema_version}");
    cache_dir().join(format!("issue-{}.json", key_hash(&key)))
}

fn legacy_issue_cache_path(repo: &str, issue_number: u64) -> PathBuf {
    cache_dir().join(format!(
        "issue-{}.json",
        key_hash(&format!("issue:{repo}:{issue_number}"))
    ))
}

fn list_cache_path(repo: &str, kind: &str, page: usize, extra: &str) -> PathBuf {
    let key = format!("list:{kind}:{repo}:{page}:{extra}");
    cache_dir().join(format!("list-{}.json", key_hash(&key)))
}

fn cache_dir() -> PathBuf {
    if let Some(xdg) = env::var_os("XDG_CACHE_HOME") {
        PathBuf::from(xdg).join("gitnit")
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".cache")
            .join("gitnit")
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

    #[test]
    fn hash_uses_first_16_hex_chars() {
        assert_eq!(key_hash("abc").len(), 16);
    }
}
