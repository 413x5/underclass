//! Read-only terminal monitor for a running underclass server.
use crate::config::Config;
use crate::models::{AccountStatus, BackendId};
use crate::monitor::{MonitorAccount, MonitorSnapshot};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Sparkline, Table, TableState, Wrap};
use std::io::IsTerminal;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const BACKGROUND: Color = Color::Rgb(12, 16, 26);
const PANEL: Color = Color::Rgb(20, 27, 40);
const BORDER: Color = Color::Rgb(57, 70, 91);
const TEXT: Color = Color::Rgb(222, 231, 242);
const MUTED: Color = Color::Rgb(139, 154, 176);
const CYAN: Color = Color::Rgb(92, 216, 229);
const PURPLE: Color = Color::Rgb(185, 138, 245);
const GREEN: Color = Color::Rgb(111, 217, 151);
const YELLOW: Color = Color::Rgb(246, 196, 101);
const RED: Color = Color::Rgb(245, 111, 125);

fn panel() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(BORDER))
        .title_style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD))
        .style(Style::default().fg(TEXT).bg(PANEL))
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        ratatui::restore();
    }
}

enum Update {
    Snapshot(MonitorSnapshot),
    Error(String),
    Notice(Notice),
}

/// Outcome of an operator action, shown in the footer until it expires.
struct Notice {
    text: String,
    ok: bool,
    at: Instant,
}

impl Notice {
    const TTL: Duration = Duration::from_secs(6);

    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ok: true,
            at: Instant::now(),
        }
    }

    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            ok: false,
            at: Instant::now(),
        }
    }

    fn visible(&self) -> bool {
        self.at.elapsed() < Self::TTL
    }
}

/// An enable/disable the operator has asked for and not yet confirmed.
struct Confirm {
    id: String,
    label: String,
    enable: bool,
}

impl Confirm {
    fn verb(&self) -> &'static str {
        if self.enable { "Enable" } else { "Disable" }
    }
}

/// The admin API used for the one write this dashboard can make. It is deliberately separate from
/// [`MonitorTarget`]: the monitor socket is read-only, so an operator action always goes over
/// authenticated HTTP even when the snapshot arrives over the socket.
#[derive(Clone)]
struct Admin {
    client: reqwest::blocking::Client,
    url: String,
    token: String,
}

impl Admin {
    fn connect(config: &Config, url_override: Option<String>) -> Result<Self, String> {
        let (url, token) = admin_http_target(config, url_override)?;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("HTTP client: {e}"))?;
        Ok(Self { client, url, token })
    }

    fn set_enabled(&self, id: &str, enable: bool) -> Result<(), String> {
        let url = action_url(&self.url, id, enable)?;
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.token)
            .send()
            .map_err(|e| format!("cannot reach {}: {e}", self.url))?;
        match response.status() {
            status if status.is_success() => Ok(()),
            reqwest::StatusCode::UNAUTHORIZED => Err("admin token rejected (401)".into()),
            reqwest::StatusCode::NOT_FOUND => Err("account no longer exists".into()),
            status => Err(format!("server returned {status}")),
        }
    }
}

/// @cc [owner:ghuntley,label:security] account-id-is-one-path-segment
/// `action_url` MUST reject an account id that is empty or contains anything other than ASCII
/// alphanumerics, `-`, or `_`, so an id cannot add a path segment, a query, or a traversal to the
/// admin URL.
fn action_url(base: &str, id: &str, enable: bool) -> Result<String, String> {
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("account id is not a single path segment".into());
    }
    let action = if enable { "enable" } else { "disable" };
    Ok(format!("{base}/admin/api/accounts/{id}/{action}"))
}

enum MonitorTarget {
    Socket(PathBuf),
    Http { url: String, token: String },
}

fn is_socket(path: &Path) -> bool {
    path.symlink_metadata()
        .is_ok_and(|metadata| metadata.file_type().is_socket())
}

/// @cc [owner:ghuntley,label:security] top-local-before-token
/// Without `--url`, `top` MUST prefer an available local monitor socket and send no admin token
/// over it. With `--url`, it MUST use HTTP(S) and require an explicit environment token.
fn monitor_target(config: &Config, url_override: Option<String>) -> Result<MonitorTarget, String> {
    if url_override.is_none() {
        let local_socket = config.monitor_socket_path();
        if is_socket(&local_socket) {
            return Ok(MonitorTarget::Socket(local_socket));
        }
        let system_socket = PathBuf::from(crate::config::SYSTEM_MONITOR_SOCKET);
        if is_socket(&system_socket) {
            return Ok(MonitorTarget::Socket(system_socket));
        }
    }
    let (url, token) = admin_http_target(config, url_override)?;
    Ok(MonitorTarget::Http { url, token })
}

/// Resolve the admin HTTP origin and bearer token for a CLI command that must talk to a running
/// server. Without `--url` the token comes from config or the local database; with `--url` it
/// MUST come from `UNDERCLASS_UI_TOKEN` so a database token is never sent to a remote host.
pub fn admin_http_target(
    config: &Config,
    url_override: Option<String>,
) -> Result<(String, String), String> {
    let url = match url_override.as_deref() {
        Some(url) => url.trim_end_matches('/').to_string(),
        None => {
            let bind = config
                .bind
                .replace("0.0.0.0", "127.0.0.1")
                .replace("[::]", "[::1]");
            format!("http://{bind}")
        }
    };
    let parsed = reqwest::Url::parse(&url).map_err(|_| "invalid server URL")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || parsed.username() != ""
        || parsed.password().is_some()
        || !matches!(parsed.path(), "" | "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(
            "server URL must be an HTTP(S) origin without credentials, path, or query".into(),
        );
    }
    let token = if url_override.is_some() {
        std::env::var("UNDERCLASS_UI_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
    } else {
        config
            .ui_token
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| local_token(config))
    }
    .ok_or("monitor socket unavailable and admin token unavailable; start underclass serve or set UNDERCLASS_UI_TOKEN")?;
    Ok((url, token))
}

/// @cc [owner:ghuntley,label:cli] top-observes-only
/// Dashboard data MUST be obtained only through GET requests to the local monitor socket or
/// authenticated HTTP, and `run` MUST leave the local SQLite database untouched and restore
/// terminal state on every normal exit or error.
///
/// @cc [owner:ghuntley,label:cli] top-operator-toggle
/// The only write `top` MAY perform is an operator's confirmed enable/disable of the selected
/// account, and it MUST go through the running server's authenticated admin API
/// (`POST /admin/api/accounts/{id}/enable|disable`) - never through the monitor socket, which
/// stays read-only, and never by touching the database. A request MUST be issued only after an
/// explicit `y` confirmation, and `n`/`Esc` MUST cancel it without contacting the server.
pub fn run(url_override: Option<String>) -> Result<(), String> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err("underclass top requires an interactive terminal".into());
    }
    let config = Config::load();
    let target = monitor_target(&config, url_override.clone())?;
    // Built once so the key binding can report a missing token immediately instead of at the
    // moment the operator asks for it. It stays separate from the read-only monitor target above:
    // over HTTP they share an origin and token, over the socket only this side is authenticated.
    let admin = Admin::connect(&config, url_override);
    let mut client_builder = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .redirect(reqwest::redirect::Policy::none());
    if let MonitorTarget::Socket(path) = &target {
        client_builder = client_builder.unix_socket(path.clone());
    }
    let client = client_builder
        .build()
        .map_err(|e| format!("HTTP client: {e}"))?;
    let (tx, rx) = mpsc::channel();
    let action_tx = tx.clone();
    let worker_url = match &target {
        MonitorTarget::Socket(_) => "http://localhost/monitor".to_string(),
        MonitorTarget::Http { url, .. } => format!("{url}/admin/api/monitor"),
    };
    let worker = std::thread::spawn(move || {
        loop {
            let started = Instant::now();
            let request = client.get(&worker_url);
            let request = match &target {
                MonitorTarget::Socket(_) => request,
                MonitorTarget::Http { token, .. } => request.bearer_auth(token),
            };
            let update = match request.send() {
                Ok(response) if response.status().is_success() => {
                    match response.json::<MonitorSnapshot>() {
                        Ok(snapshot) => Update::Snapshot(snapshot),
                        Err(_) => Update::Error("invalid monitor response".into()),
                    }
                }
                Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                    Update::Error("admin token rejected (401)".into())
                }
                Ok(response) => Update::Error(format!("server returned {}", response.status())),
                Err(_) => Update::Error("server unavailable; retrying".into()),
            };
            if tx.send(update).is_err() {
                break;
            }
            let pause = Duration::from_secs(1).saturating_sub(started.elapsed());
            if !pause.is_zero() {
                std::thread::sleep(pause);
            }
        }
    });
    let mut terminal = ratatui::init();
    let _guard = TerminalGuard;
    let mut app = App::default();
    let result = loop {
        while let Ok(update) = rx.try_recv() {
            match update {
                Update::Snapshot(snapshot) => {
                    app.error = None;
                    app.snapshot = Some(snapshot);
                    app.last_seen = Some(Instant::now());
                }
                Update::Error(error) => app.error = Some(error),
                Update::Notice(notice) => app.notice = Some(notice),
            }
        }
        if app.notice.as_ref().is_some_and(|notice| !notice.visible()) {
            app.notice = None;
        }
        if let Err(error) = terminal.draw(|frame| draw(frame, &mut app)) {
            break Err(format!("terminal draw: {error}"));
        }
        match event::poll(Duration::from_millis(100)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                    if let Some(confirm) = app.confirm.take() {
                        match key.code {
                            KeyCode::Char('y') | KeyCode::Char('Y') => match admin.as_ref() {
                                Ok(admin) => app.run_toggle(&confirm, admin, &action_tx),
                                Err(error) => app.notice = Some(Notice::error(error.clone())),
                            },
                            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                                app.notice = Some(Notice::ok(format!(
                                    "kept {} {}",
                                    confirm.label,
                                    if confirm.enable {
                                        "disabled"
                                    } else {
                                        "enabled"
                                    }
                                )));
                            }
                            _ => app.confirm = Some(confirm),
                        }
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break Ok(());
                        }
                        KeyCode::Down | KeyCode::Char('j') => app.down(),
                        KeyCode::Up | KeyCode::Char('k') => app.up(),
                        KeyCode::PageDown => app.page(10),
                        KeyCode::PageUp => app.page(-10),
                        KeyCode::Home => app.selected = 0,
                        KeyCode::End => app.selected = app.account_count().saturating_sub(1),
                        KeyCode::Char('d') | KeyCode::Char('e') => {
                            if let Err(error) = app.ask_toggle(key.code == KeyCode::Char('e')) {
                                app.notice = Some(Notice::error(error));
                            }
                        }
                        _ => {}
                    }
                }
                Ok(_) => {}
                Err(error) => break Err(format!("terminal input: {error}")),
            },
            Ok(false) => {}
            Err(error) => break Err(format!("terminal poll: {error}")),
        }
    };
    drop(terminal);
    drop(rx);
    let _ = worker.join();
    result
}

