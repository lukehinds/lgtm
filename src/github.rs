use std::env;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::{Client, StatusCode, header};
use serde::Deserialize;

use crate::models::{CiStatus, IssueData, IssueDetail, IssueLabel, PrData, PrDetail};

const API_ROOT: &str = "https://api.github.com";

#[derive(Clone)]
pub struct GitHubClient {
    client: Client,
    repo: String,
}

impl GitHubClient {
    pub fn new(repo: String) -> Result<Self> {
        let token =
            env::var("GITHUB_TOKEN").context("GITHUB_TOKEN environment variable is required")?;
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
        headers.insert(
            header::ACCEPT,
            header::HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            header::HeaderValue::from_static("2022-11-28"),
        );
        let client = Client::builder()
            .user_agent("lgtm/0.1.0")
            .default_headers(headers)
            .build()?;
        Ok(Self { client, repo })
    }

    pub async fn ensure_repo_access(&self) -> Result<()> {
        let _: RepoResponse = self
            .get(&format!("{API_ROOT}/repos/{}", self.repo))
            .await
            .with_context(|| format!("failed to access repository '{}'", self.repo))?;
        Ok(())
    }

    pub async fn list_prs(
        &self,
        page: usize,
        per_page: usize,
        fetch_files: bool,
    ) -> Result<(Vec<PrData>, u64)> {
        let url = format!(
            "{API_ROOT}/repos/{}/pulls?state=open&sort=updated&direction=desc&page={}&per_page={}",
            self.repo,
            page + 1,
            per_page
        );
        let response = self.client.get(url).send().await?;
        let total = estimate_total_from_link(response.headers(), per_page).unwrap_or(0);
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            bail!("GitHub API error {status}: {body}");
        }
        let pulls: Vec<PullResponse> = serde_json::from_str(&body)?;
        let total = total.max(pulls.len() as u64);
        let mut out = Vec::with_capacity(pulls.len());
        for pr in pulls {
            let files = if fetch_files {
                self.list_pr_files(pr.number).await.unwrap_or_default()
            } else {
                Vec::new()
            };
            out.push(pr.into_pr_data(CiStatus::Unknown, files));
        }
        Ok((out, total))
    }

    pub async fn list_issues(
        &self,
        page: usize,
        per_page: usize,
        direction: &str,
    ) -> Result<(Vec<IssueData>, u64)> {
        let result: SearchIssuesResponse = self
            .get(&format!(
                "{API_ROOT}/search/issues?q=repo:{}+is:issue+is:open&sort=created&order={}&page={}&per_page={}",
                self.repo,
                direction,
                page + 1,
                per_page
            ))
            .await?;
        let total = result.total_count;
        Ok((
            result
                .items
                .into_iter()
                .map(IssueResponse::into_issue_data)
                .collect(),
            total,
        ))
    }

    pub async fn get_pr_head_sha(&self, number: u64) -> Result<String> {
        let pr: PullResponse = self
            .get(&format!("{API_ROOT}/repos/{}/pulls/{number}", self.repo))
            .await?;
        Ok(pr.head.sha)
    }

    pub async fn get_pr_summary(&self, number: u64, fetch_ci: bool) -> Result<PrDetail> {
        let pr: PullResponse = self
            .get(&format!("{API_ROOT}/repos/{}/pulls/{number}", self.repo))
            .await?;
        let ci = if fetch_ci {
            self.get_ci_status(&pr.head.sha)
                .await
                .unwrap_or(CiStatus::Unknown)
        } else {
            CiStatus::Unknown
        };
        Ok(PrDetail {
            pr: pr.into_pr_data(ci, Vec::new()),
            diff: String::new(),
            files: Vec::new(),
            review_comments: Vec::new(),
        })
    }

    pub async fn get_pr_detail(&self, number: u64) -> Result<PrDetail> {
        let pr: PullResponse = self
            .get(&format!("{API_ROOT}/repos/{}/pulls/{number}", self.repo))
            .await?;
        let files_raw: Vec<PullFileResponse> = self
            .get(&format!(
                "{API_ROOT}/repos/{}/pulls/{number}/files?per_page=100",
                self.repo
            ))
            .await?;
        let files = files_raw
            .iter()
            .map(|f| f.filename.clone())
            .collect::<Vec<_>>();
        let diff = files_raw
            .iter()
            .filter_map(|f| {
                f.patch
                    .as_ref()
                    .map(|patch| format!("--- {}\n{}", f.filename, patch))
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let comments: Vec<CommentResponse> = self
            .get(&format!(
                "{API_ROOT}/repos/{}/issues/{number}/comments?per_page=100",
                self.repo
            ))
            .await
            .unwrap_or_default();
        let review_comments = comments
            .into_iter()
            .map(|c| format!("{}: {}", c.user.login, c.body.unwrap_or_default()))
            .collect();
        let ci = self
            .get_ci_status(&pr.head.sha)
            .await
            .unwrap_or(CiStatus::Unknown);
        Ok(PrDetail {
            pr: pr.into_pr_data(ci, files.clone()),
            diff,
            files,
            review_comments,
        })
    }

    pub async fn get_issue_summary(&self, number: u64) -> Result<IssueDetail> {
        let issue: IssueResponse = self
            .get(&format!("{API_ROOT}/repos/{}/issues/{number}", self.repo))
            .await?;
        Ok(IssueDetail {
            issue: issue.into_issue_data(),
            comments: Vec::new(),
        })
    }

    pub async fn get_issue_detail(&self, number: u64) -> Result<IssueDetail> {
        let issue: IssueResponse = self
            .get(&format!("{API_ROOT}/repos/{}/issues/{number}", self.repo))
            .await?;
        let comments: Vec<CommentResponse> = self
            .get(&format!(
                "{API_ROOT}/repos/{}/issues/{number}/comments?per_page=100",
                self.repo
            ))
            .await
            .unwrap_or_default();
        Ok(IssueDetail {
            issue: issue.into_issue_data(),
            comments: comments
                .into_iter()
                .map(|c| format!("{}: {}", c.user.login, c.body.unwrap_or_default()))
                .collect(),
        })
    }

    async fn list_pr_files(&self, number: u64) -> Result<Vec<String>> {
        let files: Vec<PullFileResponse> = self
            .get(&format!(
                "{API_ROOT}/repos/{}/pulls/{number}/files?per_page=100",
                self.repo
            ))
            .await?;
        Ok(files.into_iter().map(|f| f.filename).collect())
    }

    async fn get_ci_status(&self, sha: &str) -> Result<CiStatus> {
        let runs: CheckRunsResponse = self
            .get(&format!(
                "{API_ROOT}/repos/{}/commits/{sha}/check-runs",
                self.repo
            ))
            .await?;
        if runs.check_runs.is_empty() {
            let combined: CombinedStatusResponse = self
                .get(&format!(
                    "{API_ROOT}/repos/{}/commits/{sha}/status",
                    self.repo
                ))
                .await?;
            return Ok(match combined.state.as_str() {
                "success" => CiStatus::Passing,
                "failure" => CiStatus::Failing,
                "pending" => CiStatus::Pending,
                _ => CiStatus::Unknown,
            });
        }
        if runs
            .check_runs
            .iter()
            .all(|r| r.conclusion.as_deref() == Some("success"))
        {
            Ok(CiStatus::Passing)
        } else if runs
            .check_runs
            .iter()
            .any(|r| matches!(r.conclusion.as_deref(), Some("failure" | "cancelled")))
        {
            Ok(CiStatus::Failing)
        } else if runs
            .check_runs
            .iter()
            .any(|r| matches!(r.status.as_str(), "queued" | "in_progress"))
        {
            Ok(CiStatus::Pending)
        } else {
            Ok(CiStatus::Unknown)
        }
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Result<T> {
        let response = self.client.get(url).send().await?;
        let status = response.status();
        let body = response.text().await?;
        if status == StatusCode::NOT_FOUND {
            bail!("not found");
        }
        if !status.is_success() {
            bail!("GitHub API error {status}: {body}");
        }
        Ok(serde_json::from_str(&body)?)
    }
}

fn estimate_total_from_link(headers: &header::HeaderMap, per_page: usize) -> Option<u64> {
    let link = headers.get(header::LINK)?.to_str().ok()?;
    for part in link.split(',') {
        if part.contains("rel=\"last\"") {
            let page = part
                .split("page=")
                .nth(1)?
                .split(['&', '>'])
                .next()?
                .parse::<u64>()
                .ok()?;
            return Some(page * per_page as u64);
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct RepoResponse {}

#[derive(Debug, Deserialize)]
struct UserResponse {
    login: String,
    #[serde(default)]
    r#type: String,
}

#[derive(Debug, Deserialize)]
struct LabelResponse {
    name: String,
}

#[derive(Debug, Deserialize)]
struct PullHeadResponse {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct PullResponse {
    number: u64,
    title: String,
    user: Option<UserResponse>,
    #[serde(default)]
    additions: i64,
    #[serde(default)]
    deletions: i64,
    #[serde(default)]
    changed_files: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    head: PullHeadResponse,
    body: Option<String>,
    #[serde(default)]
    labels: Vec<LabelResponse>,
}

impl PullResponse {
    fn into_pr_data(self, ci_status: CiStatus, files: Vec<String>) -> PrData {
        let author = self
            .user
            .as_ref()
            .map(|u| u.login.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let is_dependabot = author.to_lowercase().contains("dependabot")
            || self
                .user
                .as_ref()
                .map(|u| u.r#type.to_lowercase().contains("bot"))
                .unwrap_or(false);
        PrData {
            number: self.number,
            title: self.title,
            author,
            is_dependabot,
            additions: self.additions,
            deletions: self.deletions,
            changed_files: self.changed_files,
            ci_status,
            created_at: self.created_at,
            updated_at: self.updated_at,
            head_sha: self.head.sha,
            body: self.body.unwrap_or_default(),
            labels: self.labels.into_iter().map(|l| l.name).collect(),
            files,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PullFileResponse {
    filename: String,
    patch: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchIssuesResponse {
    total_count: u64,
    items: Vec<IssueResponse>,
}

#[derive(Debug, Deserialize)]
struct IssueResponse {
    number: u64,
    title: String,
    user: Option<UserResponse>,
    #[serde(default)]
    labels: Vec<LabelResponse>,
    state: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    body: Option<String>,
    comments: u64,
}

impl IssueResponse {
    fn into_issue_data(self) -> IssueData {
        let label_raw = self
            .labels
            .first()
            .map(|l| l.name.clone())
            .unwrap_or_default();
        let label = if label_raw.is_empty() {
            IssueLabel::Other
        } else {
            IssueLabel::from_github_label(&label_raw)
        };
        IssueData {
            number: self.number,
            title: self.title,
            author: self
                .user
                .map(|u| u.login)
                .unwrap_or_else(|| "unknown".to_string()),
            label,
            label_raw,
            state: self.state,
            created_at: self.created_at,
            updated_at: self.updated_at,
            body: self.body.unwrap_or_default(),
            comment_count: self.comments,
        }
    }
}

#[derive(Debug, Deserialize)]
struct CommentResponse {
    user: UserResponse,
    body: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CheckRunsResponse {
    check_runs: Vec<CheckRunResponse>,
}

#[derive(Debug, Deserialize)]
struct CheckRunResponse {
    status: String,
    conclusion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CombinedStatusResponse {
    state: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_list_response_defaults_detail_only_counts() {
        let json = r##"
        {
          "number": 42,
          "title": "Improve terminal UI",
          "user": {"login": "octocat", "type": "User"},
          "created_at": "2026-05-24T08:00:00Z",
          "updated_at": "2026-05-24T08:30:00Z",
          "head": {"sha": "abc123"},
          "body": "hello",
          "labels": []
        }
        "##;
        let pr: PullResponse = serde_json::from_str(json).unwrap();
        let data = pr.into_pr_data(CiStatus::Unknown, Vec::new());
        assert_eq!(data.number, 42);
        assert_eq!(data.additions, 0);
        assert_eq!(data.deletions, 0);
        assert_eq!(data.changed_files, 0);
    }
}
