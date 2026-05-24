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

    pub fn size(&self) -> PrSize {
        PrSize::from_lines(self.lines_changed())
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
