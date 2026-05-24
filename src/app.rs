use std::{
    io::{self, Stdout},
    path::PathBuf,
    process::Command,
    time::Duration,
};

use chrono::{DateTime, Utc};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Tabs, Wrap,
    },
};
use tokio::{
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task::JoinHandle,
};

use crate::{
    ai, cache,
    config::{CacheConfig, WatchedPathConfig},
    github::GitHubClient,
    models::{
        IssueAnalysis, IssueData, IssueDetail, IssueLabel, IssueSeverity, PrAnalysis, PrData,
        PrDetail, PrSize,
    },
    sorting,
};

// RGB text tiers — bypasses terminal ANSI palette remapping so these render
// consistently across Warp, iTerm2, kitty, etc. regardless of color scheme.
const TEXT_PRIMARY: Color = Color::Rgb(210, 210, 210); // body text, titles
const TEXT_SECONDARY: Color = Color::Rgb(150, 150, 150); // age, model name, dim info
const TEXT_MUTED: Color = Color::Rgb(100, 100, 100); // separators, borders

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub repo: String,
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    pub prompt_version: String,
    pub cache: CacheConfig,
    pub cache_ttl_seconds: u64,
    pub poll_interval_seconds: u64,
    pub config_paths: Vec<PathBuf>,
    pub watch_paths: Vec<WatchedPathConfig>,
    pub columns: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    PullRequests,
    Issues,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrSort {
    Smart,
    Newest,
    Oldest,
    Smallest,
    Largest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IssueSort {
    Newest,
    Oldest,
    Author,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheFilter {
    All,
    Cached,
    Uncached,
}

#[derive(Debug, Clone)]
enum Screen {
    List,
    Loading,
    Error,
    Help,
    Info,
    PrDetail,
    IssueDetail,
    Diff,
}

#[derive(Debug, Clone)]
struct DiffFile {
    old_path: String,
    new_path: String,
    start_line: usize,
}

#[derive(Debug, Clone)]
struct DiffHunk {
    file_index: usize,
    start_line: usize,
}

enum DetailLoadResult {
    Pr {
        detail: PrDetail,
        analysis: PrAnalysis,
        status: String,
    },
    Issue {
        detail: IssueDetail,
        analysis: IssueAnalysis,
        status: String,
    },
}

struct State {
    config: AppConfig,
    client: GitHubClient,
    tab: Tab,
    screen: Screen,
    prs: Vec<PrData>,
    issues: Vec<IssueData>,
    pr_page: usize,
    issue_page: usize,
    pr_total: u64,
    issue_total: u64,
    selected_pr: usize,
    selected_issue: usize,
    pr_sort: PrSort,
    issue_sort: IssueSort,
    cache_filter: CacheFilter,
    search_query: String,
    search_input: bool,
    status: String,
    pr_detail: Option<PrDetail>,
    pr_analysis: Option<PrAnalysis>,
    issue_detail: Option<IssueDetail>,
    issue_analysis: Option<IssueAnalysis>,
    detail_task: Option<JoinHandle<std::result::Result<DetailLoadResult, String>>>,
    detail_phase_rx: Option<UnboundedReceiver<String>>,
    load_error: Option<String>,
    pr_list_state: ListState,
    issue_list_state: ListState,
    pr_detail_scroll: u16,
    issue_detail_scroll: u16,
    diff_scroll: u16,
    diff_file_index: usize,
    diff_hunk_index: usize,
    quit_armed: bool,
}

pub async fn run(config: AppConfig, client: GitHubClient) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let mut state = State::new(config, client);

    let result = async {
        terminal.draw(|frame| render(frame, &mut state))?;
        state.status = "Connecting to GitHub...".to_string();
        terminal.draw(|frame| render(frame, &mut state))?;
        state.client.ensure_repo_access().await?;
        state.status = "Loading pull requests...".to_string();
        terminal.draw(|frame| render(frame, &mut state))?;
        state.load_prs(false).await;
        terminal.draw(|frame| render(frame, &mut state))?;
        state.status = "Loading issues...".to_string();
        terminal.draw(|frame| render(frame, &mut state))?;
        state.load_issues(false).await;
        state.status = "Ready".to_string();
        run_loop(&mut terminal, &mut state).await
    }
    .await;
    restore_terminal(&mut terminal)?;
    result
}

impl State {
    fn new(config: AppConfig, client: GitHubClient) -> Self {
        Self {
            config,
            client,
            tab: Tab::PullRequests,
            screen: Screen::List,
            prs: Vec::new(),
            issues: Vec::new(),
            pr_page: 0,
            issue_page: 0,
            pr_total: 0,
            issue_total: 0,
            selected_pr: 0,
            selected_issue: 0,
            pr_sort: PrSort::Smart,
            issue_sort: IssueSort::Newest,
            cache_filter: CacheFilter::All,
            search_query: String::new(),
            search_input: false,
            status: "Loading repository data...".to_string(),
            pr_detail: None,
            pr_analysis: None,
            issue_detail: None,
            issue_analysis: None,
            detail_task: None,
            detail_phase_rx: None,
            load_error: None,
            pr_list_state: ListState::default(),
            issue_list_state: ListState::default(),
            pr_detail_scroll: 0,
            issue_detail_scroll: 0,
            diff_scroll: 0,
            diff_file_index: 0,
            diff_hunk_index: 0,
            quit_armed: false,
        }
    }

    fn apply_pr_sort(&self, prs: &mut Vec<PrData>) {
        match self.pr_sort {
            PrSort::Smart => sorting::sort_prs(prs),
            PrSort::Newest => prs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at)),
            PrSort::Oldest => prs.sort_by(|a, b| a.updated_at.cmp(&b.updated_at)),
            PrSort::Smallest => prs.sort_by_key(PrData::lines_changed),
            PrSort::Largest => prs.sort_by_key(|pr| std::cmp::Reverse(pr.lines_changed())),
        }
    }

    fn apply_issue_sort(&self, issues: &mut Vec<IssueData>) {
        match self.issue_sort {
            IssueSort::Newest => issues.sort_by(|a, b| b.created_at.cmp(&a.created_at)),
            IssueSort::Oldest => issues.sort_by(|a, b| a.created_at.cmp(&b.created_at)),
            IssueSort::Author => issues.sort_by(|a, b| a.author.cmp(&b.author)),
        }
    }

    async fn load_prs(&mut self, force: bool) {
        if !force {
            if let Some((mut prs, total)) = cache::get_cached_pr_list(
                &self.config.cache,
                &self.config.repo,
                self.pr_page,
                Some(Duration::from_secs(self.config.cache_ttl_seconds)),
            ) {
                self.apply_pr_sort(&mut prs);
                self.prs = prs;
                self.pr_total = total;
                return;
            }
        }
        self.status = "Fetching pull requests...".to_string();
        match self
            .client
            .list_prs(self.pr_page, 15, !self.config.watch_paths.is_empty())
            .await
        {
            Ok((mut prs, total)) => {
                cache::save_pr_list(
                    &self.config.cache,
                    &self.config.repo,
                    self.pr_page,
                    &prs,
                    total,
                );
                self.apply_pr_sort(&mut prs);
                self.prs = prs;
                self.pr_total = total;
                self.selected_pr = self.selected_pr.min(self.prs.len().saturating_sub(1));
                self.status = "Pull requests loaded".to_string();
            }
            Err(err) => self.status = format!("Failed to load PRs: {err}"),
        }
    }

    async fn load_issues(&mut self, force: bool) {
        let direction = if self.issue_sort == IssueSort::Oldest {
            "asc"
        } else {
            "desc"
        };
        if !force {
            if let Some((mut issues, total)) = cache::get_cached_issue_list(
                &self.config.cache,
                &self.config.repo,
                self.issue_page,
                direction,
                Some(Duration::from_secs(self.config.cache_ttl_seconds)),
            ) {
                self.apply_issue_sort(&mut issues);
                self.issues = issues;
                self.issue_total = total;
                return;
            }
        }
        self.status = "Fetching issues...".to_string();
        match self
            .client
            .list_issues(self.issue_page, 15, direction)
            .await
        {
            Ok((issues, total)) => {
                let mut issues = issues;
                cache::save_issue_list(
                    &self.config.cache,
                    &self.config.repo,
                    self.issue_page,
                    &issues,
                    total,
                    direction,
                );
                self.apply_issue_sort(&mut issues);
                self.issues = issues;
                self.issue_total = total;
                self.selected_issue = self.selected_issue.min(self.issues.len().saturating_sub(1));
                self.status = "Issues loaded".to_string();
            }
            Err(err) => self.status = format!("Failed to load issues: {err}"),
        }
    }

    fn open_selected(&mut self) {
        if self.detail_task.is_some() {
            return;
        }
        match self.tab {
            Tab::PullRequests => {
                let Some(pr) = self.prs.get(self.selected_pr).cloned() else {
                    return;
                };
                self.status = format!("Loading PR #{}...", pr.number);
                self.screen = Screen::Loading;
                self.pr_detail = None;
                self.pr_analysis = None;
                self.load_error = None;
                self.pr_detail_scroll = 0;
                self.diff_scroll = 0;
                self.diff_file_index = 0;
                self.diff_hunk_index = 0;
                let client = self.client.clone();
                let config = self.config.clone();
                let (phase_tx, phase_rx) = mpsc::unbounded_channel();
                self.detail_phase_rx = Some(phase_rx);
                self.detail_task = Some(tokio::spawn(async move {
                    load_pr_detail(client, config, pr, phase_tx).await
                }));
            }
            Tab::Issues => {
                let Some(issue) = self.issues.get(self.selected_issue).cloned() else {
                    return;
                };
                self.status = format!("Loading issue #{}...", issue.number);
                self.screen = Screen::Loading;
                self.issue_detail = None;
                self.issue_analysis = None;
                self.load_error = None;
                self.issue_detail_scroll = 0;
                let client = self.client.clone();
                let config = self.config.clone();
                let (phase_tx, phase_rx) = mpsc::unbounded_channel();
                self.detail_phase_rx = Some(phase_rx);
                self.detail_task = Some(tokio::spawn(async move {
                    load_issue_detail(client, config, issue, phase_tx).await
                }));
            }
        }
    }

    fn drain_detail_phases(&mut self) {
        let Some(rx) = self.detail_phase_rx.as_mut() else {
            return;
        };
        while let Ok(status) = rx.try_recv() {
            self.status = status;
        }
    }

    async fn complete_detail_load(&mut self) {
        self.drain_detail_phases();
        let Some(task) = self.detail_task.as_ref() else {
            return;
        };
        if !task.is_finished() {
            return;
        }
        let Some(task) = self.detail_task.take() else {
            return;
        };
        self.detail_phase_rx = None;
        match task.await {
            Ok(Ok(DetailLoadResult::Pr {
                detail,
                analysis,
                status,
            })) => {
                self.pr_detail = Some(detail);
                self.pr_analysis = Some(analysis);
                self.pr_detail_scroll = 0;
                self.diff_scroll = 0;
                self.diff_file_index = 0;
                self.diff_hunk_index = 0;
                self.screen = Screen::PrDetail;
                self.status = status;
            }
            Ok(Ok(DetailLoadResult::Issue {
                detail,
                analysis,
                status,
            })) => {
                self.issue_detail = Some(detail);
                self.issue_analysis = Some(analysis);
                self.issue_detail_scroll = 0;
                self.screen = Screen::IssueDetail;
                self.status = status;
            }
            Ok(Err(err)) => {
                self.load_error = Some(err.clone());
                self.screen = Screen::Error;
                self.status = err;
            }
            Err(err) => {
                let message = format!("Detail load failed: {err}");
                self.load_error = Some(message.clone());
                self.screen = Screen::Error;
                self.status = message;
            }
        }
    }
}

