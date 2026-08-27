//! Orbit's conversation-first terminal interface.
//!
//! The TUI reads only Orbit's local projection and typed daemon API. Rendering
//! never launches `wacli`, which keeps input deterministic and prevents popup
//! consoles on Windows.

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
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    config::OrbitPaths,
    ipc,
    model::{ConversationEntry, Request, Response, SignalEntry},
    store::Store,
};

const MAX_INPUT_CHARS: usize = 16_384;
const INBOX_LIMIT: u32 = 50;
const TRANSCRIPT_LIMIT: u32 = 100;
const WIDE_BREAKPOINT: u16 = 96;

/// Curated palettes keep the same semantic roles across every visual theme.
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
            // Default tokens follow the selected Command Canvas visual target.
            Self::MidnightIndigo => Palette::new([
                (8, 13, 20),
                (18, 27, 37),
                (67, 53, 105),
                (232, 235, 241),
                (143, 151, 166),
                (174, 124, 255),
                (72, 214, 132),
                (255, 184, 92),
            ]),
            Self::ArcticLight => Palette::new([
                (241, 245, 249),
                (221, 230, 239),
                (197, 202, 238),
                (20, 31, 46),
                (82, 97, 116),
                (91, 72, 196),
                (0, 130, 92),
                (181, 92, 21),
            ]),
            Self::Ember => Palette::new([
                (24, 13, 13),
                (49, 27, 26),
                (91, 45, 36),
                (255, 239, 226),
                (185, 146, 126),
                (255, 122, 82),
                (98, 210, 143),
                (255, 190, 75),
            ]),
            Self::Moss => Palette::new([
                (7, 19, 17),
                (16, 41, 34),
                (30, 70, 54),
                (231, 244, 233),
                (133, 166, 145),
                (112, 205, 150),
                (70, 199, 181),
                (238, 151, 92),
            ]),
            Self::HighContrast => Palette {
                background: Color::Black,
                surface: Color::Rgb(28, 28, 28),
                selection: Color::Blue,
                text: Color::White,
                muted: Color::Gray,
                accent: Color::Yellow,
                success: Color::Cyan,
                attention: Color::LightRed,
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
    selection: Color,
    text: Color,
    muted: Color,
    accent: Color,
    success: Color,
    attention: Color,
}

impl Palette {
    const fn new(colors: [(u8, u8, u8); 8]) -> Self {
        Self {
            background: Color::Rgb(colors[0].0, colors[0].1, colors[0].2),
            surface: Color::Rgb(colors[1].0, colors[1].1, colors[1].2),
            selection: Color::Rgb(colors[2].0, colors[2].1, colors[2].2),
            text: Color::Rgb(colors[3].0, colors[3].1, colors[3].2),
            muted: Color::Rgb(colors[4].0, colors[4].1, colors[4].2),
            accent: Color::Rgb(colors[5].0, colors[5].1, colors[5].2),
            success: Color::Rgb(colors[6].0, colors[6].1, colors[6].2),
            attention: Color::Rgb(colors[7].0, colors[7].1, colors[7].2),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Mode {
    #[default]
    Normal,
    Search,
    Compose,
    Theme,
    Help,
}

#[derive(Debug, Eq, PartialEq)]
enum Action {
    None,
    Quit,
    LoadConversation(String),
    SendText { to: String, message: String },
}

/// Pure application state. Terminal I/O and daemon requests live in `run`, so
/// interaction behavior can be tested without a WhatsApp account.
#[derive(Debug, Default)]
pub struct App {
    pub theme: ThemeId,
    conversations: Vec<ConversationEntry>,
    messages: Vec<SignalEntry>,
    selected: usize,
    active_chat_jid: String,
    mode: Mode,
    query: String,
    composer: String,
    connected: bool,
    privacy: bool,
    compact_thread: bool,
    notice: Option<String>,
}

impl App {
    #[must_use]
    pub fn new(
        mut conversations: Vec<ConversationEntry>,
        mut messages: Vec<SignalEntry>,
        connected: bool,
    ) -> Self {
        sanitize_conversations(&mut conversations);
        sanitize_messages(&mut messages);
        let active_chat_jid = conversations
            .first()
            .map(|item| item.chat_jid.clone())
            .unwrap_or_default();
        Self {
            conversations,
            messages,
            active_chat_jid,
            connected,
            ..Self::default()
        }
    }

    fn selected_conversation(&self) -> Option<&ConversationEntry> {
        self.conversations.get(self.selected)
    }

    fn active_conversation(&self) -> Option<&ConversationEntry> {
        self.conversations
            .iter()
            .find(|item| item.chat_jid == self.active_chat_jid)
            .or_else(|| self.selected_conversation())
    }

    fn visible_indices(&self) -> Vec<usize> {
        let query = self.query.trim().to_lowercase();
        self.conversations
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                (query.is_empty()
                    || item.chat_name.to_lowercase().contains(&query)
                    || item.preview.to_lowercase().contains(&query)
                    || item.last_sender_name.to_lowercase().contains(&query))
                .then_some(index)
            })
            .collect()
    }

    fn ensure_visible_selection(&mut self) {
        let visible = self.visible_indices();
        if !visible.contains(&self.selected) {
            self.selected = visible.first().copied().unwrap_or(0);
        }
    }

    fn move_selection(&mut self, amount: isize) -> Action {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return Action::None;
        }
        let position = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let next = position
            .saturating_add_signed(amount)
            .min(visible.len().saturating_sub(1));
        self.selected = visible[next];
        self.load_selected()
    }

