use std::{
    io::{self, Stdout},
    path::PathBuf,
    time::Duration,
};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};

use crate::{
    ai, cache,
    config::WatchedPathConfig,
    github::GitHubClient,
    models::{
        IssueAnalysis, IssueData, IssueDetail, IssueLabel, IssueSeverity, PrAnalysis, PrData,
        PrDetail, PrSize,
    },
    sorting,
};

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub repo: String,
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    pub prompt_version: String,
    pub cache_ttl_seconds: u64,
    pub poll_interval_seconds: u64,
    pub config_paths: Vec<PathBuf>,
    pub watch_paths: Vec<WatchedPathConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    PullRequests,
    Issues,
}

#[derive(Debug, Clone)]
enum Screen {
    List,
    Help,
    Info,
    PrDetail,
    IssueDetail,
    Diff,
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
    issue_newest: bool,
    status: String,
    pr_detail: Option<PrDetail>,
    pr_analysis: Option<PrAnalysis>,
    issue_detail: Option<IssueDetail>,
    issue_analysis: Option<IssueAnalysis>,
    quit_armed: bool,
}

pub async fn run(config: AppConfig, client: GitHubClient) -> Result<()> {
    client.ensure_repo_access().await?;
    let mut terminal = setup_terminal()?;
    let mut state = State::new(config, client);
    state.load_initial().await;

    let result = run_loop(&mut terminal, &mut state).await;
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
            issue_newest: true,
            status: "Loading repository data...".to_string(),
            pr_detail: None,
            pr_analysis: None,
            issue_detail: None,
            issue_analysis: None,
            quit_armed: false,
        }
    }

    async fn load_initial(&mut self) {
        self.load_prs(false).await;
        self.load_issues(false).await;
        self.status = "Ready".to_string();
    }

    async fn load_prs(&mut self, force: bool) {
        let max_age = (!force).then_some(Duration::from_secs(self.config.cache_ttl_seconds));
        if let Some((mut prs, total)) =
            cache::get_cached_pr_list(&self.config.repo, self.pr_page, max_age)
        {
            sorting::sort_prs(&mut prs);
            self.prs = prs;
            self.pr_total = total;
            return;
        }
        self.status = "Fetching pull requests...".to_string();
        match self
            .client
            .list_prs(self.pr_page, 15, !self.config.watch_paths.is_empty())
            .await
        {
            Ok((mut prs, total)) => {
                cache::save_pr_list(&self.config.repo, self.pr_page, &prs, total);
                sorting::sort_prs(&mut prs);
                self.prs = prs;
                self.pr_total = total;
                self.selected_pr = self.selected_pr.min(self.prs.len().saturating_sub(1));
                self.status = "Pull requests loaded".to_string();
            }
            Err(err) => self.status = format!("Failed to load PRs: {err}"),
        }
    }

    async fn load_issues(&mut self, force: bool) {
        let direction = if self.issue_newest { "desc" } else { "asc" };
        let max_age = (!force).then_some(Duration::from_secs(self.config.cache_ttl_seconds));
        if let Some((issues, total)) =
            cache::get_cached_issue_list(&self.config.repo, self.issue_page, direction, max_age)
        {
            self.issues = issues;
            self.issue_total = total;
            return;
        }
        self.status = "Fetching issues...".to_string();
        match self
            .client
            .list_issues(self.issue_page, 15, direction)
            .await
        {
            Ok((issues, total)) => {
                cache::save_issue_list(
                    &self.config.repo,
                    self.issue_page,
                    &issues,
                    total,
                    direction,
                );
                self.issues = issues;
                self.issue_total = total;
                self.selected_issue = self.selected_issue.min(self.issues.len().saturating_sub(1));
                self.status = "Issues loaded".to_string();
            }
            Err(err) => self.status = format!("Failed to load issues: {err}"),
        }
    }

    async fn open_selected(&mut self) {
        match self.tab {
            Tab::PullRequests => {
                let Some(pr) = self.prs.get(self.selected_pr).cloned() else {
                    return;
                };
                self.status = format!("Loading PR #{}...", pr.number);
                let head_sha = if pr.head_sha.is_empty() {
                    self.client
                        .get_pr_head_sha(pr.number)
                        .await
                        .unwrap_or_default()
                } else {
                    pr.head_sha.clone()
                };
                let provider = resolved_provider(&self.config);
                if let Some(cached) = cache::get_cached_pr_analysis(
                    &self.config.repo,
                    pr.number,
                    &head_sha,
                    &provider.provider,
                    &provider.model,
                    &self.config.prompt_version,
                    "pr-analysis-v1",
                ) {
                    match self.client.get_pr_summary(pr.number, false).await {
                        Ok(detail) => {
                            self.pr_detail = Some(detail);
                            self.pr_analysis = Some(ai::pr_analysis_from_value(&cached));
                            self.screen = Screen::PrDetail;
                            self.status = "PR analysis loaded from cache".to_string();
                        }
                        Err(err) => self.status = format!("Failed to load PR metadata: {err}"),
                    }
                    return;
                }
                match self.client.get_pr_detail(pr.number).await {
                    Ok(detail) => {
                        let analysis = ai::analyze_pr(
                            &detail,
                            &self.config.provider,
                            &self.config.model,
                            &self.config.base_url,
                            &self.config.api_key_env,
                            &self.config.repo,
                            &self.config.prompt_version,
                        )
                        .await;
                        self.pr_detail = Some(detail);
                        self.pr_analysis = Some(analysis);
                        self.screen = Screen::PrDetail;
                        self.status = "PR analysis loaded".to_string();
                    }
                    Err(err) => self.status = format!("Failed to load PR: {err}"),
                }
            }
            Tab::Issues => {
                let Some(issue) = self.issues.get(self.selected_issue).cloned() else {
                    return;
                };
                self.status = format!("Loading issue #{}...", issue.number);
                let provider = resolved_provider(&self.config);
                if let Some(cached) = cache::get_cached_issue_analysis(
                    &self.config.repo,
                    issue.number,
                    &provider.provider,
                    &provider.model,
                    &self.config.prompt_version,
                    "issue-analysis-v1",
                ) {
                    match self.client.get_issue_summary(issue.number).await {
                        Ok(detail) => {
                            self.issue_detail = Some(detail);
                            self.issue_analysis = Some(ai::issue_analysis_from_value(&cached));
                            self.screen = Screen::IssueDetail;
                            self.status = "Issue analysis loaded from cache".to_string();
                        }
                        Err(err) => self.status = format!("Failed to load issue metadata: {err}"),
                    }
                    return;
                }
                match self.client.get_issue_detail(issue.number).await {
                    Ok(detail) => {
                        let analysis = ai::analyze_issue(
                            &detail,
                            &self.config.provider,
                            &self.config.model,
                            &self.config.base_url,
                            &self.config.api_key_env,
                            &self.config.repo,
                            &self.config.prompt_version,
                        )
                        .await;
                        self.issue_detail = Some(detail);
                        self.issue_analysis = Some(analysis);
                        self.screen = Screen::IssueDetail;
                        self.status = "Issue analysis loaded".to_string();
                    }
                    Err(err) => self.status = format!("Failed to load issue: {err}"),
                }
            }
        }
    }
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    state: &mut State,
) -> Result<()> {
    loop {
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
                KeyCode::Tab => {
                    state.tab = if state.tab == Tab::PullRequests {
                        Tab::Issues
                    } else {
                        Tab::PullRequests
                    };
                    state.quit_armed = false;
                }
                KeyCode::Char('s') if state.tab == Tab::Issues => {
                    state.issue_newest = !state.issue_newest;
                    state.issue_page = 0;
                    state.load_issues(true).await;
                }
                KeyCode::Char('r') => match state.tab {
                    Tab::PullRequests => state.load_prs(true).await,
                    Tab::Issues => state.load_issues(true).await,
                },
                KeyCode::Down => match state.tab {
                    Tab::PullRequests => {
                        state.selected_pr =
                            (state.selected_pr + 1).min(state.prs.len().saturating_sub(1))
                    }
                    Tab::Issues => {
                        state.selected_issue =
                            (state.selected_issue + 1).min(state.issues.len().saturating_sub(1))
                    }
                },
                KeyCode::Up => match state.tab {
                    Tab::PullRequests => state.selected_pr = state.selected_pr.saturating_sub(1),
                    Tab::Issues => state.selected_issue = state.selected_issue.saturating_sub(1),
                },
                KeyCode::Right => match state.tab {
                    Tab::PullRequests if ((state.pr_page + 1) * 15) < state.pr_total as usize => {
                        state.pr_page += 1;
                        state.selected_pr = 0;
                        state.load_prs(false).await;
                    }
                    Tab::Issues if ((state.issue_page + 1) * 15) < state.issue_total as usize => {
                        state.issue_page += 1;
                        state.selected_issue = 0;
                        state.load_issues(false).await;
                    }
                    _ => {}
                },
                KeyCode::Left => match state.tab {
                    Tab::PullRequests if state.pr_page > 0 => {
                        state.pr_page -= 1;
                        state.selected_pr = 0;
                        state.load_prs(false).await;
                    }
                    Tab::Issues if state.issue_page > 0 => {
                        state.issue_page -= 1;
                        state.selected_issue = 0;
                        state.load_issues(false).await;
                    }
                    _ => {}
                },
                KeyCode::Enter => state.open_selected().await,
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
                KeyCode::Char('c') => copy_text(
                    state
                        .pr_analysis
                        .as_ref()
                        .map(|a| a.review_comment.as_str())
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
                KeyCode::Char('c') => copy_text(
                    state
                        .issue_analysis
                        .as_ref()
                        .map(|a| a.suggested_fix.as_str())
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

fn render(frame: &mut Frame<'_>, state: &State) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);

    let titles = ["Pull Requests", "Issues"]
        .into_iter()
        .map(Line::from)
        .collect::<Vec<_>>();
    let selected = if state.tab == Tab::PullRequests { 0 } else { 1 };
    frame.render_widget(
        Tabs::new(titles)
            .select(selected)
            .block(
                Block::default()
                    .title(format!(" wftt - {} ", state.config.repo))
                    .borders(Borders::ALL),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        chunks[0],
    );

    match state.screen {
        Screen::List => render_list(frame, state, chunks[1]),
        Screen::Help => render_help(frame, chunks[1]),
        Screen::Info => render_info(frame, state, chunks[1]),
        Screen::PrDetail => render_pr_detail(frame, state, chunks[1]),
        Screen::IssueDetail => render_issue_detail(frame, state, chunks[1]),
        Screen::Diff => render_diff(frame, state, chunks[1]),
    }

    frame.render_widget(
        Paragraph::new(state.status.as_str()).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn render_list(frame: &mut Frame<'_>, state: &State, area: ratatui::layout::Rect) {
    match state.tab {
        Tab::PullRequests => {
            let items = state
                .prs
                .iter()
                .map(|pr| pr_item(pr, &state.config.watch_paths))
                .collect::<Vec<_>>();
            let mut list_state = ListState::default();
            list_state.select((!items.is_empty()).then_some(state.selected_pr));
            let total_pages = pages(state.pr_total);
            frame.render_stateful_widget(
                List::new(items)
                    .block(
                        Block::default()
                            .title(format!(
                                " Pull Requests - Page {} / {} ",
                                state.pr_page + 1,
                                total_pages
                            ))
                            .borders(Borders::ALL),
                    )
                    .highlight_style(Style::default().bg(Color::DarkGray)),
                area,
                &mut list_state,
            );
        }
        Tab::Issues => {
            let items = state.issues.iter().map(issue_item).collect::<Vec<_>>();
            let mut list_state = ListState::default();
            list_state.select((!items.is_empty()).then_some(state.selected_issue));
            let order = if state.issue_newest {
                "Newest first"
            } else {
                "Oldest first"
            };
            frame.render_stateful_widget(
                List::new(items)
                    .block(
                        Block::default()
                            .title(format!(
                                " Issues - {order} - Page {} / {} ",
                                state.issue_page + 1,
                                pages(state.issue_total)
                            ))
                            .borders(Borders::ALL),
                    )
                    .highlight_style(Style::default().bg(Color::DarkGray)),
                area,
                &mut list_state,
            );
        }
    }
}

fn pr_item(pr: &PrData, watch_paths: &[WatchedPathConfig]) -> ListItem<'static> {
    let size = match pr.size() {
        PrSize::XS | PrSize::S => Color::Green,
        PrSize::M => Color::Yellow,
        PrSize::L | PrSize::XL => Color::Red,
    };
    let author = if pr.is_dependabot {
        "BOT".to_string()
    } else {
        pr.author.clone()
    };
    let badges = watch_badges(&pr.files, watch_paths);
    let title = if badges.is_empty() {
        truncate(&pr.title, 62)
    } else {
        truncate(&format!("{badges} {}", pr.title), 62)
    };
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("#{} ", pr.number),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(title),
        Span::styled(format!("  {author}  "), Style::default().fg(Color::Cyan)),
        Span::styled(
            pr.created_at.format("%d/%b").to_string(),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("  {:?}", pr.size()),
            Style::default().fg(size).add_modifier(Modifier::BOLD),
        ),
    ]))
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

fn issue_item(issue: &IssueData) -> ListItem<'static> {
    let color = match issue.label {
        IssueLabel::Bug => Color::Red,
        IssueLabel::Question => Color::Green,
        IssueLabel::Enhancement => Color::Blue,
        IssueLabel::Feature => Color::Magenta,
        IssueLabel::Other => Color::DarkGray,
    };
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("#{} ", issue.number),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(truncate(&issue.title, 62)),
        Span::styled(
            format!("  {}  ", issue.author),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(
            issue.created_at.format("%d/%b").to_string(),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("  || {}", issue.label_raw),
            Style::default().fg(color),
        ),
    ]))
}

fn render_pr_detail(frame: &mut Frame<'_>, state: &State, area: ratatui::layout::Rect) {
    let Some(detail) = &state.pr_detail else {
        frame.render_widget(Paragraph::new("No PR loaded."), area);
        return;
    };
    let analysis = state.pr_analysis.clone().unwrap_or_default();
    let text = format!(
        "PR #{}: {}\n{} | +{} / -{} | {} files | {:?}\n\nSummary\n{}\n\nSecurity\n{}\n\nCode Quality\n{}\n\nRisk: {}\n{}\n\nBackwards Compatibility\n{}\n\nSemver Impact: {}\n\nReview Comment\n{}\n\n[d] Diff  [c] Copy review  [Esc] Back  [q q] Quit",
        detail.pr.number,
        detail.pr.title,
        detail.pr.author,
        detail.pr.additions,
        detail.pr.deletions,
        detail.pr.changed_files,
        detail.pr.size(),
        analysis.summary,
        analysis.security_risks,
        analysis.code_quality,
        analysis.risk_level,
        analysis.disruption_assessment,
        analysis.backwards_compatibility,
        analysis.semver_impact,
        analysis.review_comment,
    );
    frame.render_widget(
        Paragraph::new(text)
            .block(Block::default().title(" PR Detail ").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_issue_detail(frame: &mut Frame<'_>, state: &State, area: ratatui::layout::Rect) {
    let Some(detail) = &state.issue_detail else {
        frame.render_widget(Paragraph::new("No issue loaded."), area);
        return;
    };
    let analysis = state.issue_analysis.clone().unwrap_or_default();
    let severity = match analysis.severity {
        IssueSeverity::Critical => "Critical",
        IssueSeverity::High => "High",
        IssueSeverity::Medium => "Medium",
        IssueSeverity::Low => "Low",
        IssueSeverity::Info => "Info",
    };
    let text = format!(
        "Issue #{}: {}\n{} | {} | {} comments\n\nSeverity: {}\n\nOverview\n{}\n\nSuspected Cause\n{}\n\nSuggested Fix\n{}\n\n[c] Copy fix  [Esc] Back  [q q] Quit",
        detail.issue.number,
        detail.issue.title,
        detail.issue.author,
        if detail.issue.label_raw.is_empty() {
            "no label"
        } else {
            &detail.issue.label_raw
        },
        detail.issue.comment_count,
        severity,
        analysis.overview,
        analysis.suspected_cause,
        analysis.suggested_fix,
    );
    frame.render_widget(
        Paragraph::new(text)
            .block(
                Block::default()
                    .title(" Issue Detail ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_diff(frame: &mut Frame<'_>, state: &State, area: ratatui::layout::Rect) {
    let diff = state
        .pr_detail
        .as_ref()
        .map(|d| {
            if d.diff.is_empty() {
                "No diff available.".to_string()
            } else {
                d.diff.clone()
            }
        })
        .unwrap_or_else(|| "No PR loaded.".to_string());
    frame.render_widget(
        Paragraph::new(diff)
            .block(
                Block::default()
                    .title(" Diff - Esc Back ")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_help(frame: &mut Frame<'_>, area: ratatui::layout::Rect) {
    let text = "Up/Down   Navigate list items\nLeft/Right Change page\nEnter     Open selected item\nTab       Switch PRs/issues\nr         Refresh current view\ns         Toggle issue sort\ni         Runtime info\n?         Help\nc         Copy review/fix on detail screens\nd         View PR diff from PR detail\nEsc       Back/close\nq q       Quit";
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
        "Repository: {}\nProvider: {}\nModel: {}\nBase URL: {}\nAPI Key Env: {}\nPrompt Version: {}\nCache TTL: {}s\nPoll Interval: {}s\nConfig:\n{}",
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