async fn load_pr_detail(
    client: GitHubClient,
    config: AppConfig,
    pr: PrData,
    phase_tx: UnboundedSender<String>,
) -> std::result::Result<DetailLoadResult, String> {
    let _ = phase_tx.send(format!("Resolving PR #{} revision...", pr.number));
    let head_sha = if pr.head_sha.is_empty() {
        client.get_pr_head_sha(pr.number).await.unwrap_or_default()
    } else {
        pr.head_sha.clone()
    };
    let provider = resolved_provider(&config);
    let _ = phase_tx.send(format!("Checking cached PR #{} analysis...", pr.number));
    if let Some(cached) = cache::get_cached_pr_analysis(
        &config.cache,
        &config.repo,
        pr.number,
        &head_sha,
        &provider.provider,
        &provider.model,
        &config.prompt_version,
        "pr-analysis-v1",
    ) {
        let _ = phase_tx.send(format!("Loading PR #{} metadata...", pr.number));
        let detail = client
            .get_pr_summary(pr.number, false)
            .await
            .map_err(|err| format!("Failed to load PR metadata: {err}"))?;
        return Ok(DetailLoadResult::Pr {
            detail,
            analysis: ai::pr_analysis_from_value(&cached),
            status: "PR analysis loaded from cache".to_string(),
        });
    }
    let _ = phase_tx.send(format!("Fetching PR #{} diff and metadata...", pr.number));
    let detail = client
        .get_pr_detail(pr.number)
        .await
        .map_err(|err| format!("Failed to load PR: {err}"))?;
    let _ = phase_tx.send(format!("Analyzing PR #{}...", pr.number));
    let analysis = ai::analyze_pr(
        &detail,
        &config.provider,
        &config.model,
        &config.base_url,
        &config.api_key_env,
        &config.cache,
        &config.repo,
        &config.prompt_version,
    )
    .await;
    Ok(DetailLoadResult::Pr {
        detail,
        analysis,
        status: "PR analysis loaded".to_string(),
    })
}

