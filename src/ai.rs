use std::env;

use anyhow::{Context, Result};
use regex::Regex;
use reqwest::Client;
use serde_json::{Value, json};

use crate::{
    cache,
    models::{IssueAnalysis, IssueDetail, IssueSeverity, PrAnalysis, PrDetail},
};

const DEFAULT_MODEL: &str = "sonnet";
const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-pro";
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

pub async fn analyze_pr(
    pr_detail: &PrDetail,
    provider: &str,
    model: &str,
    repo: &str,
    prompt_version: &str,
) -> PrAnalysis {
    let model = normalize_model(provider, model);
    if !repo.is_empty() && !pr_detail.pr.head_sha.is_empty() {
        if let Some(cached) = cache::get_cached_pr_analysis(
            repo,
            pr_detail.pr.number,
            &pr_detail.pr.head_sha,
            provider,
            &model,
            prompt_version,
            PR_SCHEMA_VERSION,
        ) {
            return pr_analysis_from_value(&cached);
        }
    }

    let analysis = match provider {
        "gemini" => analyze_pr_with_gemini(pr_detail, &model).await,
        "claude-code" | "claude" => PrAnalysis {
            summary: "The Rust port does not implement the Python-only claude-agent-sdk provider.".to_string(),
            review_comment: "Unable to generate review comment with claude-code in this Rust build. Use --provider gemini with GEMINI_API_KEY.".to_string(),
            ..PrAnalysis::default()
        },
        other => PrAnalysis {
            summary: format!("AI provider '{other}' is not implemented yet."),
            review_comment: format!("Unable to generate review comment with provider '{other}'."),
            ..PrAnalysis::default()
        },
    };

    if !repo.is_empty() && !pr_detail.pr.head_sha.is_empty() {
        cache::save_pr_analysis(
            repo,
            pr_detail.pr.number,
            &pr_detail.pr.head_sha,
            &serde_json::to_value(&analysis).unwrap_or_else(|_| json!({})),
            provider,
            &model,
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
    repo: &str,
    prompt_version: &str,
) -> IssueAnalysis {
    let model = normalize_model(provider, model);
    if !repo.is_empty() {
        if let Some(cached) = cache::get_cached_issue_analysis(
            repo,
            issue_detail.issue.number,
            provider,
            &model,
            prompt_version,
            ISSUE_SCHEMA_VERSION,
        ) {
            return issue_analysis_from_value(&cached);
        }
    }

    let analysis = match provider {
        "gemini" => analyze_issue_with_gemini(issue_detail, &model).await,
        "claude-code" | "claude" => IssueAnalysis {
            overview: "The Rust port does not implement the Python-only claude-agent-sdk provider.".to_string(),
            suggested_fix: "Use --provider gemini with GEMINI_API_KEY for AI issue analysis in this Rust build.".to_string(),
            ..IssueAnalysis::default()
        },
        other => IssueAnalysis {
            overview: format!("AI provider '{other}' is not implemented yet."),
            suggested_fix: format!("Unable to generate fix suggestion with provider '{other}'."),
            ..IssueAnalysis::default()
        },
    };

    if !repo.is_empty() {
        cache::save_issue_analysis(
            repo,
            issue_detail.issue.number,
            &serde_json::to_value(&analysis).unwrap_or_else(|_| json!({})),
            provider,
            &model,
            prompt_version,
            ISSUE_SCHEMA_VERSION,
        );
    }
    analysis
}

async fn analyze_pr_with_gemini(pr_detail: &PrDetail, model: &str) -> PrAnalysis {
    let diff = if pr_detail.diff.len() > 15_000 {
        &pr_detail.diff[..15_000]
    } else {
        &pr_detail.diff
    };
    let prompt = PR_ANALYSIS_PROMPT
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
        .replace("{diff}", diff);

    match generate_gemini(&prompt, model).await {
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
            summary: format!("Gemini analysis failed: {err}"),
            review_comment: "Unable to generate review comment due to a Gemini API error."
                .to_string(),
            ..PrAnalysis::default()
        },
    }
}

async fn analyze_issue_with_gemini(issue_detail: &IssueDetail, model: &str) -> IssueAnalysis {
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
    let prompt = ISSUE_ANALYSIS_PROMPT
        .replace("{title}", &issue_detail.issue.title)
        .replace("{author}", &issue_detail.issue.author)
        .replace("{number}", &issue_detail.issue.number.to_string())
        .replace("{labels}", empty_as(&issue_detail.issue.label_raw, "none"))
        .replace(
            "{body}",
            empty_as(&issue_detail.issue.body, "(no description)"),
        )
        .replace("{comments}", &comments);

    match generate_gemini(&prompt, model).await {
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
            overview: format!("Gemini analysis failed: {err}"),
            suggested_fix: "Unable to generate fix suggestion due to a Gemini API error."
                .to_string(),
            ..IssueAnalysis::default()
        },
    }
}

async fn generate_gemini(prompt: &str, model: &str) -> Result<String> {
    let api_key =
        env::var("GEMINI_API_KEY").context("GEMINI_API_KEY environment variable is required")?;
    let url = format!(
        "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
        model, api_key
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

fn normalize_model(provider: &str, model: &str) -> String {
    if provider == "gemini" && (model.is_empty() || model == DEFAULT_MODEL) {
        DEFAULT_GEMINI_MODEL.to_string()
    } else {
        model.to_string()
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
}
