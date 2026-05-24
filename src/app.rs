use std::{
    io::{self, Stdout},
    path::PathBuf,
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
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap},
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
    pr_smart_sort: bool,
    status: String,
    pr_detail: Option<PrDetail>,
    pr_analysis: Option<PrAnalysis>,
    issue_detail: Option<IssueDetail>,
    issue_analysis: Option<IssueAnalysis>,
    quit_armed: bool,
}

pub async fn run(config: AppConfig, client: GitHubClient) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let mut state = State::new(config, client);

    let result = async {
        terminal.draw(|frame| render(frame, &state))?;
        state.status = "Connecting to GitHub...".to_string();
        terminal.draw(|frame| render(frame, &state))?;
        state.client.ensure_repo_access().await?;
        state.status = "Loading pull requests...".to_string();
        terminal.draw(|frame| render(frame, &state))?;
        state.load_prs(false).await;
        terminal.draw(|frame| render(frame, &state))?;
        state.status = "Loading issues...".to_string();
        terminal.draw(|frame| render(frame, &state))?;
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
            issue_newest: true,
            pr_smart_sort: true,
            status: "Loading repository data...".to_string(),
            pr_detail: None,
            pr_analysis: None,
            issue_detail: None,
            issue_analysis: None,
            quit_armed: false,
        }
    }

    fn apply_pr_sort(&self, prs: &mut Vec<PrData>) {
        if self.pr_smart_sort {
            sorting::sort_prs(prs);
        } else {
            prs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
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
        let direction = if self.issue_newest { "desc" } else { "asc" };
        if !force {
            if let Some((issues, total)) = cache::get_cached_issue_list(
                &self.config.cache,
                &self.config.repo,
                self.issue_page,
                direction,
                Some(Duration::from_secs(self.config.cache_ttl_seconds)),
            ) {
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
                cache::save_issue_list(
                    &self.config.cache,
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
                    &self.config.cache,
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
                            &self.config.cache,
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
                    &self.config.cache,
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
                            &self.config.cache,
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
                Style::default().fg(TEXT_SECONDARY).add_modifier(Modifier::BOLD),
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
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        chunks[1],
    );

    match state.screen {
        Screen::List => render_list(frame, state, chunks[2]),
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
            ("i", "info"),
            ("?", "help"),
            ("q q", "quit"),
        ]),
        Screen::List => hint_spans(&[
            ("Enter", "open"),
            ("Tab", "switch"),
            ("r", "refresh"),
            ("s", "newest/oldest"),
            ("?", "help"),
            ("q q", "quit"),
        ]),
        Screen::PrDetail => hint_spans(&[
            ("d", "diff"),
            ("c", "copy review"),
            ("Esc", "back"),
            ("q q", "quit"),
        ]),
        Screen::IssueDetail => hint_spans(&[
            ("c", "copy fix"),
            ("Esc", "back"),
            ("q q", "quit"),
        ]),
        Screen::Diff => hint_spans(&[("Esc", "back"), ("q q", "quit")]),
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
            let title = format!(
                " Pull Requests  page {} / {}  total {} ",
                state.pr_page + 1,
                total_pages,
                state.pr_total
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
                    &mut list_state,
                );
            }
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
            let title = format!(
                " Issues  {order}  page {} / {}  total {} ",
                state.issue_page + 1,
                pages(state.issue_total),
                state.issue_total
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
                    &mut list_state,
                );
            }
        }
    }
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

fn relative_age(dt: &DateTime<Utc>) -> String {
    let secs = Utc::now()
        .signed_duration_since(*dt)
        .num_seconds()
        .max(0);
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
        Color::Rgb(80, 200, 200),   // teal
        Color::Rgb(100, 210, 100),  // green
        Color::Rgb(200, 100, 200),  // magenta
        Color::Rgb(100, 150, 240),  // blue
        Color::Rgb(230, 200, 80),   // yellow
        Color::Rgb(80, 220, 180),   // cyan-green
        Color::Rgb(210, 130, 210),  // light magenta
    ];
    let hash = author
        .bytes()
        .fold(0usize, |acc, b| acc.wrapping_mul(31).wrapping_add(b as usize));
    PALETTE[hash % PALETTE.len()]
}

fn pad_right(s: &str, width: usize) -> String {
    format!("{:<width$}", s, width = width)
}

fn pr_item(pr: &PrData, watch_paths: &[WatchedPathConfig]) -> ListItem<'static> {
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
    let title_display = pad_right(&truncate(&title_raw, 54), 54);

    let age = relative_age(&pr.created_at);

    ListItem::new(Line::from(vec![
        Span::styled(
            format!("#{:>4}", pr.number),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default().fg(TEXT_MUTED)),
        Span::styled(title_display, Style::default().fg(TEXT_PRIMARY)),
        Span::styled("  ", Style::default().fg(TEXT_MUTED)),
        Span::styled(author_display, Style::default().fg(a_color)),
        Span::styled("  ", Style::default().fg(TEXT_MUTED)),
        Span::styled(
            format!("{:>4}", age),
            Style::default().fg(TEXT_SECONDARY),
        ),
        Span::styled("  ", Style::default().fg(TEXT_MUTED)),
        Span::styled(
            size_label,
            Style::default()
                .fg(size_color)
                .add_modifier(Modifier::BOLD),
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
    let title_display = pad_right(&truncate(&issue.title, 54), 54);
    let age = relative_age(&issue.created_at);
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("#{:>4}", issue.number),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default().fg(TEXT_MUTED)),
        Span::styled(title_display, Style::default().fg(TEXT_PRIMARY)),
        Span::styled("  ", Style::default().fg(TEXT_MUTED)),
        Span::styled(author_display, Style::default().fg(Color::Cyan)),
        Span::styled("  ", Style::default().fg(TEXT_MUTED)),
        Span::styled(
            format!("{:>4}", age),
            Style::default().fg(TEXT_SECONDARY),
        ),
        Span::styled("  ", Style::default().fg(TEXT_MUTED)),
        Span::styled(
            label_display,
            Style::default()
                .fg(label_color)
                .add_modifier(Modifier::BOLD),
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