async fn load_issue_detail(
    client: GitHubClient,
    config: AppConfig,
    issue: IssueData,
    phase_tx: UnboundedSender<String>,
) -> std::result::Result<DetailLoadResult, String> {
    let provider = resolved_provider(&config);
    let _ = phase_tx.send(format!(
        "Checking cached issue #{} analysis...",
        issue.number
    ));
    if let Some(cached) = cache::get_cached_issue_analysis(
        &config.cache,
        &config.repo,
        issue.number,
        &provider.provider,
        &provider.model,
        &config.prompt_version,
        "issue-analysis-v1",
    ) {
        let _ = phase_tx.send(format!("Loading issue #{} metadata...", issue.number));
        let detail = client
            .get_issue_summary(issue.number)
            .await
            .map_err(|err| format!("Failed to load issue metadata: {err}"))?;
        return Ok(DetailLoadResult::Issue {
            detail,
            analysis: ai::issue_analysis_from_value(&cached),
            status: "Issue analysis loaded from cache".to_string(),
        });
    }
    let _ = phase_tx.send(format!("Fetching issue #{} details...", issue.number));
    let detail = client
        .get_issue_detail(issue.number)
        .await
        .map_err(|err| format!("Failed to load issue: {err}"))?;
    let _ = phase_tx.send(format!("Analyzing issue #{}...", issue.number));
    let analysis = ai::analyze_issue(
        &detail,
        &config.provider,
        &config.model,
        &config.base_url,
        &config.api_key_env,
        &config.cache,
        &config.repo,
        &config.prompt_version,
    )
    .await;
    Ok(DetailLoadResult::Issue {
        detail,
        analysis,
        status: "Issue analysis loaded".to_string(),
    })
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut State,
) -> Result<()> {
    loop {
        state.complete_detail_load().await;
        terminal.draw(|frame| render(frame, state))?;
        if !event::poll(Duration::from_millis(200))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if state.search_input {
            match key.code {
                KeyCode::Esc => {
                    state.search_input = false;
                    state.status = "Search cancelled".to_string();
                }
                KeyCode::Enter => {
                    state.search_input = false;
                    jump_to_search_match(state);
                    state.status = search_status(state);
                }
                KeyCode::Backspace => {
                    state.search_query.pop();
                    state.status = format!("Search: {}", state.search_query);
                }
                KeyCode::Char(c) => {
                    state.search_query.push(c);
                    state.status = format!("Search: {}", state.search_query);
                }
                _ => {}
            }
            continue;
        }
        match state.screen {
            Screen::List => match key.code {
                KeyCode::Char('q') => {
                    if state.quit_armed {
                        break;
                    }
                    state.quit_armed = true;
                    state.status = "Press q again to quit".to_string();
                }
                KeyCode::Char('?') => state.screen = Screen::Help,
                KeyCode::Char('i') => state.screen = Screen::Info,
                KeyCode::Char('o') => open_selected_in_browser(state),
                KeyCode::Char('/') => start_search(state),
                KeyCode::Char('x') => clear_search_and_filter(state),
                KeyCode::Char('f') => cycle_cache_filter(state),
                KeyCode::Tab => {
                    state.tab = if state.tab == Tab::PullRequests {
                        Tab::Issues
                    } else {
                        Tab::PullRequests
                    };
                    state.quit_armed = false;
                }
                KeyCode::Char('s') => cycle_sort(state).await,
                KeyCode::Char('r') => match state.tab {
                    Tab::PullRequests => state.load_prs(true).await,
                    Tab::Issues => state.load_issues(true).await,
                },
                KeyCode::Down => match state.tab {
                    Tab::PullRequests => move_list_selection(state, 1),
                    Tab::Issues => move_list_selection(state, 1),
                },
                KeyCode::Up => match state.tab {
                    Tab::PullRequests => move_list_selection(state, -1),
                    Tab::Issues => move_list_selection(state, -1),
                },
                KeyCode::Right => match state.tab {
                    Tab::PullRequests if ((state.pr_page + 1) * 15) < state.pr_total as usize => {
                        state.pr_page += 1;
                        state.selected_pr = 0;
                        *state.pr_list_state.offset_mut() = 0;
                        state.load_prs(false).await;
                    }
                    Tab::Issues if ((state.issue_page + 1) * 15) < state.issue_total as usize => {
                        state.issue_page += 1;
                        state.selected_issue = 0;
                        *state.issue_list_state.offset_mut() = 0;
                        state.load_issues(false).await;
                    }
                    _ => {}
                },
                KeyCode::Left => match state.tab {
                    Tab::PullRequests if state.pr_page > 0 => {
                        state.pr_page -= 1;
                        state.selected_pr = 0;
                        *state.pr_list_state.offset_mut() = 0;
                        state.load_prs(false).await;
                    }
                    Tab::Issues if state.issue_page > 0 => {
                        state.issue_page -= 1;
                        state.selected_issue = 0;
                        *state.issue_list_state.offset_mut() = 0;
                        state.load_issues(false).await;
                    }
                    _ => {}
                },
                KeyCode::Enter => state.open_selected(),
                _ => state.quit_armed = false,
            },
            Screen::Loading => match key.code {
                KeyCode::Esc => {
                    if let Some(task) = state.detail_task.take() {
                        task.abort();
                    }
                    state.detail_phase_rx = None;
                    state.screen = Screen::List;
                    state.status = "Load cancelled".to_string();
                }
                KeyCode::Char('q') => {
                    if state.quit_armed {
                        break;
                    }
                    state.quit_armed = true;
                    state.status = "Press q again to quit".to_string();
                }
                _ => state.quit_armed = false,
            },
            Screen::Error => match key.code {
                KeyCode::Esc => state.screen = Screen::List,
                KeyCode::Char('r') => state.open_selected(),
                KeyCode::Char('q') => {
                    if state.quit_armed {
                        break;
                    }
                    state.quit_armed = true;
                    state.status = "Press q again to quit".to_string();
                }
                _ => state.quit_armed = false,
            },
            Screen::Help | Screen::Info => match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('i') => {
                    state.screen = Screen::List
                }
                _ => {}
            },
            Screen::PrDetail => match key.code {
                KeyCode::Esc => state.screen = Screen::List,
                KeyCode::Char('d') => state.screen = Screen::Diff,
                KeyCode::Char('o') => open_selected_in_browser(state),
                KeyCode::Char('/') => start_search(state),
                KeyCode::Char('n') => search_next_in_detail(state),
                KeyCode::Down => scroll_down(&mut state.pr_detail_scroll, 1),
                KeyCode::Up => scroll_up(&mut state.pr_detail_scroll, 1),
                KeyCode::PageDown => scroll_down(&mut state.pr_detail_scroll, 10),
                KeyCode::PageUp => scroll_up(&mut state.pr_detail_scroll, 10),
                KeyCode::Home => state.pr_detail_scroll = 0,
                KeyCode::Char('c') => copy_text(
                    state
                        .pr_analysis
                        .as_ref()
                        .map(|a| a.review_comment.as_str())
                        .unwrap_or(""),
                    &mut state.status,
                ),
                KeyCode::Char('v') => copy_text(
                    state
                        .pr_analysis
                        .as_ref()
                        .map(|a| a.summary.as_str())
                        .unwrap_or(""),
                    &mut state.status,
                ),
                KeyCode::Char('q') => {
                    if state.quit_armed {
                        break;
                    }
                    state.quit_armed = true;
                    state.status = "Press q again to quit".to_string();
                }
                _ => state.quit_armed = false,
            },
            Screen::IssueDetail => match key.code {
                KeyCode::Esc => state.screen = Screen::List,
                KeyCode::Char('o') => open_selected_in_browser(state),
                KeyCode::Char('/') => start_search(state),
                KeyCode::Char('n') => search_next_in_detail(state),
                KeyCode::Down => scroll_down(&mut state.issue_detail_scroll, 1),
                KeyCode::Up => scroll_up(&mut state.issue_detail_scroll, 1),
                KeyCode::PageDown => scroll_down(&mut state.issue_detail_scroll, 10),
                KeyCode::PageUp => scroll_up(&mut state.issue_detail_scroll, 10),
                KeyCode::Home => state.issue_detail_scroll = 0,
                KeyCode::Char('c') => copy_text(
                    state
                        .issue_analysis
                        .as_ref()
                        .map(|a| a.suggested_fix.as_str())
                        .unwrap_or(""),
                    &mut state.status,
                ),
                KeyCode::Char('v') => copy_text(
                    state
                        .issue_analysis
                        .as_ref()
                        .map(|a| a.overview.as_str())
                        .unwrap_or(""),
                    &mut state.status,
                ),
                KeyCode::Char('q') => {
                    if state.quit_armed {
                        break;
                    }
                    state.quit_armed = true;
                    state.status = "Press q again to quit".to_string();
                }
                _ => state.quit_armed = false,
            },
            Screen::Diff => match key.code {
                KeyCode::Esc => state.screen = Screen::PrDetail,
                KeyCode::Char('/') => start_search(state),
                KeyCode::Down => scroll_down(&mut state.diff_scroll, 1),
                KeyCode::Up => scroll_up(&mut state.diff_scroll, 1),
                KeyCode::PageDown => scroll_down(&mut state.diff_scroll, 10),
                KeyCode::PageUp => scroll_up(&mut state.diff_scroll, 10),
                KeyCode::Home => state.diff_scroll = 0,
                KeyCode::Char('n') => jump_diff_file(state, 1),
                KeyCode::Char('p') => jump_diff_file(state, -1),
                KeyCode::Char(']') => jump_diff_hunk(state, 1),
                KeyCode::Char('[') => jump_diff_hunk(state, -1),
                KeyCode::Char('q') => {
                    if state.quit_armed {
                        break;
                    }
                    state.quit_armed = true;
                    state.status = "Press q again to quit".to_string();
                }
                _ => state.quit_armed = false,
            },
        }
    }
    Ok(())
}