    fn jump_to(&mut self, digit: char) -> Action {
        let visible = self.visible_indices();
        let requested = if digit == '0' {
            9
        } else {
            digit.to_digit(10).unwrap_or(1).saturating_sub(1) as usize
        };
        if let Some(index) = visible.get(requested) {
            self.selected = *index;
            self.load_selected()
        } else {
            self.notice = Some(format!("Conversation {} is not visible", requested + 1));
            Action::None
        }
    }

    fn load_selected(&mut self) -> Action {
        let Some(chat_jid) = self
            .selected_conversation()
            .map(|item| item.chat_jid.clone())
        else {
            self.notice = Some("No conversations match this search".into());
            return Action::None;
        };
        self.active_chat_jid.clone_from(&chat_jid);
        Action::LoadConversation(chat_jid)
    }

    fn replace_conversations(&mut self, mut conversations: Vec<ConversationEntry>) {
        let selected_jid = self
            .selected_conversation()
            .map(|item| item.chat_jid.clone());
        sanitize_conversations(&mut conversations);
        self.conversations = conversations;
        self.selected = selected_jid
            .and_then(|jid| {
                self.conversations
                    .iter()
                    .position(|item| item.chat_jid == jid)
            })
            .unwrap_or_else(|| {
                self.selected
                    .min(self.conversations.len().saturating_sub(1))
            });
        self.ensure_visible_selection();
    }

    fn replace_messages(&mut self, mut messages: Vec<SignalEntry>) {
        sanitize_messages(&mut messages);
        self.messages = messages;
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        match self.mode {
            Mode::Search => self.handle_search_key(key),
            Mode::Compose => self.handle_compose_key(key),
            Mode::Theme => self.handle_theme_key(key),
            Mode::Help => {
                if matches!(key.code, KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?')) {
                    self.mode = Mode::Normal;
                }
                Action::None
            }
            Mode::Normal => self.handle_normal_key(key),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::PageDown => self.move_selection(8),
            KeyCode::PageUp => self.move_selection(-8),
            KeyCode::Home => {
                if let Some(index) = self.visible_indices().first().copied() {
                    self.selected = index;
                }
                self.load_selected()
            }
            KeyCode::End => {
                if let Some(index) = self.visible_indices().last().copied() {
                    self.selected = index;
                }
                self.load_selected()
            }
            KeyCode::Char(digit @ '0'..='9') => self.jump_to(digit),
            KeyCode::Char('/') | KeyCode::F(2) => {
                self.mode = Mode::Search;
                Action::None
            }
            KeyCode::Char('c') | KeyCode::F(3) => self.open_composer(),
            KeyCode::Enter => {
                self.compact_thread = true;
                self.load_selected()
            }
            KeyCode::Esc => {
                self.compact_thread = false;
                Action::None
            }
            KeyCode::Char('t') | KeyCode::F(6) => {
                self.mode = Mode::Theme;
                Action::None
            }
            KeyCode::Char('?') | KeyCode::F(1) => {
                self.mode = Mode::Help;
                Action::None
            }
            KeyCode::Char('p') => {
                self.privacy = !self.privacy;
                Action::None
            }
            KeyCode::Char('v') => {
                self.notice =
                    Some("Voice beta is not paired; messaging remains fully available".into());
                Action::None
            }
            _ => Action::None,
        }
    }

