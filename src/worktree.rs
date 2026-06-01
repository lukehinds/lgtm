use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

use crate::{cache, config::CacheConfig};

#[derive(Debug, Clone)]
pub struct PreparedWorktree {
    pub path: PathBuf,
    pub reused: bool,
}

pub fn prepare_pr_worktree(
    cache_config: &CacheConfig,
    repo: &str,
    repo_path: &str,
    pr_number: u64,
    head_sha: &str,
) -> Result<PreparedWorktree> {
    if !cache_config.enabled {
        bail!("cache is disabled");
    }
    if repo.trim().is_empty() {
        bail!("repository is not configured");
    }
    if head_sha.trim().is_empty() {
        bail!("PR head SHA is not available");
    }

    let source_repo = PathBuf::from(repo_path)
        .canonicalize()
        .with_context(|| format!("could not resolve review.repo_path `{repo_path}`"))?;
    let origin_url = git_stdout(&source_repo, &["remote", "get-url", "origin"])
        .ok()
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| repo_https_url(repo));

    let root = cache::cache_dir(cache_config)
        .join("worktrees")
        .join(safe_component(repo));
    fs::create_dir_all(&root)?;

    let mirror = root.join("repo.git");
    ensure_mirror(&mirror, &origin_url)?;
    fetch_pr_ref(&mirror, repo, &origin_url, pr_number)
        .with_context(|| format!("could not fetch PR #{pr_number}"))?;

    let fetched_sha = git_stdout(
        &mirror,
        &["rev-parse", &format!("refs/lgtm/pr-{pr_number}^{{commit}}")],
    )?;
    if fetched_sha.trim() != head_sha.trim() {
        bail!(
            "fetched PR head {} did not match GitHub head {}",
            fetched_sha.trim(),
            head_sha.trim()
        );
    }

    let worktree = root.join(format!("pr-{pr_number}-{}", short_sha(head_sha)));
    if worktree_head(&worktree).as_deref() == Some(head_sha.trim()) {
        return Ok(PreparedWorktree {
            path: worktree,
            reused: true,
        });
    }

    remove_worktree_path(&mirror, &worktree)?;
    git_run(
        &mirror,
        &[
            "worktree",
            "add",
            "--detach",
            "--force",
            path_arg(&worktree)?,
            head_sha.trim(),
        ],
    )?;

    Ok(PreparedWorktree {
        path: worktree,
        reused: false,
    })
}

fn ensure_mirror(mirror: &Path, origin_url: &str) -> Result<()> {
    if mirror.exists() {
        if git_run(mirror, &["rev-parse", "--git-dir"]).is_ok() {
            git_run(mirror, &["remote", "set-url", "origin", origin_url])?;
            return Ok(());
        }
        if mirror.is_dir() {
            fs::remove_dir_all(mirror)?;
        } else {
            fs::remove_file(mirror)?;
        }
    }
    let parent = mirror
        .parent()
        .context("mirror path did not have a parent directory")?;
    fs::create_dir_all(parent)?;
    git_run(
        parent,
        &[
            "clone",
            "--bare",
            "--no-tags",
            origin_url,
            path_arg(mirror)?,
        ],
    )
}

fn fetch_pr_ref(mirror: &Path, repo: &str, origin_url: &str, pr_number: u64) -> Result<()> {
    let refspec = format!("+refs/pull/{pr_number}/head:refs/lgtm/pr-{pr_number}");
    match git_run(mirror, &["fetch", "--no-tags", "origin", &refspec]) {
        Ok(()) => Ok(()),
        Err(first_err) if origin_url != repo_https_url(repo) => {
            let github_url = repo_https_url(repo);
            git_run(mirror, &["remote", "set-url", "origin", &github_url])?;
            git_run(mirror, &["fetch", "--no-tags", "origin", &refspec])
                .with_context(|| format!("fallback to {github_url} failed after: {first_err}"))
        }
        Err(err) => Err(err),
    }
}

