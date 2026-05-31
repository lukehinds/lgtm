use std::{
    env, fs,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
};

use anyhow::{Context, Result, bail};
use regex::Regex;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::{
    cache,
    config::{CacheConfig, ReviewConfig},
    models::{IssueAnalysis, IssueDetail, IssueSeverity, PrAnalysis, PrDetail},
};

const PR_SCHEMA_VERSION: &str = "pr-analysis-v1";
const ISSUE_SCHEMA_VERSION: &str = "issue-analysis-v1";
const AGENTIC_PR_SCHEMA_VERSION: &str = "pr-analysis-agentic-v3";

const PR_ANALYSIS_PROMPT: &str = r#"You are lgtm, an expert code reviewer. Analyze the following pull request and provide your assessment.

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

const ISSUE_ANALYSIS_PROMPT: &str = r#"You are lgtm, an expert at triaging GitHub issues. Analyze the following issue and provide your assessment.

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

struct ReviewToolContext<'a> {
    root: PathBuf,
    pr_detail: &'a PrDetail,
    max_output_bytes: usize,
}

enum ReviewStep {
    Tool { name: String, args: Value },
    Final(PrAnalysis),
    Invalid(String),
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
    review_config: &ReviewConfig,
    progress_tx: Option<UnboundedSender<String>>,
) -> PrAnalysis {
    let config = resolve_provider_config(provider, model, base_url, api_key_env);
    let tool_context = review_tool_context(review_config, pr_detail);
    let schema_version = if tool_context.is_some() {
        AGENTIC_PR_SCHEMA_VERSION
    } else {
        PR_SCHEMA_VERSION
    };
    let cache_prompt_version = if tool_context.is_some() {
        format!("{prompt_version}:repo-context")
    } else {
        prompt_version.to_string()
    };
    if !repo.is_empty() && !pr_detail.pr.head_sha.is_empty() {
        if let Some(cached) = cache::get_cached_pr_analysis(
            cache_config,
            repo,
            pr_detail.pr.number,
            &pr_detail.pr.head_sha,
            &config.provider,
            &config.model,
            &cache_prompt_version,
            schema_version,
        ) {
            return pr_analysis_from_value(&cached);
        }
    }

    let analysis = if let Some(tool_context) = tool_context {
        analyze_pr_with_tools(
            &config,
            pr_detail,
            &tool_context,
            review_config.min_tool_calls,
            review_config.max_tool_calls,
            progress_tx,
        )
        .await
    } else {
        analyze_pr_diff_only(&config, pr_detail).await
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
            &cache_prompt_version,
            schema_version,
        );
    }
    analysis
}

async fn analyze_pr_diff_only(config: &ProviderConfig, pr_detail: &PrDetail) -> PrAnalysis {
    let prompt = pr_prompt(pr_detail);
    match generate(config, &prompt).await {
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
    }
}