    fn open_composer(&mut self) -> Action {
        if self.active_conversation().is_some() {
            self.mode = Mode::Compose;
        } else {
            self.notice = Some("Select a conversation before composing".into());
        }
        Action::None
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::F(2) => {
                self.query.clear();
                self.ensure_visible_selection();
                self.mode = Mode::Normal;
                Action::None
            }
            KeyCode::Enter => {
                self.ensure_visible_selection();
                self.mode = Mode::Normal;
                self.compact_thread = true;
                self.load_selected()
            }
            KeyCode::Down => self.move_selection(1),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Backspace => {
                self.query.pop();
                self.ensure_visible_selection();
                Action::None
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                append_terminal_input(&mut self.query, &character.to_string(), false);
                self.ensure_visible_selection();
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
            KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.compose_send_action()
            }
            KeyCode::Enter => {
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
        let Some(to) = self.active_conversation().map(|item| item.chat_jid.clone()) else {
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

    fn handle_theme_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('t') | KeyCode::F(6) => self.mode = Mode::Normal,
            KeyCode::Char(character) => {
                let theme = match character {
                    '1' => Some(ThemeId::MidnightIndigo),
                    '2' => Some(ThemeId::ArcticLight),
                    '3' => Some(ThemeId::Ember),
                    '4' => Some(ThemeId::Moss),
                    '5' => Some(ThemeId::HighContrast),
                    _ => None,
                };
                if let Some(theme) = theme {
                    self.theme = theme;
                    self.mode = Mode::Normal;
                }
            }
            _ => {}
        }
        Action::None
    }

    fn handle_paste(&mut self, text: &str) {
        match self.mode {
            Mode::Search => {
                append_terminal_input(&mut self.query, text, false);
                self.ensure_visible_selection();
            }
            Mode::Compose => append_terminal_input(&mut self.composer, text, true),
            Mode::Normal | Mode::Theme | Mode::Help => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent, area: Rect) -> Action {
        match mouse.kind {
            MouseEventKind::ScrollDown => self.move_selection(2),
            MouseEventKind::ScrollUp => self.move_selection(-2),
            MouseEventKind::Down(MouseButton::Left) => {
                if mouse.row == 0 && mouse.column >= area.width.saturating_sub(30) {
                    self.notice = Some("Voice beta is not paired".into());
                    return Action::None;
                }
                if mouse.row >= area.height.saturating_sub(8) && mouse.column > area.width / 3 {
                    return self.open_composer();
                }
                if area.width >= WIDE_BREAKPOINT && mouse.column < area.width / 3 {
                    if mouse.row <= 4 {
                        self.mode = Mode::Search;
                        return Action::None;
                    }
                    let visible_position = usize::from(mouse.row.saturating_sub(5) / 3);
                    if let Some(index) = self.visible_indices().get(visible_position).copied() {
                        self.selected = index;
                        return self.load_selected();
                    }
                }
                Action::None
            }
            _ => Action::None,
        }
    }
}

/// Enter the alternate-screen TUI. Demo mode uses fictional in-memory data,
/// making screenshots and compatibility checks privacy-safe.
pub async fn run(paths: &OrbitPaths, demo: bool) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("`orbit ui` requires an interactive terminal; use commands with --json for scripts");
    }
    let store = Store::new(paths.database.clone());
    let (conversations, messages, connected) = if demo {
        demo_data()
    } else {
        let conversations = store.conversation_list(INBOX_LIMIT)?;
        let messages = conversations.first().map_or_else(
            || Ok(Vec::new()),
            |item| store.conversation_messages(&item.chat_jid, TRANSCRIPT_LIMIT),
        )?;
        let connected = ipc::request(&paths.ipc_name(), &Request::Ping)
            .await
            .is_ok();
        (conversations, messages, connected)
    };
    let mut app = App::new(conversations, messages, connected);
    app.theme = load_theme(paths);
    let mut terminal = TerminalGuard::enter()?;
    let mut last_refresh = Instant::now();
    loop {
        terminal.draw(|frame| render(frame, &app))?;
        if event::poll(Duration::from_millis(200)).context("poll terminal input")? {
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
                Action::LoadConversation(chat_jid) => {
                    if !demo {
                        match store.conversation_messages(&chat_jid, TRANSCRIPT_LIMIT) {
                            Ok(messages) => app.replace_messages(messages),
                            Err(error) => {
                                app.notice =
                                    Some(format!("Could not load conversation: {error:#}"));
                            }
                        }
                    }
                }
                Action::SendText { to, message } => {
                    if demo {
                        app.notice = Some("Demo mode never sends messages".into());
                        continue;
                    }
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
        if !demo
            && last_refresh.elapsed() >= Duration::from_secs(2)
            && matches!(app.mode, Mode::Normal)
            && app.query.is_empty()
        {
            if let Ok(conversations) = store.conversation_list(INBOX_LIMIT) {
                app.replace_conversations(conversations);
            }
            if !app.active_chat_jid.is_empty()
                && let Ok(messages) =
                    store.conversation_messages(&app.active_chat_jid, TRANSCRIPT_LIMIT)
            {
                app.replace_messages(messages);
            }
            app.connected = ipc::request(&paths.ipc_name(), &Request::Ping)
                .await
                .is_ok();
            last_refresh = Instant::now();
        }
    }
    Ok(())
}

fn apply_send_response(app: &mut App, response: Response) {
    if response.ok {
        app.composer.clear();
        app.notice = Some(response.warning.map_or_else(
            || "Sent — accepted by the connector".into(),
            |warning| format!("Possibly sent — {warning}"),
        ));
    } else {
        app.notice = Some(format!(
            "Send failed — {}. Draft retained.",
            response.error.unwrap_or_else(|| "unknown error".into())
        ));
    }
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
            Constraint::Length(2),
            Constraint::Min(14),
            Constraint::Length(2),
        ])
        .split(area);
    render_header(frame, rows[0], app, palette);
    if area.width >= WIDE_BREAKPOINT {
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(32), Constraint::Percentage(68)])
            .split(rows[1]);
        render_inbox(frame, columns[0], app, palette);
        render_thread(frame, columns[1], app, palette);
    } else if app.compact_thread || matches!(app.mode, Mode::Compose) {
        render_thread(frame, rows[1], app, palette);
    } else {
        render_inbox(frame, rows[1], app, palette);
    }
    render_footer(frame, rows[2], app, palette);
    match app.mode {
        Mode::Theme => render_theme_menu(frame, area, app, palette),
        Mode::Help => render_help(frame, area, palette),
        Mode::Normal | Mode::Search | Mode::Compose => {}
    }
}