fn render(frame: &mut Frame<'_>, state: &mut State) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(area);

    render_header(frame, state, chunks[0]);

    let titles = ["Pull Requests", "Issues"]
        .into_iter()
        .map(|title| {
            Line::from(Span::styled(
                title,
                Style::default()
                    .fg(TEXT_SECONDARY)
                    .add_modifier(Modifier::BOLD),
            ))
        })
        .collect::<Vec<_>>();
    let selected = if state.tab == Tab::PullRequests { 0 } else { 1 };
    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .block(panel_block(" Views "))
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        chunks[1],
    );

    match state.screen {
        Screen::List => render_list(frame, state, chunks[2]),
        Screen::Loading => render_loading(frame, state, chunks[2]),
        Screen::Error => render_error(frame, state, chunks[2]),
        Screen::Help => render_help(frame, chunks[2]),
        Screen::Info => render_info(frame, state, chunks[2]),
        Screen::PrDetail => render_pr_detail(frame, state, chunks[2]),
        Screen::IssueDetail => render_issue_detail(frame, state, chunks[2]),
        Screen::Diff => render_diff(frame, state, chunks[2]),
    }

    render_footer(frame, state, chunks[3]);
}

fn render_header(frame: &mut Frame<'_>, state: &State, area: Rect) {
    let provider = resolved_provider(&state.config);
    let title = Line::from(vec![
        Span::styled(
            " wftt ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            &state.config.repo,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {} / {}", provider.provider, provider.model),
            Style::default().fg(TEXT_SECONDARY),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(title)
            .block(panel_block(" GitHub Review Workbench "))
            .alignment(Alignment::Left),
        area,
    );
}

fn hint_spans(pairs: &[(&'static str, &'static str)]) -> Vec<Span<'static>> {
    let key_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(TEXT_SECONDARY);
    let sep_style = Style::default().fg(TEXT_MUTED);
    let mut out: Vec<Span<'static>> = Vec::new();
    for (i, (key, desc)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(Span::styled("  ", sep_style));
        }
        out.push(Span::styled(*key, key_style));
        out.push(Span::styled(format!(" {desc}"), desc_style));
    }
    out
}

fn render_footer(frame: &mut Frame<'_>, state: &State, area: Rect) {
    let help_spans: Vec<Span<'static>> = match state.screen {
        Screen::List if state.tab == Tab::PullRequests => hint_spans(&[
            ("Enter", "open"),
            ("Tab", "switch"),
            ("r", "refresh"),
            ("s", "sort"),
            ("f", "filter"),
            ("/", "search"),
            ("x", "clear"),
            ("o", "github"),
            ("i", "info"),
            ("?", "help"),
            ("q q", "quit"),
        ]),
        Screen::List => hint_spans(&[
            ("Enter", "open"),
            ("Tab", "switch"),
            ("r", "refresh"),
            ("s", "newest/oldest"),
            ("f", "filter"),
            ("/", "search"),
            ("x", "clear"),
            ("o", "github"),
            ("?", "help"),
            ("q q", "quit"),
        ]),
        Screen::PrDetail => hint_spans(&[
            ("Up/Down", "scroll"),
            ("d", "diff"),
            ("c", "copy review"),
            ("v", "copy summary"),
            ("o", "github"),
            ("Esc", "back"),
            ("q q", "quit"),
        ]),
        Screen::IssueDetail => hint_spans(&[
            ("Up/Down", "scroll"),
            ("c", "copy fix"),
            ("v", "copy overview"),
            ("o", "github"),
            ("Esc", "back"),
            ("q q", "quit"),
        ]),
        Screen::Diff => hint_spans(&[
            ("Up/Down", "scroll"),
            ("n/p", "file"),
            ("[/]", "hunk"),
            ("Esc", "back"),
            ("q q", "quit"),
        ]),
        Screen::Loading => hint_spans(&[("Esc", "cancel"), ("q q", "quit")]),
        Screen::Error => hint_spans(&[("r", "retry"), ("Esc", "back"), ("q q", "quit")]),
        Screen::Help | Screen::Info => hint_spans(&[("Esc", "close")]),
    };
    let status = if is_busy_status(&state.status) {
        format!("{} {}", spinner(), state.status)
    } else {
        state.status.clone()
    };
    let mut spans = vec![
        Span::styled(status, Style::default().fg(status_color(&state.status))),
        Span::styled(" │ ", Style::default().fg(TEXT_MUTED)),
    ];
    spans.extend(help_spans);
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(Block::default().borders(Borders::TOP)),
        area,
    );
}

fn render_list(frame: &mut Frame<'_>, state: &mut State, area: ratatui::layout::Rect) {
    match state.tab {
        Tab::PullRequests => {
            let provider = resolved_provider(&state.config);
            let visible = visible_pr_indices(state, &provider);
            if !visible.contains(&state.selected_pr) {
                if let Some(first) = visible.first() {
                    state.selected_pr = *first;
                }
            }
            let items = visible
                .iter()
                .map(|index| {
                    let pr = &state.prs[*index];
                    pr_item(
                        pr,
                        &state.config.watch_paths,
                        pr_has_cached_analysis(state, pr, &provider),
                        &state.config.columns,
                    )
                })
                .collect::<Vec<_>>();
            let selected_row = visible.iter().position(|index| *index == state.selected_pr);
            state
                .pr_list_state
                .select((!items.is_empty()).then_some(selected_row.unwrap_or(0)));
            let total_pages = pages(state.pr_total);
            let title = format!(
                " Pull Requests  page {} / {}  total {}  showing {}  {}  {}  C=cached ",
                state.pr_page + 1,
                total_pages,
                state.pr_total,
                visible.len(),
                pr_sort_label(state.pr_sort),
                filter_label(state.cache_filter),
            );
            if items.is_empty() {
                render_empty_state(
                    frame,
                    area,
                    &title,
                    "No open pull requests were returned.",
                    &state.status,
                );
            } else {
                frame.render_stateful_widget(
                    List::new(items)
                        .block(panel_block(&title))
                        .highlight_style(
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol(" > "),
                    area,
                    &mut state.pr_list_state,
                );
            }
        }
        Tab::Issues => {
            let provider = resolved_provider(&state.config);
            let visible = visible_issue_indices(state, &provider);
            if !visible.contains(&state.selected_issue) {
                if let Some(first) = visible.first() {
                    state.selected_issue = *first;
                }
            }
            let items = visible
                .iter()
                .map(|index| {
                    let issue = &state.issues[*index];
                    issue_item(
                        issue,
                        issue_has_cached_analysis(state, issue, &provider),
                        &state.config.columns,
                    )
                })
                .collect::<Vec<_>>();
            let selected_row = visible
                .iter()
                .position(|index| *index == state.selected_issue);
            state
                .issue_list_state
                .select((!items.is_empty()).then_some(selected_row.unwrap_or(0)));
            let title = format!(
                " Issues  {}  page {} / {}  total {}  showing {}  {}  C=cached ",
                issue_sort_label(state.issue_sort),
                state.issue_page + 1,
                pages(state.issue_total),
                state.issue_total,
                visible.len(),
                filter_label(state.cache_filter),
            );
            if items.is_empty() {
                render_empty_state(
                    frame,
                    area,
                    &title,
                    "No open issues were returned.",
                    &state.status,
                );
            } else {
                frame.render_stateful_widget(
                    List::new(items)
                        .block(panel_block(&title))
                        .highlight_style(
                            Style::default()
                                .fg(Color::Black)
                                .bg(Color::Yellow)
                                .add_modifier(Modifier::BOLD),
                        )
                        .highlight_symbol(" > "),
                    area,
                    &mut state.issue_list_state,
                );
            }
        }
    }
}

fn visible_pr_indices(state: &State, provider: &ai::ProviderConfig) -> Vec<usize> {
    state
        .prs
        .iter()
        .enumerate()
        .filter_map(|(index, pr)| {
            let cached = pr_has_cached_analysis(state, pr, provider);
            let labels = pr.labels.join(" ");
            item_visible(
                cached,
                state.cache_filter,
                &state.search_query,
                &[&pr.title, &pr.author, &pr.body, &labels],
            )
            .then_some(index)
        })
        .collect()
}

fn visible_issue_indices(state: &State, provider: &ai::ProviderConfig) -> Vec<usize> {
    state
        .issues
        .iter()
        .enumerate()
        .filter_map(|(index, issue)| {
            let cached = issue_has_cached_analysis(state, issue, provider);
            item_visible(
                cached,
                state.cache_filter,
                &state.search_query,
                &[&issue.title, &issue.author, &issue.body, &issue.label_raw],
            )
            .then_some(index)
        })
        .collect()
}

fn item_visible(cached: bool, filter: CacheFilter, query: &str, fields: &[&str]) -> bool {
    let cache_ok = match filter {
        CacheFilter::All => true,
        CacheFilter::Cached => cached,
        CacheFilter::Uncached => !cached,
    };
    if !cache_ok {
        return false;
    }
    let query = query.trim().to_lowercase();
    query.is_empty()
        || fields
            .iter()
            .any(|field| field.to_lowercase().contains(&query))
}

fn pr_has_cached_analysis(state: &State, pr: &PrData, provider: &ai::ProviderConfig) -> bool {
    !pr.head_sha.is_empty()
        && cache::get_cached_pr_analysis(
            &state.config.cache,
            &state.config.repo,
            pr.number,
            &pr.head_sha,
            &provider.provider,
            &provider.model,
            &state.config.prompt_version,
            "pr-analysis-v1",
        )
        .is_some()
}

fn issue_has_cached_analysis(
    state: &State,
    issue: &IssueData,
    provider: &ai::ProviderConfig,
) -> bool {
    cache::get_cached_issue_analysis(
        &state.config.cache,
        &state.config.repo,
        issue.number,
        &provider.provider,
        &provider.model,
        &state.config.prompt_version,
        "issue-analysis-v1",
    )
    .is_some()
}

fn panel_block(title: &str) -> Block<'_> {
    Block::default()
        .title(title.to_string())
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::DarkGray))
        .title_style(
            Style::default()
                .fg(Color::Rgb(80, 200, 200))
                .add_modifier(Modifier::BOLD),
        )
}

