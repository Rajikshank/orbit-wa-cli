//! Orbit's interactive terminal control surface.
//!
//! Rendering only reads Orbit's local projection and typed IPC. It never
//! launches the connector, keeping keyboard interaction fast and popup-free.

use std::{
    io::{self, IsTerminal},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
        MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use serde_json::Value;

use crate::{
    config::OrbitPaths,
    ipc,
    model::{Request, Response, SignalEntry},
    store::Store,
};

/// Curated palettes map identical semantic roles to accessible color sets.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ThemeId {
    #[default]
    MidnightIndigo,
    ArcticLight,
    Ember,
    Moss,
    HighContrast,
}

impl ThemeId {
    const ALL: [(Self, &'static str); 5] = [
        (Self::MidnightIndigo, "Midnight Indigo"),
        (Self::ArcticLight, "Arctic Light"),
        (Self::Ember, "Ember"),
        (Self::Moss, "Moss"),
        (Self::HighContrast, "High Contrast"),
    ];

    const fn palette(self) -> Palette {
        match self {
            Self::MidnightIndigo => Palette::new(
                (5, 10, 30),
                (13, 24, 58),
                (235, 241, 255),
                (145, 158, 190),
                (158, 119, 255),
                (31, 211, 198),
                (255, 108, 110),
            ),
            Self::ArcticLight => Palette::new(
                (242, 247, 252),
                (222, 234, 246),
                (18, 34, 53),
                (79, 101, 124),
                (47, 91, 211),
                (0, 128, 128),
                (201, 69, 53),
            ),
            Self::Ember => Palette::new(
                (24, 12, 10),
                (55, 25, 18),
                (255, 238, 220),
                (190, 146, 119),
                (255, 122, 54),
                (255, 190, 75),
                (255, 82, 82),
            ),
            Self::Moss => Palette::new(
                (7, 20, 17),
                (16, 45, 35),
                (230, 245, 232),
                (132, 168, 145),
                (111, 210, 139),
                (74, 198, 184),
                (239, 142, 92),
            ),
            Self::HighContrast => Palette {
                background: Color::Black,
                surface: Color::Rgb(30, 30, 30),
                text: Color::White,
                muted: Color::Gray,
                accent: Color::Yellow,
                success: Color::Cyan,
                attention: Color::Red,
            },
        }
    }

    const fn key(self) -> &'static str {
        match self {
            Self::MidnightIndigo => "midnight-indigo",
            Self::ArcticLight => "arctic-light",
            Self::Ember => "ember",
            Self::Moss => "moss",
            Self::HighContrast => "high-contrast",
        }
    }