fn local_token(config: &Config) -> Option<String> {
    let conn = rusqlite::Connection::open_with_flags(
        config.db_path(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    conn.query_row(
        "SELECT value FROM config WHERE key = 'ui_token'",
        [],
        |row| row.get(0),
    )
    .ok()
}

#[derive(Default)]
struct App {
    snapshot: Option<MonitorSnapshot>,
    error: Option<String>,
    last_seen: Option<Instant>,
    selected: usize,
    notice: Option<Notice>,
    confirm: Option<Confirm>,
}

impl App {
    fn account_count(&self) -> usize {
        self.snapshot.as_ref().map_or(0, |s| s.accounts.len())
    }
    fn down(&mut self) {
        self.selected = (self.selected + 1).min(self.account_count().saturating_sub(1));
    }
    fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }
    fn page(&mut self, delta: isize) {
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(self.account_count().saturating_sub(1));
    }
    fn selected_account(&self) -> Option<&MonitorAccount> {
        self.snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.accounts.get(self.selected))
    }
    /// Queue the enable/disable prompt for the selected account. `e` on an enabled account and `d`
    /// on a disabled one are no-ops with a notice: re-enabling an enabled account would clear a
    /// quota deadline the operator never asked to clear, so the action is only offered when it
    /// changes the state the operator can see.
    fn ask_toggle(&mut self, enable: bool) -> Result<(), String> {
        let Some(account) = self.selected_account() else {
            return Err("no account selected".into());
        };
        let disabled = matches!(
            account.status,
            AccountStatus::Disabled | AccountStatus::AuthError
        );
        if enable != disabled {
            return Err(format!(
                "{} is already {}",
                account.label,
                if enable { "enabled" } else { "disabled" }
            ));
        }
        self.confirm = Some(Confirm {
            id: account.id.clone(),
            label: account.label.clone(),
            enable,
        });
        Ok(())
    }
    /// Perform a confirmed toggle. The admin call runs on its own thread so a slow or wedged server
    /// cannot freeze the dashboard; the result arrives as an [`Update::Notice`].
    fn run_toggle(&mut self, confirm: &Confirm, admin: &Admin, tx: &mpsc::Sender<Update>) {
        let admin = admin.clone();
        let tx = tx.clone();
        let id = confirm.id.clone();
        let enable = confirm.enable;
        let label = confirm.label.clone();
        let verb = confirm.verb();
        self.notice = Some(Notice::ok(format!("{verb} {label}…")));
        std::thread::spawn(move || {
            let notice = match admin.set_enabled(&id, enable) {
                Ok(()) => Notice::ok(format!(
                    "{verb} {label}: {}",
                    if enable { "enabled" } else { "disabled" }
                )),
                Err(error) => Notice::error(format!("{verb} {label} failed: {error}")),
            };
            let _ = tx.send(Update::Notice(notice));
        });
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let area = frame.area();
    frame.render_widget(
        Block::default().style(Style::default().bg(BACKGROUND)),
        area,
    );
    let compact = area.width < 100;
    let short = area.height < 25;
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(4),
            Constraint::Length(if short { 4 } else { 5 }),
            Constraint::Min(5),
            Constraint::Length(if short { 0 } else { 7 }),
            Constraint::Length(1),
        ])
        .split(area);
    let status = if let Some(error) = &app.error {
        format!("DISCONNECTED · {error}")
    } else if let Some(seen) = app.last_seen {
        format!("LIVE · updated {}s ago", seen.elapsed().as_secs())
    } else {
        "CONNECTING".to_string()
    };
    let header = Paragraph::new(Line::from(vec![
        Span::styled(
            " UNDERCLASS ",
            Style::default()
                .fg(BACKGROUND)
                .bg(CYAN)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  pool monitor  ·  ", Style::default().fg(MUTED)),
        Span::styled(
            status,
            Style::default()
                .fg(if app.error.is_some() { RED } else { GREEN })
                .add_modifier(Modifier::BOLD),
        ),
    ]))
    .block(panel().title(" underclass top "));
    frame.render_widget(header, sections[0]);
    if let Some(snapshot) = &app.snapshot {
        let inflight: u32 = snapshot.accounts.iter().map(|a| a.inflight).sum();
        // A partially cooled account is not fully available, so it must not inflate HEALTHY.
        let healthy = snapshot
            .accounts
            .iter()
            .filter(|a| {
                a.status == AccountStatus::Healthy
                    && a.cooling_scopes
                        .values()
                        .all(|until| *until <= snapshot.now_ms)
            })
            .count();
        let summary = vec![
            Line::from(vec![
                Span::styled(" LIVE ", Style::default().fg(MUTED)),
                Span::styled(
                    format!("{inflight:>3}"),
                    Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "   HEALTHY {healthy}/{}   LAST 60s ",
                        snapshot.accounts.len()
                    ),
                    Style::default().fg(MUTED),
                ),
                Span::styled(
                    snapshot.last_minute.attempts.to_string(),
                    Style::default().fg(PURPLE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        " attempts · {} in / {} out",
                        number(snapshot.last_minute.input_tokens),
                        number(snapshot.last_minute.output_tokens)
                    ),
                    Style::default().fg(TEXT),
                ),
            ]),
            Line::from(vec![
                Span::styled(" MONTH ", Style::default().fg(MUTED)),
                Span::styled(
                    format!("{:>6}", snapshot.month.attempts),
                    Style::default().fg(PURPLE).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        " attempts · {} in / {} out · ",
                        number(snapshot.month.input_tokens),
                        number(snapshot.month.output_tokens)
                    ),
                    Style::default().fg(TEXT),
                ),
                Span::styled(
                    format!("{} unknown", snapshot.month.unknown),
                    Style::default().fg(if snapshot.month.unknown > 0 {
                        YELLOW
                    } else {
                        MUTED
                    }),
                ),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(summary)
                .block(panel().title(" inference "))
                .wrap(Wrap { trim: true }),
            sections[1],
        );
        let data: Vec<u64> = snapshot
            .minute_bins
            .iter()
            .map(|&n| n.max(0) as u64)
            .collect();
        frame.render_widget(
            Sparkline::default()
                .block(panel().title(" attempts / minute · last 60 minutes "))
                .data(&data)
                .style(Style::default().fg(PURPLE).bg(PANEL)),
            sections[2],
        );
        if !short {
            draw_recent(frame, sections[4], snapshot);
        }
        draw_accounts(frame, sections[3], app, compact);
    } else {
        frame.render_widget(
            Paragraph::new("Waiting for the server…").block(panel().title(" accounts ")),
            sections[3],
        );
    }
    let notice = app.notice.as_ref().filter(|notice| notice.visible());
    let help = if let Some(notice) = notice {
        Line::from(Span::styled(
            format!(" {} ", notice.text),
            Style::default()
                .fg(if notice.ok { GREEN } else { RED })
                .bg(BACKGROUND)
                .add_modifier(Modifier::BOLD),
        ))
    } else if compact {
        Line::from(Span::styled(
            " q quit  ↑/↓ scroll  d/e disable/enable  ·  counts: upstream attempts",
            Style::default().fg(MUTED).bg(BACKGROUND),
        ))
    } else {
        Line::from(Span::styled(
            " q quit  ↑/↓ or j/k scroll  PgUp/PgDn page  d disable  e enable  ·  upstream attempts; unknown tokens excluded from totals",
            Style::default().fg(MUTED).bg(BACKGROUND),
        ))
    };
    frame.render_widget(Paragraph::new(help), sections[5]);
    if let Some(confirm) = &app.confirm {
        draw_confirm(frame, area, confirm);
    }
}