fn render_empty_state(frame: &mut Frame<'_>, area: Rect, title: &str, message: &str, status: &str) {
    let text = Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(
            message,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            status,
            Style::default().fg(status_color(status)),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Press r to refresh. Press ? for help.",
            Style::default().fg(Color::DarkGray),
        )),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .block(panel_block(title))
            .alignment(Alignment::Center),
        area,
    );
}

fn render_loading(frame: &mut Frame<'_>, state: &State, area: Rect) {
    let text = Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("{} {}", spinner(), state.status),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Press Esc to cancel.",
            Style::default().fg(TEXT_SECONDARY),
        )),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .block(panel_block(" Loading "))
            .alignment(Alignment::Center),
        area,
    );
}

fn render_error(frame: &mut Frame<'_>, state: &State, area: Rect) {
    let message = state
        .load_error
        .as_deref()
        .unwrap_or("The selected item could not be loaded.");
    let text = Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(
            "Could not load selected item",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(TEXT_PRIMARY),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Press r to retry or Esc to return to the list.",
            Style::default().fg(TEXT_SECONDARY),
        )),
    ]);
    frame.render_widget(
        Paragraph::new(text)
            .block(panel_block(" Error "))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_scrollable_lines(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    lines: Vec<Line<'static>>,
    scroll: &mut u16,
) {
    let line_count = lines.len();
    let viewport = area.height.saturating_sub(2) as usize;
    let max_scroll = line_count.saturating_sub(viewport);
    *scroll = (*scroll).min(max_scroll.min(u16::MAX as usize) as u16);
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(Block::default().title(title).borders(Borders::ALL))
            .wrap(Wrap { trim: false })
            .scroll((*scroll, 0)),
        area,
    );
    if line_count > viewport && area.height > 2 {
        let mut state = ScrollbarState::new(max_scroll).position(*scroll as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }
}

fn status_color(status: &str) -> Color {
    let lower = status.to_lowercase();
    if lower.contains("failed") || lower.contains("error") {
        Color::Red
    } else if is_busy_status(status) {
        Color::Yellow
    } else {
        Color::Green
    }
}

fn is_busy_status(status: &str) -> bool {
    let lower = status.to_lowercase();
    lower.contains("loading") || lower.contains("fetching") || lower.contains("connecting")
}

fn spinner() -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_millis() / 100) as usize)
        .unwrap_or(0);
    FRAMES[tick % FRAMES.len()]
}

fn scroll_down(scroll: &mut u16, amount: u16) {
    *scroll = scroll.saturating_add(amount);
}

fn scroll_up(scroll: &mut u16, amount: u16) {
    *scroll = scroll.saturating_sub(amount);
}

fn move_list_selection(state: &mut State, delta: isize) {
    let provider = resolved_provider(&state.config);
    match state.tab {
        Tab::PullRequests => {
            let visible = visible_pr_indices(state, &provider);
            if visible.is_empty() {
                return;
            }
            let row = visible
                .iter()
                .position(|index| *index == state.selected_pr)
                .unwrap_or(0);
            state.selected_pr = visible[offset_index(row, delta, visible.len() - 1)];
        }
        Tab::Issues => {
            let visible = visible_issue_indices(state, &provider);
            if visible.is_empty() {
                return;
            }
            let row = visible
                .iter()
                .position(|index| *index == state.selected_issue)
                .unwrap_or(0);
            state.selected_issue = visible[offset_index(row, delta, visible.len() - 1)];
        }
    }
}

fn start_search(state: &mut State) {
    state.search_input = true;
    state.status = if state.search_query.is_empty() {
        "Search: ".to_string()
    } else {
        format!("Search: {}", state.search_query)
    };
}

fn clear_search_and_filter(state: &mut State) {
    state.search_query.clear();
    state.cache_filter = CacheFilter::All;
    state.status = "Search and filters cleared".to_string();
}

fn cycle_cache_filter(state: &mut State) {
    state.cache_filter = match state.cache_filter {
        CacheFilter::All => CacheFilter::Cached,
        CacheFilter::Cached => CacheFilter::Uncached,
        CacheFilter::Uncached => CacheFilter::All,
    };
    state.status = format!("Filter: {}", filter_label(state.cache_filter));
}

async fn cycle_sort(state: &mut State) {
    match state.tab {
        Tab::PullRequests => {
            state.pr_sort = match state.pr_sort {
                PrSort::Smart => PrSort::Newest,
                PrSort::Newest => PrSort::Oldest,
                PrSort::Oldest => PrSort::Smallest,
                PrSort::Smallest => PrSort::Largest,
                PrSort::Largest => PrSort::Smart,
            };
            let mut prs = std::mem::take(&mut state.prs);
            state.apply_pr_sort(&mut prs);
            state.prs = prs;
            state.status = format!("PR sort: {}", pr_sort_label(state.pr_sort));
        }
        Tab::Issues => {
            state.issue_sort = match state.issue_sort {
                IssueSort::Newest => IssueSort::Oldest,
                IssueSort::Oldest => IssueSort::Author,
                IssueSort::Author => IssueSort::Newest,
            };
            state.issue_page = 0;
            state.load_issues(true).await;
            state.status = format!("Issue sort: {}", issue_sort_label(state.issue_sort));
        }
    }
}