    fn from_key(value: &str) -> Option<Self> {
        match value.trim() {
            "midnight-indigo" => Some(Self::MidnightIndigo),
            "arctic-light" => Some(Self::ArcticLight),
            "ember" => Some(Self::Ember),
            "moss" => Some(Self::Moss),
            "high-contrast" => Some(Self::HighContrast),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
struct Palette {
    background: Color,
    surface: Color,
    text: Color,
    muted: Color,
    accent: Color,
    success: Color,
    attention: Color,
}

impl Palette {
    const fn new(
        background: (u8, u8, u8),
        surface: (u8, u8, u8),
        text: (u8, u8, u8),
        muted: (u8, u8, u8),
        accent: (u8, u8, u8),
        success: (u8, u8, u8),
        attention: (u8, u8, u8),
    ) -> Self {
        Self {
            background: Color::Rgb(background.0, background.1, background.2),
            surface: Color::Rgb(surface.0, surface.1, surface.2),
            text: Color::Rgb(text.0, text.1, text.2),
            muted: Color::Rgb(muted.0, muted.1, muted.2),
            accent: Color::Rgb(accent.0, accent.1, accent.2),
            success: Color::Rgb(success.0, success.1, success.2),
            attention: Color::Rgb(attention.0, attention.1, attention.2),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Mode {
    #[default]
    Normal,
    Theme,
    Search,
    Help,
    Compose,
}

#[derive(Debug, Eq, PartialEq)]
enum Action {
    None,
    Quit,
    Search(String),
    SendText { to: String, message: String },
}

/// Pure application state, separated from terminal and IPC for deterministic tests.
#[derive(Debug, Default)]
pub struct App {
    pub theme: ThemeId,
    pub theme_menu_open: bool,
    signals: Vec<SignalEntry>,
    selected: usize,
    mode: Mode,
    query: String,
    composer: String,
    connected: bool,
    privacy: bool,
    notice: Option<String>,
}

impl App {
    #[must_use]
    pub fn new(mut signals: Vec<SignalEntry>, connected: bool) -> Self {
        sanitize_signals(&mut signals);
        Self {
            signals,
            connected,
            ..Self::default()
        }
    }

    fn selected_signal(&self) -> Option<&SignalEntry> {
        self.signals.get(self.selected)
    }

    fn replace_signals(&mut self, mut signals: Vec<SignalEntry>) {
        // Refreshes can prepend new messages. Preserve the stable message key
        // instead of silently moving the user's selection to a different row.
        let selected_id = self
            .selected_signal()
            .map(|signal| signal.message_id.clone());
        sanitize_signals(&mut signals);
        self.signals = signals;
        self.selected = selected_id
            .and_then(|id| {
                self.signals
                    .iter()
                    .position(|signal| signal.message_id == id)
            })
            .unwrap_or_else(|| self.selected.min(self.signals.len().saturating_sub(1)));
    }

    fn handle_char(&mut self, character: char) {
        if !self.theme_menu_open {
            return;
        }
        self.theme = match character {
            '1' => ThemeId::MidnightIndigo,
            '2' => ThemeId::ArcticLight,
            '3' => ThemeId::Ember,
            '4' => ThemeId::Moss,
            '5' => ThemeId::HighContrast,
            _ => return,
        };
        self.theme_menu_open = false;
        self.mode = Mode::Normal;
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        match self.mode {
            Mode::Theme => self.handle_theme_key(key),
            Mode::Search => self.handle_search_key(key),
            Mode::Compose => self.handle_compose_key(key),
            Mode::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
                    self.mode = Mode::Normal;
                }
                Action::None
            }
            Mode::Normal => self.handle_normal_key(key),
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> Action {
        match mouse.kind {
            MouseEventKind::ScrollDown => {
                self.selected = (self.selected + 3).min(self.signals.len().saturating_sub(1));
            }
            MouseEventKind::ScrollUp => self.selected = self.selected.saturating_sub(3),
            MouseEventKind::Down(MouseButton::Left) => {
                if self.theme_menu_open && mouse.row >= 4 && mouse.row <= 8 {
                    let theme_key = match mouse.row {
                        4 => '1',
                        5 => '2',
                        6 => '3',
                        7 => '4',
                        8 => '5',
                        _ => unreachable!("theme row is range-checked"),
                    };
                    self.handle_char(theme_key);
                    return Action::None;
                }
                if mouse.row < 3 && mouse.column >= area.width.saturating_sub(10) {
                    self.mode = Mode::Theme;
                    self.theme_menu_open = true;
                    return Action::None;
                }
                let navigation_width = if area.width >= 120 {
                    18
                } else if area.width >= 85 {
                    14
                } else {
                    0
                };
                if navigation_width > 0 && mouse.column < navigation_width && mouse.row >= 3 {
                    match mouse.row - 3 {
                        2 => {
                            self.mode = Mode::Search;
                            self.query.clear();
                        }
                        4 => {
                            if self.selected_signal().is_some() {
                                self.mode = Mode::Compose;
                            }
                        }
                        6 => {
                            self.mode = Mode::Theme;
                            self.theme_menu_open = true;
                        }
                        8 => self.privacy = !self.privacy,
                        _ => {}
                    }
                    return Action::None;
                }
                let evidence_width = if area.width >= 120 {
                    38
                } else if area.width >= 85 {
                    30
                } else {
                    0
                };
                if mouse.column >= navigation_width
                    && mouse.column < area.width.saturating_sub(evidence_width)
                    && mouse.row >= 4
                {
                    let index = usize::from((mouse.row - 4) / 2);
                    if index < self.signals.len() {
                        self.selected = index;
                    }
                }
            }
            _ => {}
        }
        Action::None
    }

    fn handle_paste(&mut self, text: &str) {
        match self.mode {
            Mode::Search => append_terminal_input(&mut self.query, text, false),
            Mode::Compose => append_terminal_input(&mut self.composer, text, true),
            Mode::Normal | Mode::Theme | Mode::Help => {}
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Action {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
            self.mode = Mode::Help;
            return Action::None;
        }
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(self.signals.len().saturating_sub(1));
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Home => {
                self.selected = 0;
                Action::None
            }
            KeyCode::End => {
                self.selected = self.signals.len().saturating_sub(1);
                Action::None
            }
            KeyCode::PageDown => {
                self.selected = (self.selected + 10).min(self.signals.len().saturating_sub(1));
                Action::None
            }
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(10);
                Action::None
            }
            KeyCode::Char('t') => {
                self.mode = Mode::Theme;
                self.theme_menu_open = true;
                Action::None
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.query.clear();
                Action::None
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help;
                Action::None
            }
            KeyCode::Char('p') => {
                self.privacy = !self.privacy;
                Action::None
            }
            KeyCode::Char('c') | KeyCode::Enter => {
                if self.selected_signal().is_some() {
                    self.mode = Mode::Compose;
                } else {
                    self.notice = Some("Select a conversation before composing".into());
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_theme_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('t') => {
                self.theme_menu_open = false;
                self.mode = Mode::Normal;
            }
            KeyCode::Char(character) => self.handle_char(character),
            _ => {}
        }
        Action::None
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.query.clear();
                self.mode = Mode::Normal;
                Action::None
            }
            KeyCode::Enter => {
                // Keep the active query so periodic refreshes do not replace
                // search results with the unfiltered stream.
                let query = self.query.clone();
                self.mode = Mode::Normal;
                Action::Search(query)
            }
            KeyCode::Backspace => {
                self.query.pop();
                Action::None
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                append_terminal_input(&mut self.query, &character.to_string(), false);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_compose_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                Action::None
            }
            KeyCode::F(10) => self.compose_send_action(),
            KeyCode::Char('s') | KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.compose_send_action()
            }
            KeyCode::Enter => {
                // Plain Enter must remain safe for multi-line input and for
                // terminals that do not emit bracketed-paste events reliably.
                append_terminal_input(&mut self.composer, "\n", true);
                Action::None
            }
            KeyCode::Backspace => {
                self.composer.pop();
                Action::None
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                append_terminal_input(&mut self.composer, &character.to_string(), true);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn compose_send_action(&mut self) -> Action {
        let Some(to) = self.selected_signal().map(|signal| signal.chat_jid.clone()) else {
            self.mode = Mode::Normal;
            return Action::None;
        };
        let message = self.composer.trim().to_owned();
        if message.is_empty() {
            self.notice = Some("Message cannot be empty".into());
            return Action::None;
        }
        self.mode = Mode::Normal;
        Action::SendText { to, message }
    }
}

/// Enter the alternate-screen TUI. Redirected output remains ANSI-free.
pub async fn run(paths: &OrbitPaths) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("`orbit ui` requires an interactive terminal; use commands with --json for scripts");
    }
    let store = Store::new(paths.database.clone());
    let connected = ipc::request(&paths.ipc_name(), &Request::Ping)
        .await
        .is_ok();
    let mut app = App::new(store.signal_stream(100)?, connected);
    app.theme = load_theme(paths);
    let mut terminal = TerminalGuard::enter()?;
    let mut last_refresh = Instant::now();
    loop {
        terminal.draw(|frame| render(frame, &app))?;
        if event::poll(Duration::from_millis(250)).context("poll terminal input")? {
            let previous_theme = app.theme;
            let action = match event::read().context("read terminal input")? {
                Event::Key(key) => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse, terminal.size()?),
                Event::Paste(text) => {
                    app.handle_paste(&text);
                    Action::None
                }
                Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => Action::None,
            };
            if app.theme != previous_theme {
                save_theme(paths, app.theme)?;
            }
            match action {
                Action::None => {}
                Action::Quit => break,
                Action::Search(query) => {
                    let signals = if query.trim().is_empty() {
                        store.signal_stream(100)?
                    } else {
                        search_entries(&store.search(&query, 100)?)
                    };
                    app.replace_signals(signals);
                    app.notice = Some(format!("{} local result(s)", app.signals.len()));
                }
                Action::SendText { to, message } => {
                    app.notice = Some("Sending — waiting for connector acceptance…".into());
                    terminal.draw(|frame| render(frame, &app))?;
                    match ipc::request(&paths.ipc_name(), &Request::SendText { to, message }).await
                    {
                        Ok(response) => apply_send_response(&mut app, response),
                        Err(error) => {
                            apply_send_response(&mut app, Response::failure(format!("{error:#}")));
                        }
                    }
                }
            }
        }
        if last_refresh.elapsed() >= Duration::from_secs(2)
            && app.mode == Mode::Normal
            && app.query.is_empty()
        {
            if let Ok(signals) = store.signal_stream(100) {
                app.replace_signals(signals);
            }
            app.connected = ipc::request(&paths.ipc_name(), &Request::Ping)
                .await
                .is_ok();
            last_refresh = Instant::now();
        }
    }
    Ok(())
}

fn search_entries(value: &Value) -> Vec<SignalEntry> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| SignalEntry {
            message_id: string_field(entry, "id"),
            chat_jid: string_field(entry, "chat_jid"),
            chat_name: string_field(entry, "chat_name"),
            sender_name: string_field(entry, "sender_name"),
            timestamp: string_field(entry, "timestamp"),
            text: string_field(entry, "text"),
            content_kind: string_field(entry, "content_kind"),
            filename: string_field(entry, "filename"),
            from_me: entry
                .get("from_me")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            edited: false,
            revoked: false,
        })
        .collect()
}

fn string_field(value: &Value, name: &str) -> String {
    value
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn sanitize_signals(signals: &mut [SignalEntry]) {
    for signal in signals {
        for field in [
            &mut signal.message_id,
            &mut signal.chat_jid,
            &mut signal.chat_name,
            &mut signal.sender_name,
            &mut signal.timestamp,
            &mut signal.text,
            &mut signal.content_kind,
            &mut signal.filename,
        ] {
            // Messages and contact names are remote input. Replacing C0/C1
            // controls prevents terminal escape injection and keeps list rows
            // one-line without changing the durable database representation.
            *field = field
                .chars()
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .collect();
        }
    }
}

fn append_terminal_input(destination: &mut String, input: &str, preserve_newlines: bool) {
    const MAX_INPUT_CHARS: usize = 16_384;
    let remaining = MAX_INPUT_CHARS.saturating_sub(destination.chars().count());
    destination.extend(input.chars().take(remaining).map(|character| {
        if preserve_newlines && character == '\n' {
            '\n'
        } else if character.is_control() {
            ' '
        } else {
            character
        }
    }));
}

fn apply_send_response(app: &mut App, response: Response) {
    if response.ok {
        // Connector acceptance, including an uncertain warning, means retrying
        // could duplicate a real message. Clear the draft only at this point.
        app.composer.clear();
        app.notice = Some(response.warning.map_or_else(
            || "Sent — connector accepted the message".into(),
            |warning| format!("Possibly delivered — {warning}; do not retry"),
        ));
    } else {
        app.notice = Some(format!(
            "Send failed — {}",
            response.error.unwrap_or_else(|| "unknown error".into())
        ));
    }
}

fn theme_path(paths: &OrbitPaths) -> std::path::PathBuf {
    paths.root.join("ui-theme")
}

fn load_theme(paths: &OrbitPaths) -> ThemeId {
    std::fs::read_to_string(theme_path(paths))
        .ok()
        .and_then(|value| ThemeId::from_key(&value))
        .unwrap_or_default()
}

fn save_theme(paths: &OrbitPaths, theme: ThemeId) -> Result<()> {
    // A standalone, non-secret preference file avoids changing the daemon
    // configuration schema merely for presentation state.
    std::fs::write(theme_path(paths), theme.key()).context("save Orbit UI theme")
}

fn render(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let palette = app.theme.palette();
    frame.render_widget(
        Block::default().style(Style::default().bg(palette.background)),
        area,
    );
    if area.width < 60 || area.height < 18 {
        render_minimum_size(frame, area, palette);
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(10),
            Constraint::Length(3),
        ])
        .split(area);
    render_header(frame, rows[0], app, palette);
    render_body(frame, rows[1], app, palette);
    render_command_bar(frame, rows[2], app, palette);
    match app.mode {
        Mode::Theme => render_theme_menu(frame, area, app, palette),
        Mode::Help => render_help(frame, area, palette),
        Mode::Compose => render_composer(frame, area, app, palette),
        Mode::Normal | Mode::Search => {}
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(22),
            Constraint::Min(20),
            Constraint::Length(39),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " ◉ ",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "O R B I T",
                Style::default()
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        columns[0],
    );
    if area.width >= 95 {
        frame.render_widget(
            Paragraph::new("LOCAL-FIRST WHATSAPP COMMAND CENTER")
                .style(Style::default().fg(palette.muted))
                .alignment(Alignment::Center),
            columns[1],
        );
    }
    let status = if app.connected {
        "● CONNECTED"
    } else {
        "○ OFFLINE"
    };
    frame.render_widget(
        Paragraph::new(format!("{status}  ↻ SYNC  ▣ LOCAL  ◉ THEME "))
            .style(Style::default().fg(if app.connected {
                palette.success
            } else {
                palette.attention
            }))
            .alignment(Alignment::Right),
        columns[2],
    );
}

fn render_body(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let constraints = if area.width >= 120 {
        vec![
            Constraint::Length(18),
            Constraint::Min(52),
            Constraint::Length(38),
        ]
    } else if area.width >= 85 {
        vec![
            Constraint::Length(14),
            Constraint::Min(46),
            Constraint::Length(30),
        ]
    } else {
        vec![Constraint::Min(1)]
    };
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);
    if columns.len() == 1 {
        render_signal_stream(frame, columns[0], app, palette);
    } else {
        render_navigation(frame, columns[0], palette);
        render_signal_stream(frame, columns[1], app, palette);
        render_evidence(frame, columns[2], app, palette);
    }
}

fn render_navigation(frame: &mut Frame<'_>, area: Rect, palette: Palette) {
    let text = vec![
        Line::styled(
            " ≋  STREAM",
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::styled(" /  SEARCH", Style::default().fg(palette.muted)),
        Line::raw(""),
        Line::styled(" ✎  COMPOSE", Style::default().fg(palette.muted)),
        Line::raw(""),
        Line::styled(" ◉  THEMES", Style::default().fg(palette.muted)),
        Line::raw(""),
        Line::styled(" ▣  PRIVACY", Style::default().fg(palette.muted)),
    ];
    frame.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(Style::default().fg(palette.surface)),
        ),
        area,
    );
}