fn remove_worktree_path(mirror: &Path, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let _ = git_run(mirror, &["worktree", "remove", "--force", path_arg(path)?]);
    if path.exists() {
        if path.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn worktree_head(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    git_stdout(path, &["rev-parse", "HEAD"]).ok()
}

fn git_run(cwd: &Path, args: &[&str]) -> Result<()> {
    let output = git_command(cwd, args).output()?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{} (git {})",
        git_error_output(&output.stderr),
        args.join(" ")
    )
}

fn git_stdout(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = git_command(cwd, args).output()?;
    if !output.status.success() {
        bail!(
            "{} (git {})",
            git_error_output(&output.stderr),
            args.join(" ")
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git_command(cwd: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.current_dir(cwd);
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        if !token.trim().is_empty() {
            let auth = BASE64.encode(format!("x-access-token:{}", token.trim()));
            command.arg("-c").arg(format!(
                "http.https://github.com/.extraheader=AUTHORIZATION: basic {auth}"
            ));
        }
    }
    command.args(args);
    command
}

fn git_error_output(stderr: &[u8]) -> String {
    let message = String::from_utf8_lossy(stderr).trim().to_string();
    if message.is_empty() {
        return "git command failed".to_string();
    }
    message
        .lines()
        .last()
        .unwrap_or(&message)
        .trim()
        .to_string()
}

fn path_arg(path: &Path) -> Result<&str> {
    path.to_str()
        .context("cache path contains non-UTF-8 characters")
}

fn repo_https_url(repo: &str) -> String {
    format!("https://github.com/{}.git", repo.trim())
}

fn safe_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => ch,
            _ => '_',
        })
        .collect()
}

fn short_sha(value: &str) -> String {
    value.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn sanitizes_repo_component() {
        assert_eq!(safe_component("owner/repo"), "owner_repo");
        assert_eq!(
            safe_component("owner.repo/repo-name"),
            "owner.repo_repo-name"
        );
    }

    #[test]
    fn shortens_sha() {
        assert_eq!(short_sha("1234567890abcdef"), "1234567890ab");
    }

    #[test]
    fn formats_git_error_with_last_stderr_line() {
        assert_eq!(
            git_error_output(b"fatal: first line\nfatal: useful line\n"),
            "fatal: useful line"
        );
        assert_eq!(git_error_output(b""), "git command failed");
    }

    #[test]
    fn ensure_mirror_replaces_invalid_cache_path() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }

        let root = env::temp_dir().join(format!(
            "lgtm-invalid-mirror-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let remote = root.join("remote.git");
        let mirror = root.join("mirror.git");
        fs::create_dir_all(&mirror).unwrap();
        fs::write(mirror.join("partial"), "not git").unwrap();
        git_run(&root, &["init", "--bare", path_arg(&remote).unwrap()]).unwrap();

        ensure_mirror(&mirror, path_arg(&remote).unwrap()).unwrap();
        assert_eq!(
            git_stdout(&mirror, &["remote", "get-url", "origin"]).unwrap(),
            path_arg(&remote).unwrap()
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn prepares_and_reuses_cached_pr_worktree() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }

        let root = env::temp_dir().join(format!(
            "lgtm-worktree-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let remote = root.join("remote.git");
        let checkout = root.join("checkout");
        fs::create_dir_all(&root).unwrap();
        git_run(&root, &["init", "--bare", path_arg(&remote).unwrap()]).unwrap();
        git_run(&root, &["init", path_arg(&checkout).unwrap()]).unwrap();
        git_run(&checkout, &["config", "user.email", "lgtm@example.com"]).unwrap();
        git_run(&checkout, &["config", "user.name", "lgtm"]).unwrap();
        git_run(
            &checkout,
            &["remote", "add", "origin", path_arg(&remote).unwrap()],
        )
        .unwrap();

        fs::write(checkout.join("lib.rs"), "pub fn base() {}\n").unwrap();
        git_run(&checkout, &["add", "lib.rs"]).unwrap();
        git_run(&checkout, &["commit", "-m", "base"]).unwrap();
        git_run(&checkout, &["push", "origin", "HEAD:refs/heads/main"]).unwrap();

        fs::write(checkout.join("lib.rs"), "pub fn pr() {}\n").unwrap();
        git_run(&checkout, &["commit", "-am", "pr"]).unwrap();
        let head_sha = git_stdout(&checkout, &["rev-parse", "HEAD"]).unwrap();
        git_run(&checkout, &["push", "origin", "HEAD:refs/pull/1/head"]).unwrap();

        let cache = CacheConfig {
            enabled: true,
            dir: root.join("cache").display().to_string(),
            analysis_ttl_days: 30,
            review_input_ttl_days: 14,
            max_size_mb: 2048,
        };
        let prepared = prepare_pr_worktree(
            &cache,
            "owner/repo",
            path_arg(&checkout).unwrap(),
            1,
            &head_sha,
        )
        .unwrap();
        assert!(!prepared.reused);
        assert_eq!(
            worktree_head(&prepared.path).as_deref(),
            Some(head_sha.as_str())
        );
        assert_eq!(
            fs::read_to_string(prepared.path.join("lib.rs")).unwrap(),
            "pub fn pr() {}\n"
        );

        let reused = prepare_pr_worktree(
            &cache,
            "owner/repo",
            path_arg(&checkout).unwrap(),
            1,
            &head_sha,
        )
        .unwrap();
        assert!(reused.reused);
        assert_eq!(reused.path, prepared.path);

        let _ = fs::remove_dir_all(root);
    }
}
