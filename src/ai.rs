use std::{env, future::Future, pin::Pin};

use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::Client;
use serde_json::{Value, json};

use crate::{
    cache,
    config::CacheConfig,
    models::{IssueAnalysis, IssueDetail, IssueSeverity, PrAnalysis, PrDetail},
};

const PR_SCHEMA_VERSION: &str = "pr-analysis-v1";
const ISSUE_SCHEMA_VERSION: &str = "issue-analysis-v1";

const PR_ANALYSIS_PROMPT: &str = r#"You are GitNit, an expert code reviewer. Analyze the following pull request and provide your assessment.

## Pull Request Information
- **Title:** {title}
- **Author:** {author}
- **PR #{number}**
- **Files changed:** {changed_files}
- **Lines added:** {additions} / **Lines deleted:** {deletions}

## PR Description
{body}

## Changed Files
{files}

## Diff
{diff}

---

Respond with a JSON object containing exactly these fields (no markdown, just raw JSON):

{{
  "summary": "A brief 2-3 sentence summary of what this PR achieves, in plain language.",
  "security_risks": "Assessment of any security implications. If none, say 'No significant security risks identified.'",
  "code_quality": "Brief assessment of code quality, patterns, and maintainability.",
  "risk_level": "One of: Low, Medium, High, Critical",
  "disruption_assessment": "How likely is this PR to break existing functionality? Consider scope of changes, test coverage implied, and areas affected.",
  "backwards_compatibility": "Does this PR break any APIs, configs, or user-facing behavior? Would it require a major semver bump?",
  "semver_impact": "One of: patch, minor, major - based on backwards compatibility analysis.",
  "review_comment": "Write a friendly, professional review comment as if you are the maintainer. Be approachable but technical. Address the author by their username. Start with acknowledgment of the work, then provide specific feedback, and end with next steps or approval suggestion. Do NOT use markdown headers. Use plain paragraphs."
}}
"#;

const ISSUE_ANALYSIS_PROMPT: &str = r#"You are GitNit, an expert at triaging GitHub issues. Analyze the following issue and provide your assessment.

## Issue Information
- **Title:** {title}
- **Author:** {author}
- **Issue #{number}**
- **Labels:** {labels}

## Issue Body
{body}

## Comments
{comments}

---

Respond with a JSON object containing exactly these fields (no markdown, just raw JSON):

{{
  "severity": "One of: Critical, High, Medium, Low, Info",
  "overview": "A clear, simplified explanation of the issue. What is happening, when does it occur, and who is affected? Write for someone who hasn't read the issue.",
  "suspected_cause": "Based on the information provided, what do you believe is the root cause? If unclear, state what investigation would be needed.",
  "suggested_fix": "Describe the fix approach in clear, actionable terms. Write this so someone could copy it and give it to a coding assistant as instructions. Include specific file paths or components if you can infer them. Be concrete about what code changes are needed."
}}
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
}

pub trait RestProvider {
    fn default_model(&self) -> &'static str;
    fn default_base_url(&self) -> &'static str;
    fn default_api_key_env(&self) -> &'static str;
    fn complete<'a>(
        &'a self,
        config: &'a ProviderConfig,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;
}

struct GeminiProvider;
struct OllamaProvider;
struct OpenAiCompatibleProvider {
    default_model: &'static str,
    default_base_url: &'static str,
    default_api_key_env: &'static str,
}

impl RestProvider for GeminiProvider {
    fn default_model(&self) -> &'static str {
        "gemini-2.5-pro"
    }

    fn default_base_url(&self) -> &'static str {
        "https://generativelanguage.googleapis.com/v1beta"
    }

    fn default_api_key_env(&self) -> &'static str {
        "GEMINI_API_KEY"
    }

    fn complete<'a>(
        &'a self,
        config: &'a ProviderConfig,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            let api_key = env::var(&config.api_key_env).with_context(|| {
                format!("{} environment variable is required", config.api_key_env)
            })?;
            let url = format!(
                "{}/models/{}:generateContent?key={}",
                config.base_url.trim_end_matches('/'),
                config.model,
                api_key
            );
            let response: Value = Client::new()
                .post(url)
                .json(&json!({ "contents": [{ "parts": [{ "text": prompt }] }] }))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            Ok(response
                .pointer("/candidates/0/content/parts/0/text")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string())
        })
    }
}

impl RestProvider for OllamaProvider {
    fn default_model(&self) -> &'static str {
        "llama3.1"
    }

    fn default_base_url(&self) -> &'static str {
        "http://localhost:11434"
    }

    fn default_api_key_env(&self) -> &'static str {
        ""
    }

    fn complete<'a>(
        &'a self,
        config: &'a ProviderConfig,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            let url = format!("{}/api/generate", config.base_url.trim_end_matches('/'));
            let response: Value = Client::new()
                .post(url)
                .json(&json!({
                    "model": config.model,
                    "prompt": prompt,
                    "stream": false,
                }))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            Ok(response
                .get("response")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string())
        })
    }
}