async fn analyze_pr_with_tools(
    config: &ProviderConfig,
    pr_detail: &PrDetail,
    tool_context: &ReviewToolContext<'_>,
    min_tool_calls: usize,
    max_tool_calls: usize,
    progress_tx: Option<UnboundedSender<String>>,
) -> PrAnalysis {
    let mut transcript = String::new();
    let min_tool_calls = min_tool_calls.min(max_tool_calls);
    let mut tool_calls = 0usize;
    for turn in 0..=max_tool_calls {
        if let Some(progress_tx) = &progress_tx {
            let status = if tool_calls >= min_tool_calls {
                "Repo context: reviewing gathered context..."
            } else {
                "Repo context: gathering required context..."
            };
            let _ = progress_tx.send(format!("status:{status}"));
            let _ = progress_tx.send(format!(
                "log:Model turn {}: {} ({}/{})",
                turn + 1,
                if tool_calls > 0 {
                    "reviewing tool results"
                } else {
                    "choosing first repo tool"
                },
                tool_calls,
                min_tool_calls
            ));
        }
        let prompt = agentic_pr_prompt(
            pr_detail,
            &transcript,
            max_tool_calls - turn,
            min_tool_calls,
            tool_calls,
        );
        let text = match generate(config, &prompt).await {
            Ok(text) => text,
            Err(err) => {
                return PrAnalysis {
                    summary: format!("{} repo-context analysis failed: {err}", config.provider),
                    review_comment: format!(
                        "Unable to generate repo-context review with provider '{}'.",
                        config.provider
                    ),
                    ..PrAnalysis::default()
                };
            }
        };
        match parse_review_step(&text) {
            ReviewStep::Final(analysis) if tool_calls >= min_tool_calls || max_tool_calls == 0 => {
                return analysis;
            }
            ReviewStep::Final(_) => {
                if let Some(progress_tx) = &progress_tx {
                    let _ = progress_tx.send(format!(
                        "log:Model tried to finalize after {tool_calls}/{min_tool_calls} tool calls; requesting more context"
                    ));
                }
                transcript.push_str(&format!(
                    "\n\nYou returned final review JSON after {tool_calls} tool calls, but this review requires at least {min_tool_calls}. \
Request another read-only repo tool now, then produce the final review after enough context has been gathered.\n",
                ));
            }
            ReviewStep::Tool { name, args } if turn < max_tool_calls => {
                if let Some(progress_tx) = &progress_tx {
                    let summary = format!("{}{}", name, tool_arg_summary(&name, &args));
                    let _ = progress_tx.send(format!("status:Using repo tool: {summary}"));
                    let _ = progress_tx.send(format!("log:Tool requested: {summary}"));
                }
                let result = run_review_tool(tool_context, &name, &args);
                if let Some(progress_tx) = &progress_tx {
                    let _ = progress_tx.send(format!(
                        "log:Tool completed: {} ({} bytes)",
                        name,
                        result.len()
                    ));
                }
                tool_calls += 1;
                transcript.push_str(&format!(
                    "\n\nAssistant requested tool `{name}` with args:\n{}\n\nTool result:\n{}\n",
                    compact_json(&args),
                    result
                ));
            }
            ReviewStep::Tool { .. } => {
                transcript.push_str("\n\nTool budget exhausted. Provide final review JSON now.\n");
            }
            ReviewStep::Invalid(raw) => {
                return PrAnalysis {
                    summary: "Repo-context analysis did not return valid review JSON.".to_string(),
                    review_comment: raw.chars().take(2_000).collect(),
                    ..PrAnalysis::default()
                };
            }
        }
    }
    PrAnalysis {
        summary: "Repo-context analysis exhausted its tool budget before finalizing.".to_string(),
        review_comment: "No final review was generated.".to_string(),
        ..PrAnalysis::default()
    }
}

fn review_tool_context<'a>(
    config: &ReviewConfig,
    pr_detail: &'a PrDetail,
) -> Option<ReviewToolContext<'a>> {
    if !config.enabled || config.max_tool_calls == 0 {
        return None;
    }
    let root = PathBuf::from(&config.repo_path).canonicalize().ok()?;
    root.is_dir().then_some(ReviewToolContext {
        root,
        pr_detail,
        max_output_bytes: config.max_tool_output_bytes.max(1_000),
    })
}

fn parse_review_step(text: &str) -> ReviewStep {
    let Some(data) = extract_json(text) else {
        return ReviewStep::Invalid(text.to_string());
    };
    if let Some(tool) = data.get("tool").and_then(Value::as_str) {
        return ReviewStep::Tool {
            name: tool.to_string(),
            args: data.get("args").cloned().unwrap_or_else(|| json!({})),
        };
    }
    if let Some(request) = data.get("tool_request").and_then(Value::as_object) {
        if let Some(tool) = request.get("tool").and_then(Value::as_str) {
            return ReviewStep::Tool {
                name: tool.to_string(),
                args: request.get("args").cloned().unwrap_or_else(|| json!({})),
            };
        }
    }
    ReviewStep::Final(pr_analysis_from_value(&data))
}

fn run_review_tool(context: &ReviewToolContext<'_>, name: &str, args: &Value) -> String {
    let result = match name {
        "changed_files" => tool_changed_files(context),
        "diff_for_file" => {
            let path = string_arg(args, "path");
            tool_diff_for_file(context, &path)
        }
        "list_dir" => {
            let path = string_arg_or(args, "path", ".");
            tool_list_dir(context, &path)
        }
        "read_file" => {
            let path = string_arg(args, "path");
            let start = usize_arg_or(args, "start", 1);
            let lines = usize_arg_or(args, "lines", 120).min(300);
            tool_read_file(context, &path, start, lines)
        }
        "grep" => {
            let pattern = string_arg(args, "pattern");
            let path = string_arg_or(args, "path", ".");
            tool_grep(context, &pattern, &path)
        }
        _ => format!("Unknown tool `{name}`."),
    };
    truncate_bytes(&result, context.max_output_bytes)
}