fn render_signal_stream(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let title = format!(" SIGNAL STREAM  ·  {} local items ", app.signals.len());
    let items = if app.signals.is_empty() {
        vec![ListItem::new(Line::styled(
            "No local signals. Press / to search or wait for sync.",
            Style::default().fg(palette.muted),
        ))]
    } else {
        app.signals
            .iter()
            .map(|signal| {
                let (sender, chat, body) = if app.privacy {
                    ("Hidden", "Private conversation", "••••••••••••••••")
                } else {
                    (
                        display_or(&signal.sender_name, "Unknown sender"),
                        display_or(&signal.chat_name, &signal.chat_jid),
                        signal_body(signal),
                    )
                };
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(
                            format!(
                                " {} {:>5}  ",
                                signal_icon(signal),
                                compact_time(&signal.timestamp)
                            ),
                            Style::default().fg(icon_color(signal, palette)),
                        ),
                        Span::styled(
                            sender.to_owned(),
                            Style::default()
                                .fg(palette.text)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("  ·  {chat}"), Style::default().fg(palette.muted)),
                    ]),
                    Line::styled(
                        format!("              {body}"),
                        Style::default().fg(palette.text),
                    ),
                ])
            })
            .collect()
    };
    let mut state = ListState::default().with_selected(Some(app.selected));
    let list = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(Style::default().fg(palette.surface)),
        )
        .highlight_style(
            Style::default()
                .bg(palette.surface)
                .fg(palette.text)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▌");
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_evidence(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let lines = app.selected_signal().map_or_else(
        || {
            vec![Line::styled(
                "No signal selected",
                Style::default().fg(palette.muted),
            )]
        },
        |signal| {
            let hidden = app.privacy;
            vec![
                label_value("TYPE", signal_kind(signal), palette),
                label_value(
                    "FROM",
                    if hidden {
                        "Hidden"
                    } else {
                        display_or(&signal.sender_name, "Unknown")
                    },
                    palette,
                ),
                label_value(
                    "CHAT",
                    if hidden {
                        "Private"
                    } else {
                        display_or(&signal.chat_name, &signal.chat_jid)
                    },
                    palette,
                ),
                label_value("TIME", &signal.timestamp, palette),
                Line::raw(""),
                Line::styled(
                    "MESSAGE",
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    if hidden {
                        "••••••••••••••••"
                    } else {
                        signal_body(signal)
                    },
                    Style::default().fg(palette.text),
                ),
                Line::raw(""),
                Line::styled(
                    "EVIDENCE",
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                label_value("Source", "Orbit local projection", palette),
                label_value(
                    "Message",
                    if hidden { "Hidden" } else { &signal.message_id },
                    palette,
                ),
                label_value(
                    "Identity",
                    if hidden { "Hidden" } else { &signal.chat_jid },
                    palette,
                ),
                Line::raw(""),
                Line::styled(
                    "LOCAL STATE",
                    Style::default()
                        .fg(palette.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    "▣ Stored only on this device",
                    Style::default().fg(palette.success),
                ),
                Line::styled(
                    "No cloud processing by Orbit",
                    Style::default().fg(palette.muted),
                ),
            ]
        },
    );
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(" EVENT DETAILS ")
                .borders(Borders::LEFT | Borders::TOP | Borders::BOTTOM)
                .border_style(Style::default().fg(palette.surface)),
        ),
        area,
    );
}

fn render_command_bar(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let prompt = if app.mode == Mode::Search {
        format!(" / {}_", app.query)
    } else if let Some(notice) = &app.notice {
        format!(" orbit› {notice}")
    } else {
        " orbit› / search · c compose · t themes · p privacy · ? help · q quit".into()
    };
    frame.render_widget(
        Paragraph::new(prompt)
            .style(Style::default().fg(if app.notice.is_some() {
                palette.success
            } else {
                palette.text
            }))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(palette.accent)),
            ),
        area,
    );
}