fn render_header(frame: &mut Frame<'_>, area: Rect, _app: &App, palette: Palette) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(30), Constraint::Length(34)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " ORBIT ",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " WhatsApp TUI for Windows Terminal & CachyOS",
                Style::default().fg(palette.muted),
            ),
        ]))
        .style(Style::default().bg(palette.surface)),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new("[V] Voice beta: not paired ")
            .style(Style::default().fg(palette.muted).bg(palette.surface))
            .alignment(Alignment::Right),
        chunks[1],
    );
}

fn render_inbox(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(6),
            Constraint::Length(2),
        ])
        .split(area);
    let search_active = matches!(app.mode, Mode::Search);
    let search_value = if app.query.is_empty() {
        "Search conversations"
    } else {
        &app.query
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" [/] ", Style::default().fg(palette.accent)),
            Span::styled(
                search_value,
                Style::default().fg(if search_active {
                    palette.text
                } else {
                    palette.muted
                }),
            ),
            Span::styled(
                if search_active { " ▌" } else { "" },
                Style::default().fg(palette.accent),
            ),
        ]))
        .block(
            Block::default()
                .borders(Borders::BOTTOM | Borders::RIGHT)
                .border_style(Style::default().fg(if search_active {
                    palette.accent
                } else {
                    palette.surface
                })),
        )
        .style(Style::default().bg(palette.background)),
        parts[0],
    );

    let visible = app.visible_indices();
    let items = visible
        .iter()
        .enumerate()
        .map(|(position, index)| {
            let item = &app.conversations[*index];
            let name = if app.privacy {
                "Hidden conversation"
            } else {
                display_or(&item.chat_name, &item.chat_jid)
            };
            let preview = if app.privacy {
                "••••••••••••••••".to_owned()
            } else {
                let prefix = if item.from_me {
                    "You: "
                } else if item.last_sender_name.is_empty()
                    || item.last_sender_name == item.chat_name
                {
                    ""
                } else {
                    "Reply: "
                };
                format!("{prefix}{}", item.preview.replace('\n', " "))
            };
            let time = compact_date_time(&item.last_timestamp);
            let width = usize::from(area.width.saturating_sub(9));
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(
                        format!(" {:>2} ", (position + 1) % 10),
                        Style::default().fg(palette.accent),
                    ),
                    Span::styled(
                        truncate(name, width.saturating_sub(8)),
                        Style::default()
                            .fg(palette.text)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {time}"), Style::default().fg(palette.muted)),
                ]),
                Line::from(Span::styled(
                    format!("     {}", truncate(&preview, width)),
                    Style::default().fg(palette.muted),
                )),
                Line::raw(""),
            ])
        })
        .collect::<Vec<_>>();
    let selected_position = visible.iter().position(|index| *index == app.selected);
    let mut state = ListState::default().with_selected(selected_position);
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::default()
                    .title(format!(" CONVERSATIONS  {} ", visible.len()))
                    .borders(Borders::RIGHT)
                    .border_style(Style::default().fg(palette.surface)),
            )
            .highlight_symbol("›")
            .highlight_style(
                Style::default()
                    .bg(palette.selection)
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD),
            ),
        parts[1],
        &mut state,
    );
    frame.render_widget(
        Paragraph::new(" ↑/↓ Navigate   Enter Open   / Search ")
            .style(Style::default().fg(palette.accent).bg(palette.background))
            .block(
                Block::default()
                    .borders(Borders::TOP | Borders::RIGHT)
                    .border_style(Style::default().fg(palette.surface)),
            ),
        parts[2],
    );
}

