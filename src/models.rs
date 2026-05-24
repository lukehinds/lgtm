use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CiStatus {
    Passing,
    Failing,
    Pending,
    Unknown,
}

impl Default for CiStatus {
    fn default() -> Self {
        Self::Unknown
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrSize {
    XS,
    S,
    M,
    L,
    XL,
}

impl PrSize {
    pub fn from_lines(lines: i64) -> Self {
        match lines {
            n if n < 10 => Self::XS,
            n if n < 50 => Self::S,
            n if n < 200 => Self::M,
            n if n < 500 => Self::L,
            _ => Self::XL,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum IssueSeverity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

impl Default for IssueSeverity {
    fn default() -> Self {
        Self::Info
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueLabel {
    Bug,
    Question,
    Enhancement,
    Feature,
    Other,
}

impl IssueLabel {
    pub fn from_github_label(label: &str) -> Self {
        let normalized = label.trim().to_lowercase();
        for (needle, label) in [
            ("bug", Self::Bug),
            ("question", Self::Question),
            ("enhancement", Self::Enhancement),
            ("feature", Self::Feature),
        ] {
            if normalized.contains(needle) {
                return label;
            }
        }
        if normalized.contains("feat") {
            Self::Feature
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrData {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub is_dependabot: bool,
    pub additions: i64,
    pub deletions: i64,
    pub changed_files: i64,
    pub ci_status: CiStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub head_sha: String,
    pub body: String,
    pub labels: Vec<String>,
    pub files: Vec<String>,
}

impl PrData {
    pub fn lines_changed(&self) -> i64 {
        self.additions + self.deletions
    }

    pub fn change_stats_known(&self) -> bool {
        self.additions > 0 || self.deletions > 0 || self.changed_files > 0
    }

    pub fn size(&self) -> PrSize {
        PrSize::from_lines(self.lines_changed())
    }

    pub fn size_if_known(&self) -> Option<PrSize> {
        self.change_stats_known().then(|| self.size())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrDetail {
    pub pr: PrData,
    pub diff: String,
    pub files: Vec<String>,
    pub review_comments: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrAnalysis {
    pub summary: String,
    pub security_risks: String,
    pub code_quality: String,
    pub risk_level: String,
    pub disruption_assessment: String,
    pub backwards_compatibility: String,
    pub semver_impact: String,
    pub review_comment: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueData {
    pub number: u64,
    pub title: String,
    pub author: String,
    pub label: IssueLabel,
    pub label_raw: String,
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub body: String,
    pub comment_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueDetail {
    pub issue: IssueData,
    pub comments: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueAnalysis {
    pub severity: IssueSeverity,
    pub overview: String,
    pub suspected_cause: String,
    pub suggested_fix: String,
}

impl Default for IssueAnalysis {
    fn default() -> Self {
        Self {
            severity: IssueSeverity::Info,
            overview: String::new(),
            suspected_cause: String::new(),
            suggested_fix: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pr(additions: i64, deletions: i64, changed_files: i64) -> PrData {
        PrData {
            number: 1,
            title: "test".to_string(),
            author: "author".to_string(),
            is_dependabot: false,
            additions,
            deletions,
            changed_files,
            ci_status: CiStatus::Unknown,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            head_sha: "sha".to_string(),
            body: String::new(),
            labels: Vec::new(),
            files: Vec::new(),
        }
    }

    #[test]
    fn zero_change_stats_are_unknown_for_list_prs() {
        let pr = sample_pr(0, 0, 0);
        assert!(!pr.change_stats_known());
        assert_eq!(pr.size_if_known(), None);
    }

    #[test]
    fn detail_change_stats_get_size() {
        let pr = sample_pr(208_146, 5_962, 816);
        assert!(pr.change_stats_known());
        assert_eq!(pr.size_if_known(), Some(PrSize::XL));
    }
}
