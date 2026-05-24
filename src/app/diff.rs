use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::{TEXT_PRIMARY, TEXT_SECONDARY};

#[derive(Debug, Clone)]
pub(super) struct DiffFile {
    pub old_path: String,
    pub new_path: String,
    pub start_line: usize,
}

#[derive(Debug, Clone)]
pub(super) struct DiffHunk {
    pub file_index: usize,
    pub start_line: usize,
}

pub(super) fn parse_diff(diff: &str) -> (Vec<Line<'static>>, Vec<DiffFile>, Vec<DiffHunk>) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut files = Vec::new();
    let mut hunks = Vec::new();
    let mut current_file: Option<usize> = None;
    let mut pending_old_path: Option<String> = None;

    for raw_line in diff.lines() {
        let line_index = lines.len();
        let starts_file_chunk = raw_line.starts_with("--- ")
            && (current_file.is_none()
                || lines.last().is_some_and(|line| line.to_string().is_empty()));
        if let Some(rest) = raw_line.strip_prefix("diff --git ") {
            let (old_path, new_path) = parse_diff_git_paths(rest);
            files.push(DiffFile {
                old_path,
                new_path,
                start_line: line_index,
            });
            current_file = Some(files.len() - 1);
            pending_old_path = None;
            lines.push(styled_diff_line(raw_line));
            continue;
        }
        if let Some(path) = raw_line.strip_prefix("--- ") {
            pending_old_path = Some(trim_diff_path(path));
            if starts_file_chunk {
                let path = trim_diff_path(path);
                files.push(DiffFile {
                    old_path: path.clone(),
                    new_path: path,
                    start_line: line_index,
                });
                current_file = Some(files.len() - 1);
            }
            lines.push(styled_diff_line(raw_line));
            continue;
        }
        if let Some(path) = raw_line.strip_prefix("+++ ") {
            if let Some(file_index) = current_file {
                files[file_index].new_path = trim_diff_path(path);
            }
            if current_file.is_none() {
                let old_path = pending_old_path
                    .take()
                    .unwrap_or_else(|| "unknown".to_string());
                files.push(DiffFile {
                    old_path,
                    new_path: trim_diff_path(path),
                    start_line: line_index.saturating_sub(1),
                });
                current_file = Some(files.len() - 1);
            }
            lines.push(styled_diff_line(raw_line));
            continue;
        }
        if raw_line.starts_with("@@") {
            let file_index = current_file.unwrap_or_else(|| {
                files.push(DiffFile {
                    old_path: "unknown".to_string(),
                    new_path: "unknown".to_string(),
                    start_line: line_index,
                });
                files.len() - 1
            });
            hunks.push(DiffHunk {
                file_index,
                start_line: line_index,
            });
        }
        lines.push(styled_diff_line(raw_line));
    }

    if lines.is_empty() {
        lines.push(Line::from("No diff available."));
    }
    (lines, files, hunks)
}

pub(super) fn current_diff_file<'a>(index: usize, files: &'a [DiffFile]) -> Option<&'a DiffFile> {
    files.get(index).or_else(|| files.first())
}

pub(super) fn display_diff_path(file: &DiffFile) -> String {
    if file.old_path == file.new_path || file.old_path == "/dev/null" {
        file.new_path.clone()
    } else if file.new_path == "/dev/null" {
        file.old_path.clone()
    } else {
        format!("{} -> {}", file.old_path, file.new_path)
    }
}

fn styled_diff_line(line: &str) -> Line<'static> {
    let style = if line.starts_with("diff --git ") {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if line.starts_with("@@") {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else if line.starts_with("+++") || line.starts_with("---") {
        Style::default().fg(Color::LightBlue)
    } else if line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::default().fg(Color::Red)
    } else if line.starts_with("index ")
        || line.starts_with("new file mode")
        || line.starts_with("deleted file mode")
        || line.starts_with("similarity index")
        || line.starts_with("rename from")
        || line.starts_with("rename to")
    {
        Style::default().fg(TEXT_SECONDARY)
    } else {
        Style::default().fg(TEXT_PRIMARY)
    };
    Line::from(Span::styled(line.to_string(), style))
}

fn parse_diff_git_paths(rest: &str) -> (String, String) {
    let mut parts = rest.split_whitespace();
    let old_path = parts.next().map(trim_diff_path).unwrap_or_default();
    let new_path = parts.next().map(trim_diff_path).unwrap_or_default();
    (old_path, new_path)
}

fn trim_diff_path(path: &str) -> String {
    path.trim()
        .trim_matches('"')
        .trim_start_matches("a/")
        .trim_start_matches("b/")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_files_and_hunks_from_git_diff() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,2 @@
-old
+new
diff --git a/src/b.rs b/src/b.rs
--- a/src/b.rs
+++ b/src/b.rs
@@ -4,1 +4,2 @@
 context";

        let (lines, files, hunks) = parse_diff(diff);

        assert_eq!(lines.len(), 12);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].old_path, "src/a.rs");
        assert_eq!(files[0].new_path, "src/a.rs");
        assert_eq!(files[1].old_path, "src/b.rs");
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].file_index, 0);
        assert_eq!(hunks[1].file_index, 1);
    }

    #[test]
    fn displays_renames_and_deleted_paths() {
        let renamed = DiffFile {
            old_path: "old.rs".to_string(),
            new_path: "new.rs".to_string(),
            start_line: 0,
        };
        let deleted = DiffFile {
            old_path: "old.rs".to_string(),
            new_path: "/dev/null".to_string(),
            start_line: 0,
        };

        assert_eq!(display_diff_path(&renamed), "old.rs -> new.rs");
        assert_eq!(display_diff_path(&deleted), "old.rs");
    }

    #[test]
    fn parses_github_file_patch_chunks_without_git_headers() {
        let diff = "\
--- crates/nono-cli/src/command_runtime.rs
@@ -168,7 +168,7 @@ pub(crate) fn run_shell(args: ShellArgs, silent: bool) -> Result<()> {
-    let prepared = prepare_sandbox(&args.sandbox, silent)?;
+    let mut prepared = prepare_sandbox(&args.sandbox, silent)?;

--- crates/nono-cli/src/main.rs
@@ -10,1 +10,1 @@
-old
+new";

        let (_, files, hunks) = parse_diff(diff);

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path, "crates/nono-cli/src/command_runtime.rs");
        assert_eq!(files[1].new_path, "crates/nono-cli/src/main.rs");
        assert_eq!(hunks[0].file_index, 0);
        assert_eq!(hunks[1].file_index, 1);
    }
}
