use chrono::Utc;

use crate::models::{CiStatus, PrData};

pub fn pr_sort_score(pr: &PrData) -> f64 {
    let ci_score = match pr.ci_status {
        CiStatus::Passing => 0.0,
        CiStatus::Pending => 0.5,
        CiStatus::Unknown => 0.7,
        CiStatus::Failing => 1.0,
    };

    let lines = pr.lines_changed();
    let size_score = match lines {
        n if n < 10 => 0.0,
        n if n < 50 => 0.2,
        n if n < 200 => 0.5,
        n if n < 500 => 0.8,
        _ => 1.0,
    };

    let age_days = (Utc::now() - pr.created_at).num_days();
    let age_score = (1.0 - (age_days as f64 / 90.0)).max(0.0);
    (ci_score * 0.40) + (size_score * 0.35) + (age_score * 0.25)
}

pub fn sort_prs(prs: &mut [PrData]) {
    prs.sort_by(|a, b| pr_sort_score(a).total_cmp(&pr_sort_score(b)));
}