fn tool_arg_summary(name: &str, args: &Value) -> String {
    match name {
        "read_file" | "diff_for_file" | "list_dir" => {
            let path = string_arg(args, "path");
            if path.is_empty() {
                String::new()
            } else {
                format!(" {path}")
            }
        }
        "grep" => {
            let pattern = string_arg(args, "pattern");
            let path = string_arg_or(args, "path", ".");
            if pattern.is_empty() {
                format!(" {path}")
            } else {
                format!(" {pattern} in {path}")
            }
        }
        _ => String::new(),
    }
}

fn tool_changed_files(context: &ReviewToolContext<'_>) -> String {
    context
        .pr_detail
        .files
        .iter()
        .map(|file| format!("- {file}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn tool_diff_for_file(context: &ReviewToolContext<'_>, path: &str) -> String {
    if path.trim().is_empty() {
        return "Missing path.".to_string();
    }
    let mut out = Vec::new();
    let mut capture = false;
    for line in context.pr_detail.diff.lines() {
        if let Some(header) = line.strip_prefix("--- ") {
            capture = header.trim() == path.trim();
        }
        if line.starts_with("diff --git ") {
            capture = line.contains(path.trim());
        }
        if capture {
            out.push(line.to_string());
        }
    }
    if out.is_empty() {
        format!("No diff found for `{path}`.")
    } else {
        out.join("\n")
    }
}

fn tool_list_dir(context: &ReviewToolContext<'_>, path: &str) -> String {
    let Ok(path) = safe_path(&context.root, path) else {
        return "Path is outside repo or does not exist.".to_string();
    };
    let Ok(entries) = fs::read_dir(&path) else {
        return format!(
            "Could not list `{}`.",
            display_repo_path(&context.root, &path)
        );
    };
    let mut rows = Vec::new();
    for entry in entries.flatten().take(200) {
        let file_type = entry.file_type().ok();
        let kind = if file_type.as_ref().is_some_and(|t| t.is_dir()) {
            "dir "
        } else {
            "file"
        };
        rows.push(format!("{kind} {}", entry.file_name().to_string_lossy()));
    }
    rows.sort();
    rows.join("\n")
}

fn tool_read_file(
    context: &ReviewToolContext<'_>,
    path: &str,
    start: usize,
    lines: usize,
) -> String {
    let Ok(path) = safe_path(&context.root, path) else {
        return "Path is outside repo or does not exist.".to_string();
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return format!(
            "Could not read `{}` as UTF-8.",
            display_repo_path(&context.root, &path)
        );
    };
    let start = start.max(1);
    let selected = content
        .lines()
        .enumerate()
        .skip(start - 1)
        .take(lines)
        .map(|(index, line)| format!("{:>5} | {line}", index + 1))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{}\n{}", display_repo_path(&context.root, &path), selected)
}

fn tool_grep(context: &ReviewToolContext<'_>, pattern: &str, path: &str) -> String {
    if pattern.trim().is_empty() {
        return "Missing pattern.".to_string();
    }
    let Ok(path) = safe_path(&context.root, path) else {
        return "Path is outside repo or does not exist.".to_string();
    };
    let regex = Regex::new(pattern).ok();
    let mut matches = Vec::new();
    grep_path(
        &context.root,
        &path,
        pattern,
        regex.as_ref(),
        &mut matches,
        120,
    );
    if matches.is_empty() {
        format!("No matches for `{pattern}`.")
    } else {
        matches.join("\n")
    }
}

fn grep_path(
    root: &Path,
    path: &Path,
    pattern: &str,
    regex: Option<&Regex>,
    matches: &mut Vec<String>,
    max_matches: usize,
) {
    if matches.len() >= max_matches || ignored_path(path) {
        return;
    }
    if path.is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            grep_path(root, &entry.path(), pattern, regex, matches, max_matches);
            if matches.len() >= max_matches {
                break;
            }
        }
        return;
    }
    let Ok(metadata) = fs::metadata(path) else {
        return;
    };
    if metadata.len() > 1_000_000 {
        return;
    }
    let Ok(content) = fs::read_to_string(path) else {
        return;
    };
    for (index, line) in content.lines().enumerate() {
        let matched = regex
            .map(|regex| regex.is_match(line))
            .unwrap_or_else(|| line.contains(pattern));
        if matched {
            matches.push(format!(
                "{}:{}: {}",
                display_repo_path(root, path),
                index + 1,
                line.trim_end()
            ));
            if matches.len() >= max_matches {
                break;
            }
        }
    }
}