fn search_status(state: &State) -> String {
    if state.search_query.trim().is_empty() {
        "Search cleared".to_string()
    } else {
        format!("Search: {}", state.search_query)
    }
}

fn jump_to_search_match(state: &mut State) {
    if state.search_query.trim().is_empty() {
        return;
    }
    match state.screen {
        Screen::List => {}
        Screen::PrDetail | Screen::IssueDetail | Screen::Diff => search_next_in_detail(state),
        _ => {}
    }
}

fn search_next_in_detail(state: &mut State) {
    let query = state.search_query.trim().to_lowercase();
    if query.is_empty() {
        state.status = "No search query".to_string();
        return;
    }
    let (text, scroll) = match state.screen {
        Screen::PrDetail => (pr_detail_text(state), &mut state.pr_detail_scroll),
        Screen::IssueDetail => (issue_detail_text(state), &mut state.issue_detail_scroll),
        Screen::Diff => (
            state
                .pr_detail
                .as_ref()
                .map(|detail| detail.diff.clone())
                .unwrap_or_default(),
            &mut state.diff_scroll,
        ),
        _ => return,
    };
    let start = (*scroll as usize).saturating_add(1);
    if let Some(index) =
        find_line_after(&text, &query, start).or_else(|| find_line_after(&text, &query, 0))
    {
        *scroll = index.min(u16::MAX as usize) as u16;
        state.status = format!("Match {} for {}", index + 1, state.search_query);
    } else {
        state.status = format!("No match for {}", state.search_query);
    }
}

fn find_line_after(text: &str, query: &str, start: usize) -> Option<usize> {
    text.lines()
        .enumerate()
        .skip(start)
        .find(|(_, line)| line.to_lowercase().contains(query))
        .map(|(index, _)| index)
}

fn pr_sort_label(sort: PrSort) -> &'static str {
    match sort {
        PrSort::Smart => "smart",
        PrSort::Newest => "newest",
        PrSort::Oldest => "oldest",
        PrSort::Smallest => "smallest",
        PrSort::Largest => "largest",
    }
}

fn issue_sort_label(sort: IssueSort) -> &'static str {
    match sort {
        IssueSort::Newest => "newest",
        IssueSort::Oldest => "oldest",
        IssueSort::Author => "author",
    }
}

fn filter_label(filter: CacheFilter) -> &'static str {
    match filter {
        CacheFilter::All => "all",
        CacheFilter::Cached => "cached",
        CacheFilter::Uncached => "uncached",
    }
}

fn jump_diff_file(state: &mut State, delta: isize) {
    let Some(detail) = state.pr_detail.as_ref() else {
        return;
    };
    let (_, files, hunks) = parse_diff(&detail.diff);
    if files.is_empty() {
        return;
    }
    let max = files.len().saturating_sub(1);
    let next = offset_index(state.diff_file_index, delta, max);
    state.diff_file_index = next;
    state.diff_hunk_index = hunks
        .iter()
        .position(|hunk| hunk.file_index == next)
        .unwrap_or(state.diff_hunk_index);
    state.diff_scroll = files[next].start_line.min(u16::MAX as usize) as u16;
}

fn jump_diff_hunk(state: &mut State, delta: isize) {
    let Some(detail) = state.pr_detail.as_ref() else {
        return;
    };
    let (_, files, hunks) = parse_diff(&detail.diff);
    if hunks.is_empty() {
        return;
    }
    let max = hunks.len().saturating_sub(1);
    let next = offset_index(state.diff_hunk_index, delta, max);
    state.diff_hunk_index = next;
    state.diff_file_index = hunks[next].file_index;
    state.diff_scroll = hunks[next].start_line.min(u16::MAX as usize) as u16;
    if state.diff_file_index >= files.len() {
        state.diff_file_index = files.len().saturating_sub(1);
    }
}

fn sync_diff_position(state: &mut State, files: &[DiffFile], hunks: &[DiffHunk]) {
    let scroll = state.diff_scroll as usize;
    if let Some((index, _)) = files
        .iter()
        .enumerate()
        .rev()
        .find(|(_, file)| file.start_line <= scroll)
    {
        state.diff_file_index = index;
    }
    if let Some((index, _)) = hunks
        .iter()
        .enumerate()
        .rev()
        .find(|(_, hunk)| hunk.start_line <= scroll)
    {
        state.diff_hunk_index = index;
    }
}

fn offset_index(current: usize, delta: isize, max: usize) -> usize {
    if delta.is_negative() {
        current.saturating_sub(delta.unsigned_abs())
    } else {
        current.saturating_add(delta as usize).min(max)
    }
}

fn parse_diff(diff: &str) -> (Vec<Line<'static>>, Vec<DiffFile>, Vec<DiffHunk>) {
    let mut lines = Vec::new();
    let mut files = Vec::new();
    let mut hunks = Vec::new();
    let mut current_file: Option<usize> = None;
    let mut pending_old_path: Option<String> = None;

    for raw_line in diff.lines() {
        let line_index = lines.len();
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
            lines.push(styled_diff_line(raw_line));
            continue;
        }
        if let Some(path) = raw_line.strip_prefix("+++ ") {
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

fn current_diff_file<'a>(state: &State, files: &'a [DiffFile]) -> Option<&'a DiffFile> {
    files.get(state.diff_file_index).or_else(|| files.first())
}

fn display_diff_path(file: &DiffFile) -> String {
    if file.old_path == file.new_path || file.old_path == "/dev/null" {
        file.new_path.clone()
    } else if file.new_path == "/dev/null" {
        file.old_path.clone()
    } else {
        format!("{} -> {}", file.old_path, file.new_path)
    }
}

fn open_selected_in_browser(state: &mut State) {
    let Some(url) = selected_github_url(state) else {
        state.status = "No selected item to open".to_string();
        return;
    };
    match open_url(&url) {
        Ok(()) => state.status = format!("Opened {url}"),
        Err(err) => state.status = format!("Failed to open browser: {err}"),
    }
}

fn selected_github_url(state: &State) -> Option<String> {
    let repo = state.config.repo.trim();
    if repo.is_empty() {
        return None;
    }
    match state.tab {
        Tab::PullRequests => {
            let pr = state.prs.get(state.selected_pr)?;
            Some(format!("https://github.com/{repo}/pull/{}", pr.number))
        }
        Tab::Issues => {
            let issue = state.issues.get(state.selected_issue)?;
            Some(format!("https://github.com/{repo}/issues/{}", issue.number))
        }
    }
}

fn open_url(url: &str) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd").args(["/C", "start", "", url]).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Command::new("xdg-open").arg(url).spawn()?;
    }
    Ok(())
}

fn relative_age(dt: &DateTime<Utc>) -> String {
    let secs = Utc::now().signed_duration_since(*dt).num_seconds().max(0);
    if secs < 3_600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3_600)
    } else if secs < 86_400 * 7 {
        format!("{}d", secs / 86_400)
    } else if secs < 86_400 * 30 {
        format!("{}w", secs / (86_400 * 7))
    } else if secs < 86_400 * 365 {
        format!("{}mo", secs / (86_400 * 30))
    } else {
        format!("{}y", secs / (86_400 * 365))
    }
}

fn author_color(author: &str) -> Color {
    const PALETTE: [Color; 7] = [
        Color::Rgb(80, 200, 200),  // teal
        Color::Rgb(100, 210, 100), // green
        Color::Rgb(200, 100, 200), // magenta
        Color::Rgb(100, 150, 240), // blue
        Color::Rgb(230, 200, 80),  // yellow
        Color::Rgb(80, 220, 180),  // cyan-green
        Color::Rgb(210, 130, 210), // light magenta
    ];
    let hash = author.bytes().fold(0usize, |acc, b| {
        acc.wrapping_mul(31).wrapping_add(b as usize)
    });
    PALETTE[hash % PALETTE.len()]
}