impl RestProvider for OpenAiCompatibleProvider {
    fn default_model(&self) -> &'static str {
        self.default_model
    }

    fn default_base_url(&self) -> &'static str {
        self.default_base_url
    }

    fn default_api_key_env(&self) -> &'static str {
        self.default_api_key_env
    }

    fn complete<'a>(
        &'a self,
        config: &'a ProviderConfig,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            let url = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
            let mut request = Client::new().post(url).json(&json!({
                "model": config.model,
                "messages": [
                    {
                        "role": "user",
                        "content": prompt,
                    }
                ],
                "temperature": 0.2,
            }));
            if !config.api_key_env.trim().is_empty() {
                let api_key = env::var(&config.api_key_env).with_context(|| {
                    format!("{} environment variable is required", config.api_key_env)
                })?;
                request = request.bearer_auth(api_key);
            }
            let response: Value = request.send().await?.error_for_status()?.json().await?;
            Ok(response
                .pointer("/choices/0/message/content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string())
        })
    }
}

pub fn resolve_provider_config(
    provider: &str,
    model: &str,
    base_url: &str,
    api_key_env: &str,
) -> ProviderConfig {
    let provider = provider.trim();
    let model = model.trim();
    let (provider, model) = match model.split_once('/') {
        Some((namespace, model_name))
            if !namespace.trim().is_empty() && !model_name.trim().is_empty() =>
        {
            (
                namespace.trim().to_lowercase(),
                model_name.trim().to_string(),
            )
        }
        _ => (
            if provider.is_empty() {
                "gemini"
            } else {
                provider
            }
            .to_lowercase(),
            model.to_string(),
        ),
    };

    let provider_impl = provider_by_name(&provider);
    let default_model = provider_impl
        .as_ref()
        .map(|p| p.default_model())
        .unwrap_or("gpt-4o-mini");
    let default_base_url = provider_impl
        .as_ref()
        .map(|p| p.default_base_url())
        .unwrap_or("");
    let default_api_key_env = provider_impl
        .as_ref()
        .map(|p| p.default_api_key_env())
        .unwrap_or("");

    ProviderConfig {
        provider,
        model: if model.is_empty() {
            default_model.to_string()
        } else {
            model
        },
        base_url: if base_url.trim().is_empty() {
            default_base_url.to_string()
        } else {
            base_url.trim().to_string()
        },
        api_key_env: if api_key_env.trim().is_empty() {
            default_api_key_env.to_string()
        } else {
            api_key_env.trim().to_string()
        },
    }
}

pub async fn analyze_pr(
    pr_detail: &PrDetail,
    provider: &str,
    model: &str,
    base_url: &str,
    api_key_env: &str,
    cache_config: &CacheConfig,
    repo: &str,
    prompt_version: &str,
) -> PrAnalysis {
    let config = resolve_provider_config(provider, model, base_url, api_key_env);
    if !repo.is_empty() && !pr_detail.pr.head_sha.is_empty() {
        if let Some(cached) = cache::get_cached_pr_analysis(
            cache_config,
            repo,
            pr_detail.pr.number,
            &pr_detail.pr.head_sha,
            &config.provider,
            &config.model,
            prompt_version,
            PR_SCHEMA_VERSION,
        ) {
            return pr_analysis_from_value(&cached);
        }
    }

    let prompt = pr_prompt(pr_detail);
    let analysis = match generate(&config, &prompt).await {
        Ok(text) => extract_json(&text).map_or_else(
            || PrAnalysis {
                summary: if text.is_empty() {
                    "No analysis generated.".to_string()
                } else {
                    text.chars().take(500).collect()
                },
                review_comment: if text.is_empty() {
                    "No review generated.".to_string()
                } else {
                    text
                },
                ..PrAnalysis::default()
            },
            |data| pr_analysis_from_value(&data),
        ),
        Err(err) => PrAnalysis {
            summary: format!("{} analysis failed: {err}", config.provider),
            review_comment: format!(
                "Unable to generate review comment with provider '{}'.",
                config.provider
            ),
            ..PrAnalysis::default()
        },
    };

    if !repo.is_empty() && !pr_detail.pr.head_sha.is_empty() {
        cache::save_pr_analysis(
            cache_config,
            repo,
            pr_detail.pr.number,
            &pr_detail.pr.head_sha,
            &serde_json::to_value(&analysis).unwrap_or_else(|_| json!({})),
            &config.provider,
            &config.model,
            prompt_version,
            PR_SCHEMA_VERSION,
        );
    }
    analysis
}