fn safe_path(root: &Path, path: &str) -> std::result::Result<PathBuf, ()> {
    let rel = path.trim();
    let joined = if rel.is_empty() || rel == "." {
        root.to_path_buf()
    } else {
        if Path::new(rel).is_absolute() {
            return Err(());
        }
        root.join(rel)
    };
    let canonical = joined.canonicalize().map_err(|_| ())?;
    canonical.starts_with(root).then_some(canonical).ok_or(())
}

fn ignored_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, ".git" | "target" | "node_modules" | ".next" | "dist"))
}

fn display_repo_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn string_arg(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn string_arg_or(args: &Value, key: &str, default: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_string()
}

fn usize_arg_or(args: &Value, key: &str, default: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(default)
}

fn truncate_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated to {max_bytes} bytes]", &value[..end])
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string())
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

fn agentic_pr_prompt(
    pr_detail: &PrDetail,
    transcript: &str,
    remaining_tool_calls: usize,
    min_tool_calls: usize,
    completed_tool_calls: usize,
) -> String {
    let diff = if pr_detail.diff.len() > 15_000 {
        &pr_detail.diff[..15_000]
    } else {
        &pr_detail.diff
    };
    let files = pr_detail
        .files
        .iter()
        .map(|f| format!("- {f}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"You are lgtm, an expert code reviewer with read-only repository tools.

Review this pull request. You must use at least {min_tool_calls} read-only repository tool calls before producing the final review when tool calls remain.
Use tools to inspect context needed to assess correctness, compatibility, call sites, tests, or surrounding code.

Available tools. Return exactly one JSON object when requesting a tool:
{{"tool":"changed_files","args":{{}}}}
{{"tool":"diff_for_file","args":{{"path":"src/lib.rs"}}}}
{{"tool":"list_dir","args":{{"path":"src"}}}}
{{"tool":"read_file","args":{{"path":"src/lib.rs","start":1,"lines":120}}}}
{{"tool":"grep","args":{{"pattern":"symbol_or_regex","path":"src"}}}}

Do not request write, shell, network, or mutation tools.
Completed required tool calls: {completed_tool_calls}/{min_tool_calls}. Tool calls remaining: {remaining_tool_calls}.
If completed tool calls are below the required minimum and tool calls remain, your next response must be a tool request JSON object, not final review JSON.

When ready, return exactly this final JSON object and no markdown:
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

## Pull Request Information
- Title: {title}
- Author: {author}
- PR #{number}
- Files changed: {changed_files}
- Lines added/deleted: +{additions} / -{deletions}

## PR Description
{body}

## Changed Files
{files}

## Diff
{diff}

## Prior Tool Transcript
{transcript}
"#,
        title = pr_detail.pr.title,
        author = pr_detail.pr.author,
        number = pr_detail.pr.number,
        changed_files = pr_detail.pr.changed_files,
        additions = pr_detail.pr.additions,
        deletions = pr_detail.pr.deletions,
        body = empty_as(&pr_detail.pr.body, "(no description)"),
        files = files,
        diff = diff,
        transcript = if transcript.trim().is_empty() {
            "(none)"
        } else {
            transcript
        },
        min_tool_calls = min_tool_calls,
        completed_tool_calls = completed_tool_calls,
        remaining_tool_calls = remaining_tool_calls,
    )
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
    fn parses_tool_request_step() {
        match parse_review_step(r#"{"tool":"read_file","args":{"path":"src/lib.rs"}}"#) {
            ReviewStep::Tool { name, args } => {
                assert_eq!(name, "read_file");
                assert_eq!(args["path"], "src/lib.rs");
            }
            _ => panic!("expected tool request"),
        }
    }

    #[test]
    fn parses_final_review_step() {
        match parse_review_step(r#"{"summary":"ok","risk_level":"Low"}"#) {
            ReviewStep::Final(analysis) => {
                assert_eq!(analysis.summary, "ok");
                assert_eq!(analysis.risk_level, "Low");
            }
            _ => panic!("expected final review"),
        }
    }

    #[test]
    fn truncates_tool_output_on_char_boundary() {
        let value = "abcédef";
        let truncated = truncate_bytes(value, 4);
        assert!(truncated.starts_with("abc"));
        assert!(truncated.contains("[truncated"));
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