/// The confirmation prompt. It names the account and the action, so a mistyped `y` cannot disable a
/// different account than the one the operator was looking at, and it is the only path to a write.
fn draw_confirm(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, confirm: &Confirm) {
    let lines = vec![
        Line::from(Span::styled(
            format!("{} {}?", confirm.verb(), display_text(&confirm.label)),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "y confirm · n/Esc cancel",
            Style::default().fg(MUTED),
        )),
    ];
    // Three lines plus two borders; the width is bounded by the panel so a long label wraps rather
    // than pushing the popup past the terminal edge.
    let width = area.width.saturating_sub(4).clamp(24, 52).min(area.width);
    let height = 5u16.min(area.height);
    let popup = ratatui::layout::Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(ratatui::widgets::Clear, popup);
    let block = panel()
        .title(" confirm ")
        .padding(ratatui::widgets::Padding::horizontal(1))
        .border_style(Style::default().fg(if confirm.enable { GREEN } else { YELLOW }));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: true })
            .style(Style::default().fg(TEXT).bg(PANEL)),
        popup,
    );
}

/// Column widths are measured from the cells that will actually be drawn rather than fixed
/// percentages: a percentage gives `ACCOUNT` and `QUOTA LEFT` the same share whether they hold four
/// characters or forty, which squeezed `STATE` and clipped the cooling scope and deadline.
///
/// Each column is first sized to its content (`ACCOUNT`, `PLAN`, `BACKEND`, and `QUOTA LEFT`
/// shrink to what they actually hold, so an empty `PLAN` costs two cells, not twelve). `BACKEND` is
/// measured from the backends being shown, floored at its header width and capped at
/// [`MAX_BACKEND_WIDTH`]. The leftover width is then shared out by [`WEIGHT`], so a wider terminal
/// widens every column instead of leaving dead space on the right - `Table` defaults to
/// `Flex::Start`, which would pad the right edge only.
fn account_columns(
    snapshot: &MonitorSnapshot,
    compact: bool,
) -> (Vec<&'static str>, Vec<u16>, &'static [u16]) {
    let widest = |values: Vec<String>, header: &str, cap: usize| -> u16 {
        values
            .into_iter()
            .map(|value| display_width(&value))
            .max()
            .unwrap_or(0)
            .max(display_width(header))
            .min(cap) as u16
    };

    let label_width = widest(
        snapshot
            .accounts
            .iter()
            .map(|a| display_text(&a.label))
            .collect(),
        "ACCOUNT",
        MAX_LABEL_WIDTH,
    );
    let plan_width = widest(
        snapshot.accounts.iter().map(plan_text).collect(),
        "PLAN",
        MAX_PLAN_WIDTH,
    );
    let backend_width = widest(
        snapshot
            .accounts
            .iter()
            .map(|a| a.backend.as_str().to_string())
            .collect(),
        "BACKEND",
        MAX_BACKEND_WIDTH as usize,
    );
    let status_width = widest(
        snapshot
            .accounts
            .iter()
            .map(|a| status_text(a, snapshot.now_ms))
            .collect(),
        "STATE",
        MAX_STATUS_WIDTH,
    );
    let quota_width = widest(
        snapshot
            .accounts
            .iter()
            .map(|a| quota(a, snapshot.now_ms))
            .collect(),
        "QUOTA LEFT",
        MAX_QUOTA_WIDTH,
    );

    if compact {
        return (
            vec!["ACCOUNT", "PLAN", "STATE", "LIVE", "SESS", "QUOTA LEFT"],
            vec![
                label_width,
                plan_width,
                status_width,
                display_width("LIVE") as u16,
                display_width("SESS") as u16,
                quota_width.max(10),
            ],
            // A narrower table cannot spare growth; give it all to `STATE`.
            &COMPACT_WEIGHT,
        );
    }
    (
        vec![
            "ACCOUNT",
            "PLAN",
            "BACKEND",
            "STATE",
            "LIVE",
            "SESS",
            "QUOTA LEFT",
            "RESETS",
            "MONTH ATT · IN/OUT",
        ],
        vec![
            label_width,
            plan_width,
            backend_width,
            status_width,
            display_width("LIVE") as u16,
            display_width("SESS") as u16,
            quota_width,
            display_width("RESETS") as u16,
            display_width("MONTH ATT · IN/OUT") as u16,
        ],
        &WEIGHT,
    )
}

/// Relative share of leftover width per column, in the wide and compact layouts. The fixed-count
/// columns (`LIVE`, `SESS`) are passed separately so their width is not scaled.
/// `"\u{25b6} "` is two cells and ratatui reserves it when a row is selected.
const HIGHLIGHT_WIDTH: u16 = 2;

/// A column never shrinks below this when the panel is too narrow for the full layout.
const MIN_COLUMN_WIDTH: u16 = 4;

const WEIGHT: [u16; 9] = [3, 2, 2, 5, 0, 0, 3, 2, 6];
const COMPACT_WEIGHT: [u16; 6] = [3, 2, 6, 0, 0, 3];

/// Grow `widths` by the leftover space, proportionally to `weights`, and return the final widths.
/// Integer division leaves at most `len - 1` cells unassigned; they go to the widest columns so the
/// table still fills the panel exactly.
fn grow_to_fit(widths: Vec<u16>, weights: &[u16], area_width: u16) -> Vec<Constraint> {
    // Ratatui also reserves one cell between columns and the highlight-symbol column, which the
    // caller's `area_width` still includes.
    let spacing = (widths.len() as u16).saturating_sub(1) + HIGHLIGHT_WIDTH;
    let used = widths.iter().sum::<u16>() + spacing;
    if used > area_width {
        // The content is wider than the panel: take the shortfall from the weighted columns,
        // largest weight first, instead of letting the table overflow off the right edge.
        let mut widths = widths;
        let mut deficit = used - area_width;
        // Shrink the narrowest columns first so the widest value keeps its room; `STATE` and
        // `MONTH ATT` are last to give up cells because truncating them loses information.
        let mut order: Vec<usize> = (0..weights.len()).collect();
        order.sort_by_key(|i| (weights[*i], std::cmp::Reverse(widths[*i])));
        for i in order {
            if deficit == 0 {
                break;
            }
            let take = widths[i].saturating_sub(MIN_COLUMN_WIDTH).min(deficit);
            widths[i] -= take;
            deficit -= take;
        }
        // A pathologically narrow panel can defeat each column's floor. Keep taking cells in the
        // same order until the widths fit, otherwise the table overflows the right edge, which is
        // the exact failure this branch exists to prevent.
        while deficit > 0 {
            let Some((i, _)) = widths
                .iter()
                .enumerate()
                .filter(|(_, w)| **w > 1)
                // Narrowest first, and among equals the lightest column: `STATE` and `MONTH ATT`
                // carry the most weight and must be the last to lose a cell.
                .min_by_key(|(i, w)| (*w, weights[*i]))
            else {
                break;
            };
            widths[i] -= 1;
            deficit -= 1;
        }
        return widths.into_iter().map(Constraint::Length).collect();
    }
    let available = area_width.saturating_sub(used);
    if available == 0 || weights.iter().all(|w| *w == 0) {
        return widths.into_iter().map(Constraint::Length).collect();
    }
    let total: u32 = weights.iter().map(|w| u32::from(*w)).sum();
    let mut final_widths = widths.clone();
    for (width, weight) in final_widths.iter_mut().zip(weights) {
        let extra = u32::from(available) * u32::from(*weight) / total;
        *width = width.saturating_add(extra as u16);
    }
    // Hand the rounding remainder to the columns with the largest weight, keeping the total exact.
    let mut leftover = available
        - (final_widths
            .iter()
            .zip(&widths)
            .map(|(new, old)| new - old)
            .sum::<u16>());
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by_key(|i| std::cmp::Reverse(weights[*i]));
    for i in order {
        if leftover == 0 {
            break;
        }
        final_widths[i] = final_widths[i].saturating_add(1);
        leftover -= 1;
    }
    final_widths.into_iter().map(Constraint::Length).collect()
}