pub async fn analyze_issue(
    issue_detail: &IssueDetail,
    provider: &str,
    model: &str,
    base_url: &str,
    api_key_env: &str,
    cache_config: &CacheConfig,
    repo: &str,
    prompt_version: &str,
) -> IssueAnalysis {
    let config = resolve_provider_config(provider, model, base_url, api_key_env);
    if !repo.is_empty() {
        if let Some(cached) = cache::get_cached_issue_analysis(
            cache_config,
            repo,
            issue_detail.issue.number,
            &config.provider,
            &config.model,
            prompt_version,
            ISSUE_SCHEMA_VERSION,
        ) {
            return issue_analysis_from_value(&cached);
        }
    }

    let prompt = issue_prompt(issue_detail);
    let analysis = match generate(&config, &prompt).await {
        Ok(text) => extract_json(&text).map_or_else(
            || IssueAnalysis {
                overview: if text.is_empty() {
                    "No analysis generated.".to_string()
                } else {
                    text.chars().take(500).collect()
                },
                suggested_fix: "No fix suggestion could be extracted from the response."
                    .to_string(),
                ..IssueAnalysis::default()
            },
            |data| issue_analysis_from_value(&data),
        ),
        Err(err) => IssueAnalysis {
            overview: format!("{} analysis failed: {err}", config.provider),
            suggested_fix: format!(
                "Unable to generate fix suggestion with provider '{}'.",
                config.provider
            ),
            ..IssueAnalysis::default()
        },
    };

    if !repo.is_empty() {
        cache::save_issue_analysis(
            cache_config,
            repo,
            issue_detail.issue.number,
            &serde_json::to_value(&analysis).unwrap_or_else(|_| json!({})),
            &config.provider,
            &config.model,
            prompt_version,
            ISSUE_SCHEMA_VERSION,
        );
    }
    analysis
}

async fn generate(config: &ProviderConfig, prompt: &str) -> Result<String> {
    let Some(provider) = provider_by_name(&config.provider) else {
        bail!("provider '{}' is not implemented", config.provider);
    };
    provider.complete(config, prompt).await
}

fn provider_by_name(provider: &str) -> Option<Box<dyn RestProvider + Send + Sync>> {
    match provider {
        "gemini" => Some(Box::new(GeminiProvider)),
        "ollama" => Some(Box::new(OllamaProvider)),
        "openai" | "openai-compatible" => Some(Box::new(OpenAiCompatibleProvider {
            default_model: "gpt-4o-mini",
            default_base_url: "https://api.openai.com/v1",
            default_api_key_env: "OPENAI_API_KEY",
        })),
        "openrouter" => Some(Box::new(OpenAiCompatibleProvider {
            default_model: "openai/gpt-4o-mini",
            default_base_url: "https://openrouter.ai/api/v1",
            default_api_key_env: "OPENROUTER_API_KEY",
        })),
        "lmstudio" => Some(Box::new(OpenAiCompatibleProvider {
            default_model: "local-model",
            default_base_url: "http://localhost:1234/v1",
            default_api_key_env: "",
        })),
        "vllm" => Some(Box::new(OpenAiCompatibleProvider {
            default_model: "local-model",
            default_base_url: "http://localhost:8000/v1",
            default_api_key_env: "",
        })),
        _ => None,
    }
}