fn render_thread(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let composer_height = if area.height >= 28 { 8 } else { 6 };
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(7),
            Constraint::Length(composer_height),
        ])
        .split(area);
    render_thread_header(frame, parts[0], app, palette);
    render_transcript(frame, parts[1], app, palette);
    render_composer(frame, parts[2], app, palette);
}

fn render_thread_header(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let name = app.active_conversation().map_or("No conversation", |item| {
        if app.privacy {
            "Hidden conversation"
        } else {
            display_or(&item.chat_name, &item.chat_jid)
        }
    });
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(30)])
        .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {name}"),
                Style::default()
                    .fg(palette.text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if app.connected {
                    "  • online"
                } else {
                    "  • local history"
                },
                Style::default().fg(if app.connected {
                    palette.success
                } else {
                    palette.muted
                }),
            ),
        ]))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(palette.accent)),
        ),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new("Messages stored locally ")
            .style(Style::default().fg(palette.muted))
            .alignment(Alignment::Right)
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(palette.accent)),
            ),
        chunks[1],
    );
}

fn render_transcript(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    if app.messages.is_empty() {
        frame.render_widget(
            Paragraph::new(
                "\n No messages in this conversation yet.\n Press F3 to write the first one.",
            )
            .style(Style::default().fg(palette.muted))
            .alignment(Alignment::Center),
            area,
        );
        return;
    }
    let capacity = usize::from(area.height.saturating_sub(2)) / 3;
    let start = app.messages.len().saturating_sub(capacity.max(1));
    let date = app
        .messages
        .get(start)
        .map_or("Recent messages", |message| date_only(&message.timestamp));
    let today = chrono::Local::now().date_naive().to_string();
    let date_label = if date == today {
        format!("{date} (Today)")
    } else {
        date.to_owned()
    };
    let mut lines = vec![
        Line::from(Span::styled(
            format!("────────  {date_label}  ────────"),
            Style::default().fg(palette.accent),
        ))
        .alignment(Alignment::Center),
    ];
    for message in &app.messages[start..] {
        let sender = if app.privacy {
            "Hidden"
        } else if message.from_me {
            "You"
        } else {
            display_or(&message.sender_name, &message.chat_name)
        };
        let body = if app.privacy {
            "••••••••••••••••".to_owned()
        } else {
            message_body(message)
        };
        let sender_color = if message.from_me {
            palette.accent
        } else {
            palette.success
        };
        let time = compact_time(&message.timestamp);
        let padding = usize::from(area.width.saturating_sub(3))
            .saturating_sub(sender.chars().count() + time.chars().count());
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {sender}"),
                Style::default()
                    .fg(sender_color)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ".repeat(padding), Style::default().fg(palette.muted)),
            Span::styled(time, Style::default().fg(palette.muted)),
        ]));
        lines.push(Line::from(Span::styled(
            format!(
                " {}",
                truncate(
                    &body.replace('\n', " ↵ "),
                    usize::from(area.width.saturating_sub(8))
                )
            ),
            Style::default().fg(palette.text),
        )));
        lines.push(Line::raw(""));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::default().bg(palette.background))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let focused = matches!(app.mode, Mode::Compose);
    let recipient = app.active_conversation().map_or("No recipient", |item| {
        if app.privacy {
            "Hidden conversation"
        } else {
            display_or(&item.chat_name, &item.chat_jid)
        }
    });
    let border = if focused {
        palette.accent
    } else {
        palette.surface
    };
    let text = if app.composer.is_empty() {
        if focused {
            "▌ Type your message…"
        } else {
            "Press F3 or c to compose"
        }
    } else {
        &app.composer
    };
    let inner = area.inner(Margin {
        horizontal: 1,
        vertical: 1,
    });
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);
    frame.render_widget(
        Block::default()
            .title(Line::from(vec![
                Span::styled(" To: ", Style::default().fg(palette.muted)),
                Span::styled(
                    recipient,
                    Style::default()
                        .fg(palette.success)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    if focused { "  • composing " } else { " " },
                    Style::default().fg(palette.accent),
                ),
            ]))
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .style(Style::default().bg(palette.background)),
        area,
    );
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(if app.composer.is_empty() {
                palette.muted
            } else {
                palette.text
            }))
            .wrap(Wrap { trim: false }),
        rows[0],
    );
    let hints = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(20),
            Constraint::Min(16),
            Constraint::Length(14),
        ])
        .split(rows[1]);
    frame.render_widget(
        Paragraph::new(format!(
            "{} / {MAX_INPUT_CHARS}",
            app.composer.chars().count()
        ))
        .style(Style::default().fg(palette.muted)),
        hints[0],
    );
    frame.render_widget(
        Paragraph::new("Enter = newline")
            .style(Style::default().fg(palette.muted))
            .alignment(Alignment::Center),
        hints[1],
    );
    frame.render_widget(
        Paragraph::new(" F10 Send ")
            .style(
                Style::default()
                    .fg(if focused {
                        palette.background
                    } else {
                        palette.muted
                    })
                    .bg(if focused {
                        palette.accent
                    } else {
                        palette.surface
                    })
                    .add_modifier(Modifier::BOLD),
            )
            .alignment(Alignment::Right),
        hints[2],
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(48), Constraint::Length(34)])
        .split(area);
    let message = app
        .notice
        .as_deref()
        .unwrap_or("F1 Help  F2 Search  F3 Compose  F6 Themes  P Privacy  Q Quit");
    frame.render_widget(
        Paragraph::new(format!(" {message}"))
            .style(Style::default().fg(if app.notice.is_some() {
                palette.attention
            } else {
                palette.muted
            }))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(palette.surface)),
            ),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(if app.connected {
            "Sync: live  •  Connected "
        } else {
            "Sync: paused  •  Local history "
        })
        .style(Style::default().fg(if app.connected {
            palette.success
        } else {
            palette.muted
        }))
        .alignment(Alignment::Right)
        .block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(palette.surface)),
        ),
        chunks[1],
    );
}