fn render_theme_menu(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let width = 32.min(area.width.saturating_sub(4));
    let popup = Rect::new(
        area.right().saturating_sub(width + 2),
        3,
        width,
        11.min(area.height - 4),
    );
    let mut lines = Vec::new();
    for (index, (id, name)) in ThemeId::ALL.iter().enumerate() {
        let marker = if *id == app.theme { "●" } else { "○" };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}  {marker} ", index + 1),
                Style::default().fg(if *id == app.theme {
                    palette.accent
                } else {
                    palette.muted
                }),
            ),
            Span::styled(*name, Style::default().fg(palette.text)),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        " Auto terminal  ON",
        Style::default().fg(palette.success),
    ));
    lines.push(Line::styled(
        " Motion         Reduced",
        Style::default().fg(palette.muted),
    ));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" ◉ THEME ")
                .borders(Borders::ALL)
                .style(Style::default().bg(palette.surface))
                .border_style(Style::default().fg(palette.accent)),
        ),
        popup,
    );
}

fn render_help(frame: &mut Frame<'_>, area: Rect, palette: Palette) {
    let popup = centered_rect(60, 18, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                "KEYBOARD",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::raw("j/k or ↑/↓  Move through signals"),
            Line::raw("/            Search the complete local index"),
            Line::raw("Enter or c   Compose to the selected conversation"),
            Line::raw("F10/Ctrl+S   Send; uncertain sends are never retried"),
            Line::raw("Enter        Insert a newline"),
            Line::raw("t            Open the theme switcher"),
            Line::raw("p            Toggle Privacy Curtain"),
            Line::raw("q            Exit the UI; syncing continues"),
            Line::raw(""),
            Line::styled(
                "Press ? or Esc to close",
                Style::default().fg(palette.muted),
            ),
        ])
        .style(Style::default().fg(palette.text).bg(palette.surface))
        .block(
            Block::default()
                .title(" ? ORBIT HELP ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(palette.accent)),
        ),
        popup,
    );
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let popup = centered_rect(72, 16, area);
    let recipient = app.selected_signal().map_or("Unknown", |signal| {
        if app.privacy {
            "Hidden conversation"
        } else {
            display_or(&signal.chat_name, &signal.chat_jid)
        }
    });
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(vec![
                Span::styled("TO  ", Style::default().fg(palette.muted)),
                Span::styled(
                    recipient,
                    Style::default()
                        .fg(palette.success)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::raw(""),
            Line::styled(
                if app.composer.is_empty() {
                    "Write a message…"
                } else {
                    &app.composer
                },
                Style::default().fg(if app.composer.is_empty() {
                    palette.muted
                } else {
                    palette.text
                }),
            ),
            Line::raw(""),
            Line::styled(
                "F10/Ctrl+S send    Enter newline    Esc retain draft",
                Style::default().fg(palette.muted),
            ),
        ])
        .wrap(Wrap { trim: false })
        .style(Style::default().bg(palette.background))
        .block(
            Block::default()
                .title(" ✎ COMPOSE ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(palette.accent)),
        ),
        popup,
    );
}

