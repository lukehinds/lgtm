use chrono::Utc;

use crate::models::{CiStatus, PrData};

pub fn pr_sort_score(pr: &PrData) -> f64 {
    // Lower score = higher priority (sorted ascending).
    // Goal: surface passing-CI, small, recently-active PRs first — easy to review and merge.

    let ci_score = match pr.ci_status {
        CiStatus::Passing => 0.0,
        CiStatus::Pending => 0.4,
        CiStatus::Unknown => 0.6,
        CiStatus::Failing => 1.0,
    };

    let size_score = if pr.change_stats_known() {
        match pr.lines_changed() {
            n if n < 10 => 0.0,
            n if n < 50 => 0.2,
            n if n < 200 => 0.5,
            n if n < 500 => 0.8,
            _ => 1.0,
        }
    } else {
        0.5
    };

    // Tiebreaker: recently-updated PRs score lower (more likely to be ready).
    let stale_days = (Utc::now() - pr.updated_at).num_days();
    let recency_score = (stale_days as f64 / 30.0).min(1.0);

    (ci_score * 0.55) + (size_score * 0.35) + (recency_score * 0.10)
}

pub fn sort_prs(prs: &mut [PrData]) {
    prs.sort_by(|a, b| pr_sort_score(a).total_cmp(&pr_sort_score(b)));
}