fn render_theme_menu(frame: &mut Frame<'_>, area: Rect, app: &App, palette: Palette) {
    let popup = centered_rect(42, 10, area);
    let mut lines = Vec::new();
    for (index, (id, name)) in ThemeId::ALL.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {}  ", index + 1),
                Style::default().fg(palette.accent),
            ),
            Span::styled(*name, Style::default().fg(palette.text)),
            Span::styled(
                if *id == app.theme { "  selected" } else { "" },
                Style::default().fg(palette.success),
            ),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "Esc closes without changing",
        Style::default().fg(palette.muted),
    ));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" THEME ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(palette.accent))
                .style(Style::default().bg(palette.surface)),
        ),
        popup,
    );
}

fn render_help(frame: &mut Frame<'_>, area: Rect, palette: Palette) {
    let popup = centered_rect(58, 16, area);
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(
                "CHAT WORKFLOW",
                Style::default()
                    .fg(palette.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::raw("↑/↓ or j/k     Choose a conversation"),
            Line::raw("Enter          Open the selected conversation"),
            Line::raw("/ or F2        Search conversations"),
            Line::raw("c or F3        Focus the message composer"),
            Line::raw("Enter          New line while composing"),
            Line::raw("F10 or Ctrl+S  Send; uncertain sends are not retried"),
            Line::raw("v              Explain voice beta readiness"),
            Line::raw("p              Toggle privacy masking"),
            Line::raw("F6             Change theme"),
            Line::raw("q / Ctrl+C     Exit; background sync continues"),
            Line::raw(""),
            Line::styled("Esc or F1 closes help", Style::default().fg(palette.muted)),
        ])
        .style(Style::default().fg(palette.text).bg(palette.surface))
        .block(
            Block::default()
                .title(" ORBIT HELP ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(palette.accent)),
        ),
        popup,
    );
}

fn render_minimum_size(frame: &mut Frame<'_>, area: Rect, palette: Palette) {
    frame.render_widget(Paragraph::new("ORBIT\n\nTerminal is too small.\nResize to at least 60×18.\n\nCLI commands remain available.")
        .style(Style::default().fg(palette.text).bg(palette.background)).alignment(Alignment::Center), area);
}