fn render_minimum_size(frame: &mut Frame<'_>, area: Rect, palette: Palette) {
    frame.render_widget(Paragraph::new("◉ ORBIT\n\nTerminal is too small.\nResize to at least 60×18.\n\nAll regular CLI commands remain available.").style(Style::default().fg(palette.text).bg(palette.background)).alignment(Alignment::Center), area);
}

fn signal_icon(signal: &SignalEntry) -> &'static str {
    if signal.revoked {
        "⊘"
    } else if signal.edited {
        "✎"
    } else if !signal.filename.is_empty() {
        "⌕"
    } else if signal.text.contains('@') {
        "@"
    } else {
        "◇"
    }
}
fn icon_color(signal: &SignalEntry, palette: Palette) -> Color {
    if signal.revoked {
        palette.attention
    } else if signal.edited {
        Color::Yellow
    } else {
        palette.accent
    }
}
fn signal_kind(signal: &SignalEntry) -> &'static str {
    if signal.revoked {
        "Revoked message"
    } else if signal.edited {
        "Edited message"
    } else if !signal.filename.is_empty() {
        "Shared file"
    } else if signal.text.contains('@') {
        "Mention"
    } else if signal.from_me {
        "Sent message"
    } else {
        "Received message"
    }
}
fn signal_body(signal: &SignalEntry) -> &str {
    if signal.revoked {
        "Message revoked"
    } else if !signal.filename.is_empty() {
        &signal.filename
    } else if signal.text.is_empty() {
        display_or(&signal.content_kind, "Message")
    } else {
        &signal.text
    }
}
fn display_or<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}
fn compact_time(timestamp: &str) -> &str {
    timestamp
        .get(11..16)
        .unwrap_or_else(|| timestamp.get(..5).unwrap_or(timestamp))
}
fn label_value<'a>(label: &'a str, value: &'a str, palette: Palette) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{label:<9}"), Style::default().fg(palette.muted)),
        Span::styled(value, Style::default().fg(palette.text)),
    ])
}
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(4));
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}
impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("enable terminal raw mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        ) {
            let _ = disable_raw_mode();
            return Err(error).context("enter alternate screen");
        }
        let terminal =
            Terminal::new(CrosstermBackend::new(stdout)).context("initialize terminal")?;
        Ok(Self { terminal })
    }
    fn draw(&mut self, render: impl FnOnce(&mut Frame<'_>)) -> Result<()> {
        self.terminal.draw(render).context("draw Orbit UI")?;
        Ok(())
    }

    fn size(&self) -> Result<Rect> {
        self.terminal
            .size()
            .map(|size| Rect::new(0, 0, size.width, size.height))
            .context("read terminal size")
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = self.terminal.show_cursor();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn signal(id: &str) -> SignalEntry {
        SignalEntry {
            message_id: id.into(),
            chat_jid: "team@g.us".into(),
            chat_name: "Project Falcon".into(),
            sender_name: "Priya".into(),
            timestamp: "2026-08-26T09:42:00Z".into(),
            from_me: false,
            text: "Should we move the launch?".into(),
            content_kind: "text".into(),
            filename: String::new(),
            edited: false,
            revoked: false,
        }
    }

    #[test]
    fn number_keys_select_the_five_curated_themes() {
        let mut app = App::default();
        for (key, expected) in [
            ('1', ThemeId::MidnightIndigo),
            ('2', ThemeId::ArcticLight),
            ('3', ThemeId::Ember),
            ('4', ThemeId::Moss),
            ('5', ThemeId::HighContrast),
        ] {
            app.theme_menu_open = true;
            app.mode = Mode::Theme;
            app.handle_char(key);
            assert_eq!(app.theme, expected);
            assert!(!app.theme_menu_open);
        }
    }

    #[test]
    fn keyboard_navigation_stays_inside_the_stream() {
        let mut app = App::new(vec![signal("1"), signal("2")], true);
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        assert_eq!(app.selected, 0);
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn enter_opens_the_selected_conversation_composer() {
        let mut app = App::new(vec![signal("1")], true);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
        assert_eq!(app.mode, Mode::Compose);
    }

    #[test]
    fn mouse_navigation_and_scrolling_reach_real_actions() {
        let mut app = App::new(
            vec![signal("1"), signal("2"), signal("3"), signal("4")],
            true,
        );
        let area = Rect::new(0, 0, 160, 40);
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 30,
                row: 10,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert_eq!(app.selected, 3);
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 4,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert_eq!(app.mode, Mode::Search);
    }

    #[test]
    fn control_c_always_restores_and_exits_from_a_modal() {
        let mut app = App::new(vec![signal("1")], true);
        app.mode = Mode::Compose;
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }

    #[test]
    fn pasted_text_works_in_search_and_composer_without_controls() {
        let mut app = App::new(vec![signal("1")], true);
        app.mode = Mode::Search;
        app.handle_paste("launch\nplan\u{1b}[2J");
        assert_eq!(app.query, "launch plan [2J");

        app.mode = Mode::Compose;
        app.handle_paste("line one\nline two\u{7}");
        assert_eq!(app.composer, "line one\nline two ");
    }

    #[test]
    fn every_theme_mode_and_supported_size_renders_without_panicking() {
        for (theme, _) in ThemeId::ALL {
            for mode in [
                Mode::Normal,
                Mode::Theme,
                Mode::Search,
                Mode::Help,
                Mode::Compose,
            ] {
                for (width, height) in [(60, 18), (80, 24), (120, 32), (160, 48)] {
                    let backend = TestBackend::new(width, height);
                    let mut terminal = Terminal::new(backend).unwrap();
                    let mut app = App::new(vec![signal("1"), signal("2")], true);
                    app.theme = theme;
                    app.mode = mode;
                    app.theme_menu_open = mode == Mode::Theme;
                    app.query = "launch".into();
                    app.composer = "review the plan".into();
                    terminal.draw(|frame| render(frame, &app)).unwrap();
                    assert_eq!(terminal.backend().buffer().area.width, width);
                    assert_eq!(terminal.backend().buffer().area.height, height);
                }
            }
        }
    }

    #[test]
    fn repeated_navigation_and_modal_keys_never_escape_state_bounds() {
        let mut app = App::new((0..25).map(|id| signal(&id.to_string())).collect(), true);
        for cycle in 0..5_000 {
            let key = match cycle % 12 {
                0 => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                1 => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                2 => KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                3 => KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
                4 => KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                5 => KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
                6 => KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
                7 => KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE),
                8 => KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                9 => KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
                10 => KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
                _ => KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
            };
            app.handle_key(key);
            assert!(app.selected < app.signals.len());
        }
    }

    #[test]
    fn compose_returns_a_typed_send_action() {
        let mut app = App::new(vec![signal("1")], true);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        for character in "hello".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
            Action::SendText {
                to: "team@g.us".into(),
                message: "hello".into()
            }
        );
    }

    #[test]
    fn f10_sends_while_plain_enter_and_escape_preserve_the_draft() {
        let mut app = App::new(vec![signal("1")], true);
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        for character in "keep me".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
        assert_eq!(app.composer, "keep me\n");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE)),
            Action::SendText {
                to: "team@g.us".into(),
                message: "keep me".into()
            }
        );
        assert_eq!(app.composer, "keep me\n");
    }

    #[test]
    fn approved_layout_renders_at_wide_and_compact_sizes() {
        for (width, height) in [(160, 48), (80, 24), (60, 18)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let app = App::new(vec![signal("1")], true);
            terminal.draw(|frame| render(frame, &app)).unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect::<String>();
            assert!(rendered.contains("O R B I T"));
            assert!(rendered.contains("Project Falcon"));
        }
    }

    #[test]
    fn wide_navigation_advertises_only_working_actions() {
        let backend = TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::new(vec![signal("1")], true);
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        for action in ["STREAM", "SEARCH", "COMPOSE", "THEMES", "PRIVACY"] {
            assert!(rendered.contains(action));
        }
        assert!(!rendered.contains("CONTACTS"));
        assert!(!rendered.contains("ACTIONS"));
    }

    #[test]
    fn privacy_curtain_masks_message_content() {
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(vec![signal("1")], true);
        app.privacy = true;
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(!rendered.contains("Priya"));
        assert!(!rendered.contains("Should we move"));
        assert!(rendered.contains("Hidden"));
    }

    #[test]
    fn refresh_preserves_selection_by_message_id() {
        let mut app = App::new(vec![signal("old"), signal("selected")], true);
        app.selected = 1;
        app.replace_signals(vec![signal("new"), signal("old"), signal("selected")]);
        assert_eq!(app.selected, 2);
        assert_eq!(app.selected_signal().unwrap().message_id, "selected");
    }

    #[test]
    fn failed_send_keeps_the_draft_and_success_clears_it() {
        let mut app = App::new(vec![signal("1")], true);
        app.composer = "important draft".into();
        apply_send_response(
            &mut app,
            crate::model::Response::failure("connector unavailable"),
        );
        assert_eq!(app.composer, "important draft");
        assert!(app.notice.as_deref().unwrap().contains("Send failed"));

        apply_send_response(
            &mut app,
            crate::model::Response::success(serde_json::json!({"sent": true})),
        );
        assert!(app.composer.is_empty());
        assert!(app.notice.as_deref().unwrap().contains("Sent"));
    }

    #[test]
    fn untrusted_message_control_characters_never_reach_the_terminal_buffer() {
        let mut unsafe_signal = signal("1");
        unsafe_signal.sender_name = "Priya\u{1b}[2J".into();
        unsafe_signal.text = "hello\u{7}world\nnext".into();
        let backend = TestBackend::new(120, 32);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::new(vec![unsafe_signal], true);
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{7}'));
        assert!(rendered.contains("hello world next"));
    }
}