/// Terminal cells a string occupies. `chars().count()` undercounts double-width glyphs (CJK,
/// emoji), which would size a column too small and clip the value it was measured from.
fn display_width(value: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(value)
}

fn plan_text(account: &MonitorAccount) -> String {
    display_text(
        account
            .plan
            .as_deref()
            .unwrap_or(if account.backend == BackendId::Codex {
                "unknown"
            } else {
                "\u{2014}"
            }),
    )
}

fn status_text(account: &MonitorAccount, now: i64) -> String {
    match account.status {
        AccountStatus::Healthy => {
            scoped_status(account, now).unwrap_or_else(|| "healthy".to_string())
        }
        AccountStatus::Cooling => format!("cooling {}", time_until(account.reset_at, now)),
        AccountStatus::AuthError => "auth error".to_string(),
        AccountStatus::Disabled => "disabled".to_string(),
    }
}

/// Caps that stop one unusually long value from squeezing every other column. `STATE` allows for
/// `cooling <scope> <deadline> +N`, the longest string this table renders. `MAX_BACKEND_WIDTH` is
/// derived from the widest [`BackendId`] instead of a literal, so adding or renaming a backend
/// cannot silently clip the column again.
const MAX_LABEL_WIDTH: usize = 20;
const MAX_PLAN_WIDTH: usize = 12;
const MAX_STATUS_WIDTH: usize = 34;
const MAX_QUOTA_WIDTH: usize = 30;
const MAX_BACKEND_WIDTH: u16 = BackendId::max_str_width() as u16;

fn draw_accounts(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    app: &mut App,
    compact: bool,
) {
    let snapshot = app.snapshot.as_ref().unwrap();
    app.selected = app.selected.min(snapshot.accounts.len().saturating_sub(1));
    let rows: Vec<Row<'_>> = snapshot
        .accounts
        .iter()
        .enumerate()
        .map(|(index, account)| {
            let status = status_text(account, snapshot.now_ms);
            let cells = if compact {
                vec![
                    Cell::from(display_text(&account.label)),
                    Cell::from(plan_text(account)),
                    Cell::from(status)
                        .style(Style::default().fg(row_status_color(account, snapshot.now_ms))),
                    Cell::from(account.inflight.to_string()).style(Style::default().fg(CYAN)),
                    Cell::from(account.sticky_sessions.to_string())
                        .style(Style::default().fg(PURPLE)),
                    Cell::from(quota(account, snapshot.now_ms))
                        .style(Style::default().fg(quota_color(account, snapshot.now_ms))),
                ]
            } else {
                vec![
                    Cell::from(display_text(&account.label)),
                    Cell::from(plan_text(account)),
                    Cell::from(account.backend.as_str()),
                    Cell::from(status)
                        .style(Style::default().fg(row_status_color(account, snapshot.now_ms))),
                    Cell::from(account.inflight.to_string()).style(Style::default().fg(CYAN)),
                    Cell::from(account.sticky_sessions.to_string())
                        .style(Style::default().fg(PURPLE)),
                    Cell::from(quota(account, snapshot.now_ms))
                        .style(Style::default().fg(quota_color(account, snapshot.now_ms))),
                    Cell::from(credits(account)).style(
                        Style::default().fg(
                            if account
                                .quota
                                .as_ref()
                                .is_some_and(|q| q.available_resets > 0)
                            {
                                CYAN
                            } else {
                                MUTED
                            },
                        ),
                    ),
                    Cell::from(format!(
                        "{} \u{00b7} {} / {}",
                        account.month.attempts,
                        number(account.month.input_tokens),
                        number(account.month.output_tokens)
                    ))
                    .style(Style::default().fg(MUTED)),
                ]
            };
            Row::new(cells).style(Style::default().fg(TEXT).bg(if index % 2 == 0 {
                PANEL
            } else {
                Color::Rgb(25, 34, 49)
            }))
        })
        .collect();
    let (headers, widths, weights) = account_columns(snapshot, compact);
    // The table sits inside `panel()`, whose borders take a cell on each side. Sizing against the
    // outer rect would over-subscribe the inner one by two and clip the last column.
    let widths = grow_to_fit(widths, weights, panel().inner(area).width);
    let table = Table::new(rows, widths)
        .header(Row::new(headers).style(Style::default().fg(CYAN).add_modifier(Modifier::BOLD)))
        .block(panel().title(format!(" accounts \u{00b7} {} ", snapshot.accounts.len())))
        .row_highlight_style(
            Style::default()
                .bg(Color::Rgb(48, 61, 84))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("\u{25b6} ");
    let mut state = TableState::default();
    if !snapshot.accounts.is_empty() {
        state.select(Some(app.selected));
    }
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(2)])
        .split(area);
    frame.render_stateful_widget(table, parts[0], &mut state);
    let detail = snapshot.accounts.get(app.selected).map_or_else(
        || "No accounts configured".to_string(),
        |a| account_detail(a, snapshot.now_ms),
    );
    frame.render_widget(
        Paragraph::new(detail)
            .style(Style::default().fg(MUTED).bg(PANEL))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(BORDER)),
            )
            .wrap(Wrap { trim: true }),
        parts[1],
    );
}

fn account_detail(account: &MonitorAccount, now_ms: i64) -> String {
    let mut parts = vec![format!(
        "{}: {} month attempts · {} unknown",
        display_text(&account.label),
        account.month.attempts,
        account.month.unknown
    )];
    if let Some(snapshot) = &account.quota {
        parts.push(format!(
            "quota checked {} ago",
            time_until(now_ms, snapshot.fetched_at_ms)
        ));
        if let Some(limit) = &snapshot.rate_limit {
            for (name, window) in [
                ("primary", &limit.primary_window),
                ("secondary", &limit.secondary_window),
            ] {
                if let Some(window) = window {
                    let reset = window
                        .reset_at
                        .and_then(|value| value.checked_mul(1000))
                        .map_or("reset unknown".to_string(), |time| {
                            format!("renews in {}", time_until(time, now_ms))
                        });
                    let left = (100.0 - window.used_percent).clamp(0.0, 100.0);
                    parts.push(format!(
                        "{name} {:.0}% used / {left:.0}% left · {reset}",
                        window.used_percent
                    ));
                }
            }
        }
    }
    parts.join("  ·  ")
}