fn pad_right(s: &str, width: usize) -> String {
    format!("{:<width$}", s, width = width)
}

fn cache_marker(cached: bool) -> Span<'static> {
    if cached {
        Span::styled(
            " C",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("  ", Style::default().fg(TEXT_MUTED))
    }
}

fn has_column(columns: &[String], name: &str) -> bool {
    columns.is_empty() || columns.iter().any(|column| column == name)
}

fn pr_item(
    pr: &PrData,
    watch_paths: &[WatchedPathConfig],
    cached: bool,
    columns: &[String],
) -> ListItem<'static> {
    let (size_label, size_color) = match pr.size_if_known() {
        Some(PrSize::XS) => ("XS".to_string(), Color::Green),
        Some(PrSize::S) => ("S ".to_string(), Color::LightGreen),
        Some(PrSize::M) => ("M ".to_string(), Color::Yellow),
        Some(PrSize::L) => ("L ".to_string(), Color::LightRed),
        Some(PrSize::XL) => ("XL".to_string(), Color::Red),
        None => {
            // Fall back to first label when line counts aren't available from
            // the list endpoint. Slice to 6 chars without adding "..." suffix.
            let lbl = pr
                .labels
                .first()
                .map(|l| {
                    let end = l.char_indices().nth(6).map(|(i, _)| i).unwrap_or(l.len());
                    l[..end].to_string()
                })
                .unwrap_or_else(|| "·".to_string());
            (format!("{:<6}", lbl), Color::DarkGray)
        }
    };

    let author_raw = if pr.is_dependabot {
        "BOT".to_string()
    } else {
        pr.author.clone()
    };
    let a_color = if pr.is_dependabot {
        Color::DarkGray
    } else {
        author_color(&author_raw)
    };
    let author_display = pad_right(&truncate(&author_raw, 16), 16);

    let badges = watch_badges(&pr.files, watch_paths);
    let title_raw = if badges.is_empty() {
        pr.title.clone()
    } else {
        format!("{badges} {}", pr.title)
    };
    let title_display = pad_right(&truncate(&title_raw, 52), 52);

    let age = relative_age(&pr.created_at);

    let mut spans = vec![
        Span::styled(
            format!("#{:>4}", pr.number),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        cache_marker(cached),
        Span::styled("  ", Style::default().fg(TEXT_MUTED)),
    ];
    if has_column(columns, "title") {
        spans.push(Span::styled(
            title_display,
            Style::default().fg(TEXT_PRIMARY),
        ));
        spans.push(Span::styled("  ", Style::default().fg(TEXT_MUTED)));
    }
    if has_column(columns, "author") {
        spans.push(Span::styled(author_display, Style::default().fg(a_color)));
        spans.push(Span::styled("  ", Style::default().fg(TEXT_MUTED)));
    }
    if has_column(columns, "age") {
        spans.push(Span::styled(
            format!("{:>4}", age),
            Style::default().fg(TEXT_SECONDARY),
        ));
        spans.push(Span::styled("  ", Style::default().fg(TEXT_MUTED)));
    }
    if has_column(columns, "label") || has_column(columns, "size") {
        spans.push(Span::styled(
            size_label,
            Style::default().fg(size_color).add_modifier(Modifier::BOLD),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn watch_badges(files: &[String], watch_paths: &[WatchedPathConfig]) -> String {
    let mut seen = Vec::<String>::new();
    let mut badges = Vec::new();
    for watch_path in watch_paths {
        if seen.contains(&watch_path.label) {
            continue;
        }
        let prefix = format!("{}/", watch_path.path.trim_end_matches('/'));
        if files
            .iter()
            .any(|file| file == &watch_path.path || file.starts_with(&prefix))
        {
            seen.push(watch_path.label.clone());
            badges.push(format!("■ [{}]", watch_path.label));
        }
    }
    badges.join(" ")
}

fn issue_item(issue: &IssueData, cached: bool, columns: &[String]) -> ListItem<'static> {
    let (label_display, label_color) = match issue.label {
        IssueLabel::Bug => ("bug  ", Color::Red),
        IssueLabel::Question => ("quest", Color::Green),
        IssueLabel::Enhancement => ("enhnc", Color::LightCyan),
        IssueLabel::Feature => ("feat ", Color::Magenta),
        IssueLabel::Other => {
            let short = truncate(&issue.label_raw, 5);
            // label_raw is always known at list time; use a static fallback
            let _ = short;
            ("other", Color::DarkGray)
        }
    };
    let author_display = pad_right(&truncate(&issue.author, 16), 16);
    let title_display = pad_right(&truncate(&issue.title, 52), 52);
    let age = relative_age(&issue.created_at);
    let mut spans = vec![
        Span::styled(
            format!("#{:>4}", issue.number),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        cache_marker(cached),
        Span::styled("  ", Style::default().fg(TEXT_MUTED)),
    ];
    if has_column(columns, "title") {
        spans.push(Span::styled(
            title_display,
            Style::default().fg(TEXT_PRIMARY),
        ));
        spans.push(Span::styled("  ", Style::default().fg(TEXT_MUTED)));
    }
    if has_column(columns, "author") {
        spans.push(Span::styled(
            author_display,
            Style::default().fg(Color::Cyan),
        ));
        spans.push(Span::styled("  ", Style::default().fg(TEXT_MUTED)));
    }
    if has_column(columns, "age") {
        spans.push(Span::styled(
            format!("{:>4}", age),
            Style::default().fg(TEXT_SECONDARY),
        ));
        spans.push(Span::styled("  ", Style::default().fg(TEXT_MUTED)));
    }
    if has_column(columns, "label") {
        spans.push(Span::styled(
            label_display,
            Style::default()
                .fg(label_color)
                .add_modifier(Modifier::BOLD),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn section_line(title: &str) -> Line<'static> {
    Line::from(Span::styled(
        title.to_string(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
}

fn body_lines(text: &str) -> Vec<Line<'static>> {
    if text.trim().is_empty() {
        return vec![Line::from(Span::styled(
            "None",
            Style::default().fg(TEXT_MUTED),
        ))];
    }
    text.lines()
        .map(|line| {
            Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(TEXT_PRIMARY),
            ))
        })
        .collect()
}

fn pr_detail_lines(detail: &PrDetail, analysis: &PrAnalysis) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("PR #{}: ", detail.pr.number),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(detail.pr.title.clone(), Style::default().fg(TEXT_PRIMARY)),
        ]),
        Line::from(Span::styled(
            format!(
                "{} | +{} / -{} | {} files | {:?}",
                detail.pr.author,
                detail.pr.additions,
                detail.pr.deletions,
                detail.pr.changed_files,
                detail.pr.size()
            ),
            Style::default().fg(TEXT_SECONDARY),
        )),
        Line::from(""),
        section_line("Summary"),
    ];
    lines.extend(body_lines(&analysis.summary));
    lines.push(Line::from(""));
    lines.push(section_line("Security"));
    lines.extend(body_lines(&analysis.security_risks));
    lines.push(Line::from(""));
    lines.push(section_line("Code Quality"));
    lines.extend(body_lines(&analysis.code_quality));
    lines.push(Line::from(""));
    lines.push(section_line(&format!("Risk: {}", analysis.risk_level)));
    lines.extend(body_lines(&analysis.disruption_assessment));
    lines.push(Line::from(""));
    lines.push(section_line("Backwards Compatibility"));
    lines.extend(body_lines(&analysis.backwards_compatibility));
    lines.push(Line::from(""));
    lines.push(section_line(&format!(
        "Semver Impact: {}",
        analysis.semver_impact
    )));
    lines.push(Line::from(""));
    lines.push(section_line("Review Comment"));
    lines.extend(body_lines(&analysis.review_comment));
    lines
}

fn issue_detail_lines(detail: &IssueDetail, analysis: &IssueAnalysis) -> Vec<Line<'static>> {
    let severity = match analysis.severity {
        IssueSeverity::Critical => "Critical",
        IssueSeverity::High => "High",
        IssueSeverity::Medium => "Medium",
        IssueSeverity::Low => "Low",
        IssueSeverity::Info => "Info",
    };
    let label = if detail.issue.label_raw.is_empty() {
        "no label"
    } else {
        &detail.issue.label_raw
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("Issue #{}: ", detail.issue.number),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                detail.issue.title.clone(),
                Style::default().fg(TEXT_PRIMARY),
            ),
        ]),
        Line::from(Span::styled(
            format!(
                "{} | {} | {} comments",
                detail.issue.author, label, detail.issue.comment_count
            ),
            Style::default().fg(TEXT_SECONDARY),
        )),
        Line::from(""),
        section_line(&format!("Severity: {severity}")),
        Line::from(""),
        section_line("Overview"),
    ];
    lines.extend(body_lines(&analysis.overview));
    lines.push(Line::from(""));
    lines.push(section_line("Suspected Cause"));
    lines.extend(body_lines(&analysis.suspected_cause));
    lines.push(Line::from(""));
    lines.push(section_line("Suggested Fix"));
    lines.extend(body_lines(&analysis.suggested_fix));
    lines
}