fn pr_prompt(pr_detail: &PrDetail) -> String {
    let diff = if pr_detail.diff.len() > 15_000 {
        &pr_detail.diff[..15_000]
    } else {
        &pr_detail.diff
    };
    PR_ANALYSIS_PROMPT
        .replace("{title}", &pr_detail.pr.title)
        .replace("{author}", &pr_detail.pr.author)
        .replace("{number}", &pr_detail.pr.number.to_string())
        .replace("{changed_files}", &pr_detail.pr.changed_files.to_string())
        .replace("{additions}", &pr_detail.pr.additions.to_string())
        .replace("{deletions}", &pr_detail.pr.deletions.to_string())
        .replace("{body}", empty_as(&pr_detail.pr.body, "(no description)"))
        .replace(
            "{files}",
            &pr_detail
                .files
                .iter()
                .map(|f| format!("- {f}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .replace("{diff}", diff)
}

fn issue_prompt(issue_detail: &IssueDetail) -> String {
    let comments = if issue_detail.comments.is_empty() {
        "(no comments)".to_string()
    } else {
        issue_detail
            .comments
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    ISSUE_ANALYSIS_PROMPT
        .replace("{title}", &issue_detail.issue.title)
        .replace("{author}", &issue_detail.issue.author)
        .replace("{number}", &issue_detail.issue.number.to_string())
        .replace("{labels}", empty_as(&issue_detail.issue.label_raw, "none"))
        .replace(
            "{body}",
            empty_as(&issue_detail.issue.body, "(no description)"),
        )
        .replace("{comments}", &comments)
}

pub fn extract_json(text: &str) -> Option<Value> {
    let mut trimmed = text.trim().to_string();
    if trimmed.starts_with("```") {
        let fence = Regex::new(r"^```\w*\n?|\n?```$").ok()?;
        trimmed = fence.replace_all(&trimmed, "").trim().to_string();
    }
    serde_json::from_str(&trimmed).ok().or_else(|| {
        let re = Regex::new(r"\{[\s\S]*\}").ok()?;
        let matched = re.find(&trimmed)?;
        serde_json::from_str(matched.as_str()).ok()
    })
}

pub fn pr_analysis_from_value(data: &Value) -> PrAnalysis {
    PrAnalysis {
        summary: string(data, "summary"),
        security_risks: string(data, "security_risks"),
        code_quality: string(data, "code_quality"),
        risk_level: string_or(data, "risk_level", "Unknown"),
        disruption_assessment: string(data, "disruption_assessment"),
        backwards_compatibility: string(data, "backwards_compatibility"),
        semver_impact: string(data, "semver_impact"),
        review_comment: first_non_empty(
            data,
            &[
                "review_comment",
                "suggested_comment",
                "comment",
                "review",
                "maintainer_comment",
            ],
        ),
    }
}

pub fn issue_analysis_from_value(data: &Value) -> IssueAnalysis {
    let severity = match string(data, "severity").to_lowercase().as_str() {
        "critical" => IssueSeverity::Critical,
        "high" => IssueSeverity::High,
        "medium" => IssueSeverity::Medium,
        "low" => IssueSeverity::Low,
        _ => IssueSeverity::Info,
    };
    IssueAnalysis {
        severity,
        overview: string(data, "overview"),
        suspected_cause: string(data, "suspected_cause"),
        suggested_fix: string(data, "suggested_fix"),
    }
}

fn empty_as<'a>(value: &'a str, default: &'a str) -> &'a str {
    if value.trim().is_empty() {
        default
    } else {
        value
    }
}

fn string(data: &Value, key: &str) -> String {
    data.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn string_or(data: &Value, key: &str, default: &str) -> String {
    data.get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_string()
}

fn first_non_empty(data: &Value, keys: &[&str]) -> String {
    keys.iter()
        .filter_map(|key| data.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|value| !value.is_empty())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_from_markdown_fence() {
        let value = extract_json("```json\n{\"summary\":\"ok\"}\n```").unwrap();
        assert_eq!(value["summary"], "ok");
    }

    #[test]
    fn uses_comment_aliases() {
        let analysis = pr_analysis_from_value(&json!({"comment": "ship it"}));
        assert_eq!(analysis.review_comment, "ship it");
    }

    #[test]
    fn resolves_explicit_provider_and_model() {
        let config = resolve_provider_config("gemini", "gemini-2.5-pro", "", "");
        assert_eq!(config.provider, "gemini");
        assert_eq!(config.model, "gemini-2.5-pro");
        assert_eq!(config.api_key_env, "GEMINI_API_KEY");
    }

    #[test]
    fn resolves_provider_from_namespaced_model() {
        let config = resolve_provider_config("", "ollama/llama3.1", "", "");
        assert_eq!(config.provider, "ollama");
        assert_eq!(config.model, "llama3.1");
        assert_eq!(config.base_url, "http://localhost:11434");
    }

    #[test]
    fn namespaced_model_overrides_provider() {
        let config = resolve_provider_config("gemini", "ollama/qwen2.5-coder", "", "");
        assert_eq!(config.provider, "ollama");
        assert_eq!(config.model, "qwen2.5-coder");
    }

    #[test]
    fn resolves_openrouter_namespaced_model() {
        let config = resolve_provider_config("", "openrouter/anthropic/claude-3.5-sonnet", "", "");
        assert_eq!(config.provider, "openrouter");
        assert_eq!(config.model, "anthropic/claude-3.5-sonnet");
        assert_eq!(config.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(config.api_key_env, "OPENROUTER_API_KEY");
    }

    #[test]
    fn resolves_lmstudio_defaults() {
        let config = resolve_provider_config("lmstudio", "", "", "");
        assert_eq!(config.provider, "lmstudio");
        assert_eq!(config.base_url, "http://localhost:1234/v1");
        assert_eq!(config.api_key_env, "");
    }
}