/// Recent-outcomes widths. This table has no header row, so every column is measured from the
/// values it will draw: `MODEL` is the widest in practice (`cline/cline-pass/deepseek-v4.1-flash`
/// is 41 cells) and takes the largest share, while the fixed-shape columns (`HH:MM:SS`, the status
/// code, `<n> ms`) are sized so they never truncate. `BACKEND` is measured from the backend ids in
/// the rows and capped at [`MAX_BACKEND_WIDTH`], the same cap the accounts table uses. The total is
/// clamped to the panel's inner width, so a narrow pane shrinks `MODEL`/`ACCOUNT` instead of
/// spilling past the border.
///
/// @cc [owner:ghuntley,label:cli] recent-columns-measure-backend
/// `BACKEND` MUST be measured from the backend values in `snapshot.recent` and capped at
/// `MAX_BACKEND_WIDTH`, never from a literal width, and the returned widths plus the five
/// inter-column gaps MUST NOT exceed `area_width - 2`.
fn recent_columns(area_width: u16, snapshot: &MonitorSnapshot) -> Vec<Constraint> {
    let inner = area_width.saturating_sub(2); // the panel's borders
    let spacing = 5u16; // five gaps between six columns
    let available = inner.saturating_sub(spacing);

    let widest = |values: Vec<String>, fallback: u16, cap: u16| -> u16 {
        values
            .into_iter()
            .map(|value| display_width(&value))
            .max()
            .unwrap_or(0)
            .max(usize::from(fallback))
            .min(usize::from(cap)) as u16
    };
    let time = widest(
        snapshot
            .recent
            .iter()
            .map(|_| "00:00:00".to_string())
            .collect(),
        8,
        8,
    );
    let status = widest(
        snapshot
            .recent
            .iter()
            .map(|r| r.status.to_string())
            .collect(),
        3,
        3,
    );
    let duration = widest(
        snapshot
            .recent
            .iter()
            .map(|r| format!("{} ms", r.duration_ms))
            .collect(),
        7,
        10,
    );
    let backend = widest(
        snapshot
            .recent
            .iter()
            .filter_map(|r| r.backend.clone())
            .collect(),
        display_width("—") as u16,
        MAX_BACKEND_WIDTH,
    );

    let rest = available.saturating_sub(time + status + duration + backend);
    // MODEL gets the lion's share; ACCOUNT takes what is left.
    let model = (rest * 7 / 10).max(1);
    let account = rest.saturating_sub(model).max(1);
    let mut widths = vec![time, model, backend, account, status, duration];

    // A narrow panel can still exceed the inner width; take it out of the widest columns first so
    // the fixed-shape ones keep their exact value. `BACKEND` is measured, not guessed, so it is
    // only clipped once `MODEL` and `ACCOUNT` have already given up their cells.
    let mut total: u16 = widths.iter().sum();
    while total > available {
        let Some((i, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > 1)
            .max_by_key(|(_, w)| **w)
        else {
            break;
        };
        widths[i] -= 1;
        total -= 1;
    }
    widths.into_iter().map(Constraint::Length).collect()
}

fn draw_recent(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    snapshot: &MonitorSnapshot,
) {
    let rows: Vec<Row<'_>> = snapshot
        .recent
        .iter()
        .take(5)
        .enumerate()
        .map(|(index, request)| {
            let status_color = if request.status >= 500 {
                RED
            } else if request.status >= 400 {
                YELLOW
            } else {
                GREEN
            };
            Row::new(vec![
                Cell::from(
                    chrono::DateTime::from_timestamp_millis(request.ts)
                        .map_or("?".into(), |t| t.format("%H:%M:%S").to_string()),
                )
                .style(Style::default().fg(MUTED)),
                Cell::from(display_text(&request.model)),
                Cell::from(request.backend.clone().unwrap_or_else(|| "—".into()))
                    .style(Style::default().fg(PURPLE)),
                Cell::from(
                    request
                        .label
                        .as_deref()
                        .map_or_else(|| "—".into(), display_text),
                ),
                Cell::from(request.status.to_string()).style(
                    Style::default()
                        .fg(status_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Cell::from(format!("{} ms", request.duration_ms)).style(Style::default().fg(MUTED)),
            ])
            .style(Style::default().fg(TEXT).bg(if index % 2 == 0 {
                PANEL
            } else {
                Color::Rgb(25, 34, 49)
            }))
        })
        .collect();
    let table = Table::new(rows, recent_columns(area.width, snapshot))
        .block(panel().title(" recent outcomes "));
    frame.render_widget(table, area);
}

fn quota(account: &MonitorAccount, now_ms: i64) -> String {
    if account.backend == BackendId::Claude {
        return claude_quota(account);
    }
    if account.backend != BackendId::Codex {
        return "unknown".into();
    }
    let Some(snapshot) = &account.quota else {
        return "waiting…".into();
    };
    let Some(limit) = &snapshot.rate_limit else {
        return "unavailable".into();
    };
    let mut windows = Vec::new();
    for window in [&limit.primary_window, &limit.secondary_window]
        .into_iter()
        .flatten()
    {
        let length = window.limit_window_seconds.map_or("?".into(), |s| {
            if s >= 86_400 {
                format!("{}d", s / 86_400)
            } else {
                format!("{}h", s / 3_600)
            }
        });
        let left = (100.0 - window.used_percent).clamp(0.0, 100.0);
        windows.push(format!("{length} {left:.0}%"));
    }
    if windows.is_empty() {
        "unavailable".into()
    } else {
        let mut result = windows.join(" · ");
        if now_ms.saturating_sub(snapshot.fetched_at_ms) > 180_000 {
            result.push_str(" · stale");
        }
        result
    }
}

/// The Claude Code subscription windows, as the account's own usage API last reported them.
fn claude_quota(account: &MonitorAccount) -> String {
    let Some(observation) = &account.observation else {
        return "waiting…".into();
    };
    let window = |name: &str| {
        observation
            .get(name)
            .and_then(|w| w.get("utilization"))
            .and_then(|u| u.as_f64())
            .map(|utilization| (100.0 - utilization * 100.0).clamp(0.0, 100.0))
    };
    let mut windows = Vec::new();
    if let Some(left) = window("five_hour") {
        windows.push(format!("5h {left:.0}%"));
    }
    if let Some(left) = window("seven_day") {
        windows.push(format!("7d {left:.0}%"));
    }
    if windows.is_empty() {
        return "unavailable".into();
    }
    windows.join(" · ")
}

fn credits(account: &MonitorAccount) -> String {
    if account.backend == BackendId::Claude {
        return account
            .observation
            .as_ref()
            .and_then(|o| o.get("reset_credits"))
            .and_then(|c| c.as_i64())
            .map_or("—".into(), |count| count.to_string());
    }
    account
        .quota
        .as_ref()
        .map_or("—".into(), |q| q.available_resets.to_string())
}

/// The Claude Code windows use the same severity bands as the Codex quota.
fn claude_quota_color(account: &MonitorAccount, now_ms: i64) -> Color {
    let Some(observation) = &account.observation else {
        return MUTED;
    };
    // A stale observation must not read as a live quota: the poll is what refreshes it, and a
    // percentage from days ago still looks green or red with full confidence.
    let observed_at = observation
        .get("observed_at_ms")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if now_ms.saturating_sub(observed_at) > 180_000 {
        return MUTED;
    }
    let windows: Vec<f64> = ["five_hour", "seven_day"]
        .into_iter()
        .filter_map(|name| {
            observation
                .get(name)
                .and_then(|w| w.get("utilization"))
                .and_then(|u| u.as_f64())
                .map(|utilization| (100.0 - utilization * 100.0).clamp(0.0, 100.0))
        })
        .collect();
    // Nothing to grade: an observation carrying only the plan or reset credits must read as
    // unknown, not as 100% remaining.
    let Some(remaining) = windows.into_iter().reduce(f64::min) else {
        return MUTED;
    };
    if remaining <= 10.0 {
        RED
    } else if remaining <= 30.0 {
        YELLOW
    } else {
        GREEN
    }
}

/// @cc [owner:ghuntley,label:cli] quota-color-severity
/// Fresh Codex quota MUST use the least remaining window: green above 30%, amber above 10% through
/// 30%, red at 10% or less or when exhausted. Missing or stale quota MUST use a muted color.
fn quota_color(account: &MonitorAccount, now_ms: i64) -> Color {
    if account.backend == BackendId::Claude {
        return claude_quota_color(account, now_ms);
    }
    if account.backend != BackendId::Codex {
        return MUTED;
    }
    let Some(snapshot) = &account.quota else {
        return MUTED;
    };
    if now_ms.saturating_sub(snapshot.fetched_at_ms) > 180_000 {
        return MUTED;
    }
    let Some(limit) = &snapshot.rate_limit else {
        return MUTED;
    };
    let remaining = [
        limit.primary_window.as_ref(),
        limit.secondary_window.as_ref(),
    ]
    .into_iter()
    .flatten()
    .map(|window| 100.0 - window.used_percent)
    .reduce(f64::min);
    if limit.allowed == Some(false) || limit.limit_reached == Some(true) {
        return RED;
    }
    match remaining {
        Some(value) if value <= 10.0 => RED,
        Some(value) if value <= 30.0 => YELLOW,
        Some(_) => GREEN,
        None => MUTED,
    }
}

fn row_status_color(account: &MonitorAccount, now: i64) -> Color {
    match account.status {
        AccountStatus::Healthy if scoped_status(account, now).is_some() => YELLOW,
        status => status_color(status),
    }
}

fn status_color(status: AccountStatus) -> Color {
    match status {
        AccountStatus::Healthy => GREEN,
        AccountStatus::Cooling => YELLOW,
        AccountStatus::AuthError => RED,
        AccountStatus::Disabled => MUTED,
    }
}

/// A healthy account can still have surfaces cooling; showing `healthy` alone would repeat the
/// bug this exists to fix. The earliest live deadline leads and the rest are counted.
fn scoped_status(account: &MonitorAccount, now: i64) -> Option<String> {
    let mut live: Vec<(&String, i64)> = account
        .cooling_scopes
        .iter()
        .filter(|(_, until)| **until > now)
        .map(|(scope, until)| (scope, *until))
        .collect();
    live.sort_by_key(|(_, until)| *until);
    let (first_scope, first_until) = live.first()?;
    let extra = live.len() - 1;
    let suffix = if extra > 0 {
        format!(" +{extra}")
    } else {
        String::new()
    };
    Some(format!(
        "cooling {first_scope} {}{suffix}",
        time_until(*first_until, now)
    ))
}

fn time_until(deadline: i64, now: i64) -> String {
    let seconds = (deadline.saturating_sub(now) / 1000).max(0);
    if seconds >= 3600 {
        format!("{}h{}m", seconds / 3600, seconds % 3600 / 60)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}s", seconds)
    }
}

fn number(value: i64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.1}k", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

fn display_text(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::{Counters, MonitorSnapshot};
    use crate::resets::{RateLimit, UsageSnapshot, Window};
    use ratatui::backend::TestBackend;

    #[test]
    fn claude_quota_column_reports_the_subscription_windows() {
        let account = MonitorAccount {
            id: "c".into(),
            label: "claude@example.test".into(),
            backend: BackendId::Claude,
            plan: Some("max".into()),
            status: AccountStatus::Healthy,
            reset_at: 0,
            cooling_scopes: Default::default(),
            inflight: 0,
            sticky_sessions: 0,
            month: Counters::default(),
            observation: Some(serde_json::json!({
                "plan": "max",
                "five_hour": {"utilization": 0.07},
                "seven_day": {"utilization": 0.42},
                "reset_credits": 2,
            })),
            quota: None,
        };
        assert_eq!(quota(&account, 1_000), "5h 93% · 7d 58%");
        assert_eq!(credits(&account), "2");
        assert_eq!(quota_color(&account, 1_000), GREEN);

        let exhausted = MonitorAccount {
            observation: Some(serde_json::json!({
                "five_hour": {"utilization": 1.0},
                "seven_day": {"utilization": 0.1},
            })),
            ..account
        };
        assert_eq!(quota_color(&exhausted, 1_000), RED);
    }

    #[test]
    fn quota_palette_tracks_lowest_window_and_staleness() {
        let mut account = MonitorAccount {
            id: "a".into(),
            label: "alice".into(),
            backend: BackendId::Codex,
            plan: Some("plus".into()),
            status: AccountStatus::Healthy,
            reset_at: 0,
            cooling_scopes: Default::default(),
            inflight: 0,
            sticky_sessions: 0,
            month: Counters::default(),
            observation: None,
            quota: Some(UsageSnapshot {
                rate_limit: Some(RateLimit {
                    allowed: Some(true),
                    limit_reached: Some(false),
                    primary_window: Some(Window {
                        used_percent: 20.0,
                        reset_at: None,
                        limit_window_seconds: None,
                    }),
                    secondary_window: Some(Window {
                        used_percent: 75.0,
                        reset_at: None,
                        limit_window_seconds: None,
                    }),
                }),
                available_resets: 0,
                credit_expirations: vec![],
                fetched_at_ms: 1_000,
            }),
        };
        assert_eq!(quota_color(&account, 1_000), YELLOW);
        account
            .quota
            .as_mut()
            .unwrap()
            .rate_limit
            .as_mut()
            .unwrap()
            .secondary_window
            .as_mut()
            .unwrap()
            .used_percent = 95.0;
        assert_eq!(quota_color(&account, 1_000), RED);
        assert_eq!(quota_color(&account, 181_001), MUTED);
        account.backend = BackendId::Copilot;
        assert_eq!(quota_color(&account, 1_000), MUTED);
    }

    #[test]
    fn partially_cooled_accounts_are_not_reported_healthy() {
        let mut account = MonitorAccount {
            id: "a".into(),
            label: "alice".into(),
            backend: BackendId::Cline,
            plan: None,
            status: AccountStatus::Healthy,
            reset_at: 0,
            cooling_scopes: Default::default(),
            inflight: 0,
            sticky_sessions: 0,
            month: Counters::default(),
            quota: None,
            observation: None,
        };
        assert_eq!(scoped_status(&account, 1_000), None, "nothing cooling yet");

        account.cooling_scopes.insert("credits".into(), 61_000);
        account.cooling_scopes.insert("cline-pass".into(), 121_000);
        account.cooling_scopes.insert("expired".into(), 500);
        assert_eq!(
            scoped_status(&account, 1_000).as_deref(),
            Some("cooling credits 1m +1"),
            "the earliest live deadline leads, expired surfaces are ignored, and the rest count"
        );
        assert_eq!(row_status_color(&account, 1_000), YELLOW);

        account.cooling_scopes.clear();
        assert_eq!(row_status_color(&account, 1_000), GREEN);
    }

    #[test]
    fn dashboard_renders_at_standard_and_small_sizes() {
        for (width, height) in [(120, 30), (80, 24), (40, 12)] {
            let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut app = App {
                snapshot: Some(MonitorSnapshot {
                    now_ms: 120_000,
                    month_start_ms: 0,
                    accounts: vec![MonitorAccount {
                        id: "account".into(),
                        label: "test account".into(),
                        backend: BackendId::Codex,
                        plan: Some("plus".into()),
                        status: AccountStatus::Healthy,
                        reset_at: 0,
                        cooling_scopes: Default::default(),
                        inflight: 0,
                        sticky_sessions: 0,
                        month: Counters::default(),
                        observation: None,
                        quota: None,
                    }],
                    month: Counters::default(),
                    last_minute: Counters::default(),
                    minute_bins: vec![0; 60],
                    recent: vec![],
                }),
                ..Default::default()
            };
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
            if width >= 80 {
                let screen = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(screen.contains("PLAN"));
                assert!(screen.contains("plus"));
            }
        }
    }

    #[test]
    fn control_characters_are_removed_from_display() {
        assert_eq!(display_text("alice\x1b[31m\n"), "alice [31m ");
    }
}

#[cfg(test)]
mod width_tests {
    use super::*;
    use crate::monitor::{Counters, MonitorAccount, MonitorSnapshot};
    use ratatui::backend::TestBackend;

    fn snap(scopes: &[(&str, i64)]) -> MonitorSnapshot {
        MonitorSnapshot {
            now_ms: 1_000_000,
            month_start_ms: 0,
            accounts: vec![
                MonitorAccount {
                    id: "a".into(),
                    label: "alex4ndrus".into(),
                    backend: BackendId::Cline,
                    plan: None,
                    status: AccountStatus::Healthy,
                    reset_at: 0,
                    cooling_scopes: scopes.iter().map(|(s, d)| ((*s).to_string(), *d)).collect(),
                    inflight: 0,
                    sticky_sessions: 0,
                    month: Counters::default(),
                    quota: None,
                    observation: None,
                },
                MonitorAccount {
                    id: "b".into(),
                    label: "413x5".into(),
                    backend: BackendId::Cline,
                    plan: None,
                    status: AccountStatus::Healthy,
                    reset_at: 0,
                    cooling_scopes: [("credits".to_string(), 1_725_000)].into_iter().collect(),
                    inflight: 1,
                    sticky_sessions: 0,
                    observation: None,
                    month: Counters::default(),
                    quota: None,
                },
            ],
            month: Counters::default(),
            last_minute: Counters::default(),
            minute_bins: vec![0; 60],
            recent: vec![],
        }
    }

    fn render(width: u16) -> String {
        let mut app = App {
            snapshot: Some(snap(&[("credits", 1_725_000), ("cline-pass", 1_800_000)])),
            ..Default::default()
        };
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn scoped_cooling_is_fully_visible_at_wide_widths() {
        let screen = render(160);
        assert!(
            screen.contains("cooling credits 12m +1"),
            "STATE must render the scope and deadline without clipping:\n{screen}"
        );
    }

    #[test]
    fn status_still_has_room_at_a_narrow_width() {
        let screen = render(110);
        assert!(
            screen.contains("cooling credits"),
            "STATE must keep the scope name at 110 columns:\n{screen}"
        );
    }

    /// A snapshot whose longest backend id is `opencode-go`, the widest a [`BackendId`] can be
    /// today; both the accounts and the recent-outcomes table have to draw it in full.
    fn opencode_snapshot() -> MonitorSnapshot {
        let account = |label: &str, backend| MonitorAccount {
            id: label.into(),
            label: label.into(),
            backend,
            plan: None,
            status: AccountStatus::Healthy,
            reset_at: 0,
            cooling_scopes: Default::default(),
            inflight: 0,
            sticky_sessions: 0,
            month: Counters::default(),
            quota: None,
            observation: None,
        };
        MonitorSnapshot {
            recent: vec![crate::monitor::MonitorRequest {
                ts: 0,
                model: "cline/cline-pass/deepseek-v4.1-flash".into(),
                backend: Some(BackendId::OpencodeGo.as_str().into()),
                label: Some("alex4ndrus".into()),
                status: 200,
                duration_ms: 1_234,
            }],
            accounts: vec![
                account("alex4ndrus", BackendId::OpencodeGo),
                account("413x5", BackendId::Cline),
            ],
            ..snap(&[])
        }
    }

    fn opencode_render(width: u16) -> String {
        let mut app = App {
            snapshot: Some(opencode_snapshot()),
            ..Default::default()
        };
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, 30)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    #[test]
    fn the_widest_backend_id_is_drawn_in_full_at_realistic_widths() {
        let widest = BackendId::OpencodeGo.as_str();
        assert_eq!(widest, "opencode-go");
        let snap = opencode_snapshot();
        for width in [120u16, 132, 134, 160] {
            let screen = opencode_render(width);
            assert!(
                screen.matches(widest).count() >= 2,
                "`{widest}` must appear in both the accounts and the recent table at {width} columns:\n{screen}"
            );
            assert!(
                !screen.contains("opencode-g "),
                "`{widest}` must not be clipped at {width} columns:\n{screen}"
            );
            // Widening the backend must not come out of the model column's dominance or the fit.
            let widths: Vec<u16> = recent_columns(width, &snap)
                .into_iter()
                .map(|c| match c {
                    Constraint::Length(n) => n,
                    other => panic!("expected Length, got {other:?}"),
                })
                .collect();
            assert!(
                widths[1] >= widths[3],
                "MODEL ({}) should be at least as wide as ACCOUNT ({}) at {width}",
                widths[1],
                widths[3]
            );
            assert!(
                widths.iter().sum::<u16>() + 5 <= width - 2,
                "recent {widths:?} must fit the {}-cell inner area at {width}",
                width - 2
            );
        }
    }

    #[test]
    fn a_narrow_panel_still_fits_the_table_it_clips() {
        // Below this the table cannot hold every value; the content must still stay inside the
        // border rather than overflow it, and rendering must not panic.
        let snap = opencode_snapshot();
        for area in [30u16, 40, 50] {
            let widths: Vec<u16> = recent_columns(area, &snap)
                .into_iter()
                .map(|c| match c {
                    Constraint::Length(n) => n,
                    other => panic!("expected Length, got {other:?}"),
                })
                .collect();
            let total: u16 = widths.iter().sum::<u16>() + 5;
            assert!(
                total <= area - 2,
                "recent {widths:?} total {total} must fit the {}-cell inner area",
                area - 2
            );
        }
        // The fixed-shape columns keep their exact widths even when the ids no longer fit.
        let widths: Vec<u16> = recent_columns(60, &snap)
            .into_iter()
            .map(|c| match c {
                Constraint::Length(n) => n,
                other => panic!("expected Length, got {other:?}"),
            })
            .collect();
        assert_eq!(widths[0], 8, "TIME stays HH:MM:SS");
        assert_eq!(widths[4], 3, "STATUS stays three digits");
        assert!(widths[5] >= 7, "`<n> ms` keeps its digits: {}", widths[5]);
        for width in [60u16, 80, 99] {
            assert!(
                !opencode_render(width).is_empty(),
                "the dashboard must still render at {width} columns"
            );
        }
    }
}

#[cfg(test)]
mod fit_tests {
    use super::*;
    use crate::monitor::{Counters, MonitorAccount, MonitorSnapshot};

    fn snapshot() -> MonitorSnapshot {
        let account = |label: &str, scopes: &[(&str, i64)]| MonitorAccount {
            id: label.into(),
            label: label.into(),
            backend: BackendId::Cline,
            plan: None,
            status: AccountStatus::Healthy,
            reset_at: 0,
            cooling_scopes: scopes.iter().map(|(s, d)| ((*s).into(), *d)).collect(),
            inflight: 0,
            sticky_sessions: 0,
            month: Counters::default(),
            quota: None,
            observation: None,
        };
        MonitorSnapshot {
            now_ms: 1_000_000,
            month_start_ms: 0,
            accounts: vec![
                account(
                    "alex4ndrus",
                    &[("credits", 1_725_000), ("cline-pass", 1_800_000)],
                ),
                account("413x5", &[("credits", 1_725_000)]),
            ],
            month: Counters::default(),
            last_minute: Counters::default(),
            minute_bins: vec![0; 60],
            recent: vec![],
        }
    }

    /// `area_width` is the outer rect; `panel()` borders take one cell per side.
    fn widths_at(area_width: u16, compact: bool) -> Vec<u16> {
        let snap = snapshot();
        let (_, widths, weights) = account_columns(&snap, compact);
        grow_to_fit(widths, weights, area_width.saturating_sub(2))
            .into_iter()
            .map(|c| match c {
                Constraint::Length(n) => n,
                other => panic!("expected Length, got {other:?}"),
            })
            .collect()
    }

    #[test]
    fn widths_consume_exactly_the_available_area() {
        // column_spacing = 1 between every pair, plus the highlight symbol column.
        for area in [80u16, 100, 120, 140, 160, 200] {
            let w = widths_at(area, false);
            let spacing = (w.len() as u16 - 1) + HIGHLIGHT_WIDTH;
            assert_eq!(
                w.iter().sum::<u16>() + spacing,
                area - 2,
                "widths {w:?} must fill the block's inner width exactly"
            );
        }
    }

    #[test]
    fn every_growable_column_widens_as_the_terminal_grows() {
        let narrow = widths_at(120, false);
        let wide = widths_at(200, false);
        for (i, (n, w)) in narrow.iter().zip(&wide).enumerate() {
            if WEIGHT[i] == 0 {
                assert_eq!(w, n, "column {i} is fixed-count and must keep its width");
            } else {
                assert!(w > n, "column {i} did not grow: {n} at 120 -> {w} at 200");
            }
        }
    }

    #[test]
    fn recent_table_fits_every_panel_width() {
        const COLUMNS: u16 = 6;
        let snap = snapshot();
        // 41 is the smallest panel that can hold six one-cell columns plus separators.
        for area in [41u16, 50, 60, 80, 94, 100, 120, 160, 200] {
            let widths: Vec<u16> = recent_columns(area, &snap)
                .into_iter()
                .map(|c| match c {
                    Constraint::Length(n) => n,
                    other => panic!("expected Length, got {other:?}"),
                })
                .collect();
            let total: u16 = widths.iter().sum::<u16>() + (COLUMNS - 1);
            assert!(
                total <= area - 2,
                "recent {widths:?} total {total} must fit the {}-cell inner area",
                area - 2
            );
            // The model column must dominate: it is the widest value in the table.
            assert!(
                widths[1] >= widths[3],
                "MODEL ({}) should be at least as wide as ACCOUNT ({}) at {area}",
                widths[1],
                widths[3]
            );
        }
    }

    #[test]
    fn the_backend_cap_follows_the_widest_backend_id() {
        // The cap is derived, so a backend rename or addition moves it instead of clipping.
        let widest = BackendId::all()
            .iter()
            .map(|b| display_width(b.as_str()))
            .max()
            .unwrap();
        assert_eq!(usize::from(MAX_BACKEND_WIDTH), widest);
    }

    #[test]
    fn the_accounts_backend_column_fits_its_longest_value() {
        let mut snap = snapshot();
        for account in &mut snap.accounts {
            account.backend = BackendId::OpencodeGo;
        }
        let (_, widths, _) = account_columns(&snap, false);
        assert_eq!(
            widths[2],
            display_width(BackendId::OpencodeGo.as_str()) as u16,
            "BACKEND starts at the longest backend id, not just its header"
        );
        // The header is still the floor when every account holds a shorter id.
        let mut snap = snapshot();
        for account in &mut snap.accounts {
            account.backend = BackendId::Cline;
        }
        assert_eq!(
            account_columns(&snap, false).1[2],
            display_width("BACKEND") as u16
        );
    }

    #[test]
    fn the_recent_backend_column_fits_its_longest_value() {
        let mut snap = snapshot();
        snap.recent = vec![crate::monitor::MonitorRequest {
            ts: 0,
            model: "cline/cline-pass/deepseek-v4.1-flash".into(),
            backend: Some(BackendId::OpencodeGo.as_str().into()),
            label: Some("alex4ndrus".into()),
            status: 200,
            duration_ms: 1_234,
        }];
        let widths: Vec<u16> = recent_columns(160, &snap)
            .into_iter()
            .map(|c| match c {
                Constraint::Length(n) => n,
                other => panic!("expected Length, got {other:?}"),
            })
            .collect();
        assert!(
            widths[2] >= display_width(BackendId::OpencodeGo.as_str()) as u16,
            "BACKEND is {} cells, needs {}",
            widths[2],
            display_width(BackendId::OpencodeGo.as_str())
        );
    }

    #[test]
    fn nothing_overflows_even_when_the_panel_is_too_narrow_for_the_floors() {
        // Nine columns need at least one cell each plus eight separators and the highlight column,
        // so below 19 cells no layout can fit. Every panel a real terminal can present is above
        // that; this pins the behaviour from there up.
        // Nine one-cell columns plus eight separators and the highlight column need 19 inner
        // cells, and `panel()` borders add two more, so the smallest sane outer area is 21.
        const MIN_INNER: u16 = 9 + 8 + HIGHLIGHT_WIDTH;
        const MINIMUM: u16 = MIN_INNER + 2;
        for area in [MINIMUM, 25, 30, 40, 50] {
            let w = widths_at(area, false);
            let spacing = (w.len() as u16 - 1) + HIGHLIGHT_WIDTH;
            let total = w.iter().sum::<u16>() + spacing;
            assert!(
                total <= area - 2,
                "widths {w:?} total {total} must fit the {}-cell inner area",
                area - 2
            );
        }
    }

    #[test]
    fn widths_are_measured_in_terminal_cells() {
        // Double-width glyphs need two cells each; counting code points would clip them.
        assert_eq!(display_width("413x5"), 5);
        assert_eq!(
            display_width("\u{4f60}\u{597d}"),
            4,
            "CJK glyphs are two cells each"
        );
        assert_eq!(display_width("\u{1f600}"), 2, "emoji is two cells");
        assert_eq!(display_width("MONTH ATT \u{00b7} IN/OUT"), 18);
    }

    #[test]
    fn columns_start_at_their_content_width() {
        let snap = snapshot();
        let (_, widths, _) = account_columns(&snap, false);
        // Every account is Cline with no plan, so PLAN is sized by its header, not a fixed 12.
        assert_eq!(
            widths[1],
            display_width("PLAN") as u16,
            "PLAN starts at its header width"
        );
        // RESETS is the same for every account here.
        assert_eq!(
            widths[7],
            display_width("RESETS") as u16,
            "RESETS starts at its header width"
        );
        // ACCOUNT starts at the longest label, not a percentage of the panel.
        assert_eq!(widths[0], display_width("alex4ndrus") as u16);
    }

    #[test]
    fn state_keeps_the_full_scoped_status_at_every_width_in_test() {
        for area in [100u16, 120, 160, 200] {
            let w = widths_at(area, false);
            assert!(
                w[3] >= "cooling credits 12m +1".chars().count() as u16,
                "STATE is {} cells at {area}, needs {}",
                w[3],
                "cooling credits 12m +1".len()
            );
        }
    }
}

#[cfg(test)]
mod toggle_tests {
    use super::*;
    use crate::monitor::{Counters, MonitorAccount, MonitorSnapshot};
    use ratatui::backend::TestBackend;

    fn snapshot(statuses: &[(&str, AccountStatus)]) -> MonitorSnapshot {
        MonitorSnapshot {
            now_ms: 1_000_000,
            month_start_ms: 0,
            accounts: statuses
                .iter()
                .map(|(label, status)| MonitorAccount {
                    id: format!("id-{label}"),
                    label: (*label).into(),
                    backend: BackendId::Cline,
                    plan: None,
                    status: *status,
                    reset_at: 0,
                    cooling_scopes: Default::default(),
                    inflight: 0,
                    sticky_sessions: 0,
                    month: Counters::default(),
                    quota: None,
                    observation: None,
                })
                .collect(),
            month: Counters::default(),
            last_minute: Counters::default(),
            minute_bins: vec![0; 60],
            recent: vec![],
        }
    }

    fn app(statuses: &[(&str, AccountStatus)]) -> App {
        App {
            snapshot: Some(snapshot(statuses)),
            ..Default::default()
        }
    }

    #[test]
    fn only_the_visible_state_change_is_offered() {
        let mut enabled = app(&[("alex4ndrus", AccountStatus::Healthy)]);
        assert!(enabled.ask_toggle(true).is_err(), "already enabled");
        assert!(enabled.confirm.is_none(), "a refused ask must not prompt");
        enabled.ask_toggle(false).unwrap();
        let confirm = enabled.confirm.as_ref().unwrap();
        assert_eq!(confirm.id, "id-alex4ndrus");
        assert_eq!(confirm.label, "alex4ndrus");
        assert!(!confirm.enable);

        let mut disabled = app(&[("413x5", AccountStatus::Disabled)]);
        assert!(disabled.ask_toggle(false).is_err(), "already disabled");
        disabled.ask_toggle(true).unwrap();
        assert!(disabled.confirm.as_ref().unwrap().enable);

        // `AuthError` is recoverable through the same enable path the admin API exposes.
        let mut failed = app(&[("b", AccountStatus::AuthError)]);
        failed.ask_toggle(true).unwrap();
        assert!(failed.confirm.is_some());
    }

    #[test]
    fn a_cooling_account_is_still_enabled() {
        // Cooling is upstream quota, not an operator disable: offering enable would let a keypress
        // clear a deadline the operator did not ask to clear.
        let mut cooling = app(&[("c", AccountStatus::Cooling)]);
        assert!(cooling.ask_toggle(true).is_err());
        cooling.ask_toggle(false).unwrap();
    }

    #[test]
    fn the_prompt_follows_the_selection() {
        let mut app = app(&[
            ("alex4ndrus", AccountStatus::Healthy),
            ("413x5", AccountStatus::Healthy),
        ]);
        app.selected = 1;
        app.ask_toggle(false).unwrap();
        assert_eq!(app.confirm.as_ref().unwrap().label, "413x5");
        // A move while the prompt is open does not retarget it.
        app.down();
        assert_eq!(app.confirm.as_ref().unwrap().label, "413x5");
    }

    #[test]
    fn an_empty_snapshot_prompts_nothing() {
        let mut empty = App::default();
        assert!(empty.ask_toggle(false).is_err());
        assert!(empty.confirm.is_none());
    }

    #[test]
    fn action_url_is_one_path_segment() {
        assert_eq!(
            action_url("http://127.0.0.1:8080", "ab-12_cd", true).unwrap(),
            "http://127.0.0.1:8080/admin/api/accounts/ab-12_cd/enable"
        );
        assert_eq!(
            action_url("http://127.0.0.1:8080", "ab", false).unwrap(),
            "http://127.0.0.1:8080/admin/api/accounts/ab/disable"
        );
        for bad in ["", "..", "a/b", "a?x=1", "a b", "a#f", "%2e%2e"] {
            assert!(
                action_url("http://127.0.0.1:8080", bad, true).is_err(),
                "{bad:?} must not reach the admin URL"
            );
        }
    }

    #[test]
    fn the_confirmation_prompt_names_the_account_and_action() {
        let mut app = app(&[("alex4ndrus", AccountStatus::Healthy)]);
        app.ask_toggle(false).unwrap();
        app.notice = Some(Notice::ok("Disable alex4ndrus…"));
        let screen = render(&mut app, 134);
        assert!(
            screen.contains("Disable alex4ndrus?"),
            "the prompt must name the action and the account:\n{screen}"
        );
        assert!(
            screen.contains("y confirm"),
            "the prompt must say how to confirm"
        );
        assert!(
            screen.contains("Disable alex4ndrus…"),
            "the footer shows the notice"
        );
    }

    /// Stand up a one-shot HTTP server that records the request line and headers, so the toggle can
    /// be checked against the wire the server actually receives rather than against its own helpers.
    fn one_shot_server(status: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if std::io::BufRead::read_line(&mut reader, &mut line).unwrap() == 0 {
                    break;
                }
                let done = line == "\r\n";
                request.push_str(&line);
                if done {
                    break;
                }
            }
            use std::io::Write;
            let _ = stream.write_all(
                format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .as_bytes(),
            );
            request
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    fn admin(base: String) -> Admin {
        Admin {
            client: reqwest::blocking::Client::builder().build().unwrap(),
            url: base,
            token: "secret-token".into(),
        }
    }

    #[test]
    fn a_confirmed_toggle_posts_to_the_admin_api_with_the_token() {
        for (enable, action) in [(false, "disable"), (true, "enable")] {
            let (base, server) = one_shot_server("204 No Content");
            admin(base).set_enabled("ab-12", enable).unwrap();
            let request = server.join().unwrap();
            assert!(
                request.starts_with(&format!("POST /admin/api/accounts/ab-12/{action} HTTP/1.1")),
                "{request}"
            );
            let lowered = request.to_ascii_lowercase();
            assert!(
                lowered.contains("authorization: bearer secret-token"),
                "the admin token must authenticate the write:\n{request}"
            );
        }
    }

    #[test]
    fn a_rejected_toggle_reports_the_status() {
        let (base, server) = one_shot_server("401 Unauthorized");
        let error = admin(base).set_enabled("ab", true).unwrap_err();
        let _ = server.join();
        assert!(error.contains("401"), "{error}");

        let (base, server) = one_shot_server("404 Not Found");
        let error = admin(base).set_enabled("ab", true).unwrap_err();
        let _ = server.join();
        assert!(error.contains("no longer exists"), "{error}");
    }

    #[test]
    fn an_unsafe_id_never_reaches_the_network() {
        // No server is listening on this port: a request attempt would fail differently.
        let admin = admin("http://127.0.0.1:1".into());
        let error = admin.set_enabled("../secrets", true).unwrap_err();
        assert!(error.contains("path segment"), "{error}");
    }

    #[test]
    fn a_notice_expires() {
        let notice = Notice::ok("done");
        assert!(notice.visible());
        let stale = Notice {
            at: Instant::now() - Notice::TTL - Duration::from_secs(1),
            ..notice
        };
        assert!(!stale.visible());
    }

    fn render(app: &mut App, width: u16) -> String {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, 30)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }
}