fn pr_detail_text(state: &State) -> String {
    let Some(detail) = state.pr_detail.as_ref() else {
        return String::new();
    };
    let analysis = state.pr_analysis.clone().unwrap_or_default();
    pr_detail_lines(detail, &analysis)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn issue_detail_text(state: &State) -> String {
    let Some(detail) = state.issue_detail.as_ref() else {
        return String::new();
    };
    let analysis = state.issue_analysis.clone().unwrap_or_default();
    issue_detail_lines(detail, &analysis)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_pr_detail(frame: &mut Frame<'_>, state: &mut State, area: ratatui::layout::Rect) {
    let Some(detail) = &state.pr_detail else {
        frame.render_widget(Paragraph::new("No PR loaded."), area);
        return;
    };
    let lines = pr_detail_lines(detail, &state.pr_analysis.clone().unwrap_or_default());
    render_scrollable_lines(
        frame,
        area,
        " PR Detail ",
        lines,
        &mut state.pr_detail_scroll,
    );
}

fn render_issue_detail(frame: &mut Frame<'_>, state: &mut State, area: ratatui::layout::Rect) {
    let Some(detail) = &state.issue_detail else {
        frame.render_widget(Paragraph::new("No issue loaded."), area);
        return;
    };
    let lines = issue_detail_lines(detail, &state.issue_analysis.clone().unwrap_or_default());
    render_scrollable_lines(
        frame,
        area,
        " Issue Detail ",
        lines,
        &mut state.issue_detail_scroll,
    );
}

fn render_diff(frame: &mut Frame<'_>, state: &mut State, area: ratatui::layout::Rect) {
    let Some(detail) = state.pr_detail.as_ref() else {
        render_scrollable_lines(
            frame,
            area,
            " Diff ",
            vec![Line::from("No PR loaded.")],
            &mut state.diff_scroll,
        );
        return;
    };
    if detail.diff.is_empty() {
        render_scrollable_lines(
            frame,
            area,
            " Diff ",
            vec![Line::from("No diff available.")],
            &mut state.diff_scroll,
        );
        return;
    }
    let (lines, files, hunks) = parse_diff(&detail.diff);
    sync_diff_position(state, &files, &hunks);
    let (file_area, diff_area) = if area.width >= 100 && !files.is_empty() {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(34), Constraint::Min(0)])
            .split(area);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, area)
    };
    if let Some(file_area) = file_area {
        render_diff_file_panel(frame, file_area, &files, state.diff_file_index);
    }
    if let Some(file) = current_diff_file(state, &files) {
        let file_hunks = hunks
            .iter()
            .filter(|hunk| hunk.file_index == state.diff_file_index)
            .count();
        let title = format!(
            " Diff {}/{}  {}  hunks {} ",
            state.diff_file_index + 1,
            files.len(),
            display_diff_path(file),
            file_hunks
        );
        render_scrollable_lines(frame, diff_area, &title, lines, &mut state.diff_scroll);
    } else {
        render_scrollable_lines(frame, diff_area, " Diff ", lines, &mut state.diff_scroll);
    }
}

fn render_diff_file_panel(frame: &mut Frame<'_>, area: Rect, files: &[DiffFile], selected: usize) {
    let items = files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let marker = if index == selected { "> " } else { "  " };
            let path = truncate(
                &display_diff_path(file),
                area.width.saturating_sub(5) as usize,
            );
            ListItem::new(Line::from(vec![
                Span::styled(
                    marker,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(path, Style::default().fg(TEXT_PRIMARY)),
            ]))
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items).block(panel_block(" Files ")), area);
}

fn render_help(frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    let text = "Up/Down   Navigate list items or scroll detail text\nPgUp/PgDn Scroll detail text faster\nLeft/Right Change page\nEnter     Open selected item\no         Open selected item in GitHub\nTab       Switch PRs/issues\nr         Refresh current view or retry failed load\ns         Toggle issue sort\ni         Runtime info\n?         Help\nc         Copy review/fix on detail screens\nd         View PR diff from PR detail\nn/p       Next/previous diff file\n[/]       Next/previous diff hunk\nEsc       Back/close\nq q       Quit";
    frame.render_widget(
        Paragraph::new(text).block(Block::default().title(" Help ").borders(Borders::ALL)),
        area,
    );
}

fn render_info(frame: &mut Frame<'_>, state: &State, area: ratatui::layout::Rect) {
    let config_paths = if state.config.config_paths.is_empty() {
        "(none)".to_string()
    } else {
        state
            .config
            .config_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let provider = resolved_provider(&state.config);
    let text = format!(
        "Repository: {}\nProvider: {}\nModel: {}\nBase URL: {}\nAPI Key Env: {}\nPrompt Version: {}\nCache Enabled: {}\nCache Dir: {}\nAnalysis TTL: {}d\nCache Max: {} MB\nList Cache TTL: {}s\nPoll Interval: {}s\nConfig:\n{}",
        state.config.repo,
        provider.provider,
        provider.model,
        provider.base_url,
        if provider.api_key_env.is_empty() {
            "(provider default)"
        } else {
            &provider.api_key_env
        },
        state.config.prompt_version,
        state.config.cache.enabled,
        cache::cache_dir(&state.config.cache).display(),
        state.config.cache.analysis_ttl_days,
        state.config.cache.max_size_mb,
        state.config.cache_ttl_seconds,
        state.config.poll_interval_seconds,
        config_paths
    );
    frame.render_widget(
        Paragraph::new(text).block(Block::default().title(" Info ").borders(Borders::ALL)),
        area,
    );
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn copy_text(text: &str, status: &mut String) {
    if text.trim().is_empty() {
        *status = "Nothing to copy".to_string();
        return;
    }
    match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text.to_string())) {
        Ok(()) => *status = "Copied to clipboard".to_string(),
        Err(err) => *status = format!("Failed to copy to clipboard: {err}"),
    }
}

fn pages(total: u64) -> u64 {
    std::cmp::max(1, total.div_ceil(15))
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        let mut out = value
            .chars()
            .take(max.saturating_sub(3))
            .collect::<String>();
        out.push_str("...");
        out
    }
}

fn resolved_provider(config: &AppConfig) -> ai::ProviderConfig {
    ai::resolve_provider_config(
        &config.provider,
        &config.model,
        &config.base_url,
        &config.api_key_env,
    )
}