fn demo_data() -> (Vec<ConversationEntry>, Vec<SignalEntry>, bool) {
    let conversations = vec![
        demo_conversation(
            "alex",
            "Alex Morgan",
            "Sounds good. I'll review and reply.",
            "2026-08-28T10:24:00Z",
            false,
            14,
        ),
        demo_conversation(
            "aurora",
            "Project Aurora",
            "Build is green on all jobs.",
            "2026-08-28T09:41:00Z",
            false,
            28,
        ),
        demo_conversation(
            "family",
            "Family Chat",
            "Don't forget dinner at 7!",
            "2026-08-28T08:33:00Z",
            false,
            42,
        ),
        demo_conversation(
            "sam",
            "Sam Lee",
            "Can you send the report?",
            "2026-08-28T07:58:00Z",
            false,
            9,
        ),
        demo_conversation(
            "design",
            "Design Sync",
            "Ack, I'll push the mockups.",
            "2026-08-28T07:12:00Z",
            true,
            31,
        ),
        demo_conversation(
            "jamie",
            "Jamie Patel",
            "Thanks for the quick turnaround.",
            "2026-08-27T17:20:00Z",
            false,
            7,
        ),
        demo_conversation(
            "ops",
            "Dev Ops",
            "Deployed to staging successfully.",
            "2026-08-27T15:05:00Z",
            false,
            18,
        ),
        demo_conversation(
            "books",
            "Book Club",
            "The chapter was wild.",
            "2026-08-27T13:00:00Z",
            false,
            12,
        ),
    ];
    let transcript = vec![
        demo_message(
            "1",
            false,
            "Alex Morgan",
            "Hey! How's the proposal coming along?",
            "09:15",
        ),
        demo_message(
            "2",
            true,
            "You",
            "It's going well. I'll have a draft ready by noon.",
            "09:16",
        ),
        demo_message(
            "3",
            false,
            "Alex Morgan",
            "Can you include the new timeline we discussed?",
            "09:17",
        ),
        demo_message(
            "4",
            true,
            "You",
            "Absolutely. I'll add the milestones and share here.",
            "09:18",
        ),
        demo_message(
            "5",
            false,
            "Alex Morgan",
            "Perfect. Let's highlight the risk mitigation section.",
            "09:19",
        ),
        demo_message(
            "6",
            true,
            "You",
            "Noted. I'll refine and send version two shortly.",
            "10:12",
        ),
        demo_message(
            "7",
            false,
            "Alex Morgan",
            "Sounds good. I'll review and reply.",
            "10:24",
        ),
    ];
    (conversations, transcript, true)
}

fn demo_conversation(
    id: &str,
    name: &str,
    preview: &str,
    timestamp: &str,
    from_me: bool,
    message_count: u64,
) -> ConversationEntry {
    ConversationEntry {
        chat_jid: format!("{id}@example.invalid"),
        chat_name: name.into(),
        last_message_id: format!("{id}-last"),
        last_timestamp: timestamp.into(),
        last_sender_name: name.into(),
        preview: preview.into(),
        from_me,
        message_count,
    }
}

fn demo_message(id: &str, from_me: bool, sender: &str, text: &str, time: &str) -> SignalEntry {
    SignalEntry {
        message_id: id.into(),
        chat_jid: "alex@example.invalid".into(),
        chat_name: "Alex Morgan".into(),
        sender_name: sender.into(),
        timestamp: format!("2026-08-28T{time}:00Z"),
        text: text.into(),
        content_kind: "text".into(),
        filename: String::new(),
        from_me,
        edited: false,
        revoked: false,
    }
}

fn sanitize_conversations(conversations: &mut [ConversationEntry]) {
    for item in conversations {
        for field in [
            &mut item.chat_jid,
            &mut item.chat_name,
            &mut item.last_message_id,
            &mut item.last_timestamp,
            &mut item.last_sender_name,
            &mut item.preview,
        ] {
            *field = sanitize_terminal_text(field);
        }
    }
}

fn sanitize_messages(messages: &mut [SignalEntry]) {
    for item in messages {
        for field in [
            &mut item.message_id,
            &mut item.chat_jid,
            &mut item.chat_name,
            &mut item.sender_name,
            &mut item.timestamp,
            &mut item.text,
            &mut item.content_kind,
            &mut item.filename,
        ] {
            *field = sanitize_terminal_text(field);
        }
    }
}

fn sanitize_terminal_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn append_terminal_input(target: &mut String, text: &str, allow_newline: bool) {
    let remaining = MAX_INPUT_CHARS.saturating_sub(target.chars().count());
    target.extend(
        text.chars()
            .map(|character| match character {
                '\r' | '\n' if allow_newline => '\n',
                '\r' | '\n' => ' ',
                _ if character.is_control() => ' ',
                _ => character,
            })
            .take(remaining),
    );
}

fn message_body(message: &SignalEntry) -> String {
    if message.revoked {
        "Message revoked".into()
    } else if !message.filename.is_empty() {
        format!("[FILE] {}", message.filename)
    } else if !message.text.is_empty() {
        message.text.clone()
    } else if message.content_kind.is_empty() {
        "Message".into()
    } else {
        format!("[{}]", message.content_kind.to_uppercase())
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
fn compact_date_time(timestamp: &str) -> String {
    let today = chrono::Local::now().date_naive().to_string();
    if timestamp.starts_with(&today) {
        compact_time(timestamp).to_owned()
    } else {
        timestamp.get(..10).unwrap_or(timestamp).to_owned()
    }
}
fn date_only(timestamp: &str) -> &str {
    timestamp.get(..10).unwrap_or(timestamp)
}
fn truncate(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if value.chars().count() <= width {
        return value.to_owned();
    }
    if width == 1 {
        return "…".into();
    }
    value.chars().take(width - 1).chain(['…']).collect()
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
    paths.create()?;
    std::fs::write(theme_path(paths), theme.key()).context("save TUI theme")
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

    fn app() -> App {
        let (conversations, messages, connected) = demo_data();
        App::new(conversations, messages, connected)
    }
    fn rendered(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn selected_visual_contract_is_visible_at_reference_size() {
        let output = rendered(&app(), 120, 40);
        for expected in [
            "ORBIT",
            "Search conversations",
            "CONVERSATIONS",
            "Alex Morgan",
            "Voice beta: not paired",
            "F10 Send",
            "Connected",
        ] {
            assert!(output.contains(expected), "missing {expected}");
        }
        assert!(!output.contains("SIGNAL STREAM"));
        assert!(!output.contains("EVENT DETAILS"));
    }

    #[test]
    fn moving_selection_requests_the_real_conversation() {
        let mut app = app();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Action::LoadConversation("aurora@example.invalid".into())
        );
        assert_eq!(app.selected, 1);
    }

    #[test]
    fn search_filters_the_inbox_and_enter_opens_the_result() {
        let mut app = app();
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for character in "family".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(app.visible_indices(), vec![2]);
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::LoadConversation("family@example.invalid".into())
        );
    }

    #[test]
    fn f10_sends_and_plain_enter_adds_a_newline() {
        let mut app = app();
        app.handle_key(KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE));
        for character in "hello".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
        assert_eq!(app.composer, "hello\n");
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::F(10), KeyModifiers::NONE)),
            Action::SendText {
                to: "alex@example.invalid".into(),
                message: "hello".into()
            }
        );
    }

    #[test]
    fn failed_send_retains_the_draft_and_success_clears_it() {
        let mut app = app();
        app.composer = "important draft".into();
        apply_send_response(&mut app, Response::failure("connector unavailable"));
        assert_eq!(app.composer, "important draft");
        assert!(app.notice.as_deref().unwrap().contains("Draft retained"));
        apply_send_response(
            &mut app,
            Response::success(serde_json::json!({"sent": true})),
        );
        assert!(app.composer.is_empty());
    }

    #[test]
    fn voice_beta_is_honest_and_does_not_start_a_call() {
        let mut app = app();
        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
            Action::None
        );
        assert!(app.notice.as_deref().unwrap().contains("not paired"));
    }

    #[test]
    fn privacy_masks_names_and_message_bodies() {
        let mut app = app();
        app.privacy = true;
        let output = rendered(&app, 120, 40);
        assert!(!output.contains("Alex Morgan"));
        assert!(!output.contains("proposal"));
        assert!(output.contains("Hidden conversation"));
    }

    #[test]
    fn paste_is_bounded_and_control_characters_are_sanitized() {
        let mut app = app();
        app.mode = Mode::Compose;
        app.handle_paste("line one\nline two\u{1b}[2J");
        assert_eq!(app.composer, "line one\nline two [2J");
        app.composer.clear();
        app.handle_paste(&"x".repeat(MAX_INPUT_CHARS + 100));
        assert_eq!(app.composer.chars().count(), MAX_INPUT_CHARS);
    }

    #[test]
    fn mouse_reaches_search_inbox_and_composer() {
        let mut app = app();
        let area = Rect::new(0, 0, 120, 40);
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 8,
                row: 3,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert_eq!(app.mode, Mode::Search);
        app.mode = Mode::Normal;
        let action = app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 8,
                row: 8,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert!(matches!(action, Action::LoadConversation(_)));
        app.handle_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 80,
                row: 35,
                modifiers: KeyModifiers::NONE,
            },
            area,
        );
        assert_eq!(app.mode, Mode::Compose);
    }

    #[test]
    fn all_themes_modes_and_supported_sizes_render() {
        for (theme, _) in ThemeId::ALL {
            for mode in [
                Mode::Normal,
                Mode::Search,
                Mode::Compose,
                Mode::Theme,
                Mode::Help,
            ] {
                for (width, height) in [(60, 18), (80, 24), (96, 28), (120, 40), (160, 48)] {
                    let mut app = app();
                    app.theme = theme;
                    app.mode = mode;
                    app.compact_thread = mode == Mode::Compose;
                    assert!(rendered(&app, width, height).contains("ORBIT"));
                }
            }
        }
    }

    #[test]
    fn repeated_navigation_never_escapes_the_inbox() {
        let mut app = app();
        for cycle in 0..5_000 {
            let key = match cycle % 8 {
                0 => KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
                1 => KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
                2 => KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                3 => KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
                4 => KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                5 => KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
                6 => KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
                _ => KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE),
            };
            app.handle_key(key);
            assert!(app.selected < app.conversations.len());
        }
    }
}
