//! Top-level TUI application: state, key routing, and the async event loop.
//!
//! Key routing precedence (pinned in DECISIONS.md):
//! 1. An open overlay (search/palette) consumes every key.
//! 2. Request pane focused with edtui in a non-Normal mode (insert/visual/…):
//!    all keys go to edtui.
//! 3. Otherwise the crokey keymap is consulted first; unmapped keys fall
//!    through to edtui when the request pane is focused.

use std::path::Path;
use std::time::Duration;

use churl_core::persistence::{OpenWorkspace, PersistenceError};
use color_eyre::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use edtui::{EditorEventHandler, EditorMode, EditorState, Lines};
use futures::StreamExt;
use ratatui::layout::{Constraint, Layout};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::mpsc;

use super::components::explorer::{ExplorerState, SelectedEndpoint};
use super::components::{explorer, palette, picker, request, response, statusline};
use super::events::{Action, FuzzyFinder, KeyMap};

/// Which pane has focus in [`Mode::Normal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// The workspace tree on the left.
    Explorer,
    /// The request editor in the centre.
    Request,
    /// The response viewer on the right.
    Response,
}

impl Pane {
    fn next(self) -> Self {
        match self {
            Pane::Explorer => Pane::Request,
            Pane::Request => Pane::Response,
            Pane::Response => Pane::Explorer,
        }
    }

    fn prev(self) -> Self {
        self.next().next()
    }

    fn name(self) -> &'static str {
        match self {
            Pane::Explorer => "EXPLORER",
            Pane::Request => "REQUEST",
            Pane::Response => "RESPONSE",
        }
    }
}

/// Top-level input mode. Overlays own the keyboard; edtui manages its own vim
/// modes internally and is not represented here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Pane navigation and editing.
    Normal,
    /// The fuzzy endpoint search overlay is open.
    Search,
    /// The command palette overlay is open.
    Palette,
}

/// Messages delivered to the event loop over the app channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMsg {
    /// Request a redraw on the next loop iteration.
    Redraw,
}

/// The whole TUI state. Constructible without a tokio runtime so snapshot
/// tests can drive [`render`] through a `TestBackend`.
pub struct App {
    /// The workspace opened from the cwd, if its `churl.toml` exists.
    pub workspace: Option<OpenWorkspace>,
    /// Explorer tree state.
    pub explorer: ExplorerState,
    /// edtui state for the request body.
    pub editor: EditorState,
    editor_events: EditorEventHandler,
    /// The endpoint currently loaded into the request pane.
    pub selected: Option<SelectedEndpoint>,
    /// Focused pane in [`Mode::Normal`].
    pub focus: Pane,
    /// Current top-level input mode.
    pub mode: Mode,
    /// The open overlay's picker, when `mode` is Search or Palette.
    pub picker: Option<picker::PickerState>,
    search_targets: Vec<(usize, usize)>,
    palette_actions: Vec<Action>,
    keymap: KeyMap,
    finder: FuzzyFinder,
    /// Set to exit the event loop.
    pub should_quit: bool,
    /// Sender half of the app channel (cloned into background tasks from M3 on).
    pub tx: mpsc::UnboundedSender<AppMsg>,
    rx: mpsc::UnboundedReceiver<AppMsg>,
}

/// Opens the workspace at `dir`. A missing `churl.toml` yields `None` (empty
/// state); a malformed one is a hard error.
pub fn open_workspace(dir: &Path) -> Result<Option<OpenWorkspace>> {
    match OpenWorkspace::open(dir) {
        Ok(ws) => Ok(Some(ws)),
        Err(PersistenceError::Read { ref source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(err) => Err(err.into()),
    }
}

impl App {
    /// Builds the app around an optionally opened workspace and a keymap.
    pub fn new(workspace: Option<OpenWorkspace>, keymap: KeyMap) -> Result<Self> {
        let explorer = ExplorerState::new(workspace.as_ref())?;
        let (tx, rx) = mpsc::unbounded_channel();
        Ok(Self {
            workspace,
            explorer,
            editor: EditorState::default(),
            editor_events: EditorEventHandler::default(),
            selected: None,
            focus: Pane::Explorer,
            mode: Mode::Normal,
            picker: None,
            search_targets: Vec::new(),
            palette_actions: Vec::new(),
            keymap,
            finder: FuzzyFinder::new(),
            should_quit: false,
            tx,
            rx,
        })
    }

    /// Runs the event loop: `tokio::select!` over the crossterm event stream, a
    /// 250 ms tick, and the app channel. Draws once per iteration; never awaits
    /// I/O other than these three sources.
    pub async fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        let mut events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        while !self.should_quit {
            terminal.draw(|frame| render(frame, self))?;
            tokio::select! {
                maybe_event = events.next() => match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                        self.handle_key(key)?;
                    }
                    Some(Ok(_)) => {} // resize etc. — redraw happens next iteration
                    Some(Err(err)) => return Err(err.into()),
                    None => break, // input stream closed
                },
                _ = tick.tick() => {}
                msg = self.rx.recv() => match msg {
                    Some(AppMsg::Redraw) | None => {}
                },
            }
        }
        Ok(())
    }

    /// Routes one key event (see the module docs for the precedence rules).
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        match self.mode {
            Mode::Search | Mode::Palette => self.handle_overlay_key(key),
            Mode::Normal => {
                if self.focus == Pane::Request && self.editor.mode != EditorMode::Normal {
                    self.editor_events.on_key_event(key, &mut self.editor);
                    return Ok(());
                }
                if let Some(action) = self.keymap.lookup(key) {
                    self.dispatch(action, Some(key))
                } else if self.focus == Pane::Request {
                    self.editor_events.on_key_event(key, &mut self.editor);
                    Ok(())
                } else {
                    Ok(())
                }
            }
        }
    }

    /// Executes an action. `key` carries the originating key event so that
    /// navigation actions can fall through to edtui when the request pane is
    /// focused (palette-run actions have no key).
    fn dispatch(&mut self, action: Action, key: Option<KeyEvent>) -> Result<()> {
        match action {
            Action::Quit => self.should_quit = true,
            Action::FocusNext => self.focus = self.focus.next(),
            Action::FocusPrev => self.focus = self.focus.prev(),
            Action::FocusExplorer => self.focus = Pane::Explorer,
            Action::FocusRequest => self.focus = Pane::Request,
            Action::FocusResponse => self.focus = Pane::Response,
            Action::OpenSearch => self.open_search()?,
            Action::OpenPalette => self.open_palette(),
            Action::Up
            | Action::Down
            | Action::Select
            | Action::Collapse
            | Action::Expand
            | Action::Top
            | Action::Bottom => match self.focus {
                Pane::Explorer => self.explorer_action(action)?,
                Pane::Request => {
                    // Same keys are edtui's normal-mode motions: forward them.
                    if let Some(key) = key {
                        self.editor_events.on_key_event(key, &mut self.editor);
                    }
                }
                Pane::Response => {} // placeholder pane: nothing to navigate in M2
            },
        }
        Ok(())
    }

    fn explorer_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Up => self.explorer.move_up(),
            Action::Down => self.explorer.move_down(),
            Action::Top => self.explorer.top(),
            Action::Bottom => self.explorer.bottom(),
            Action::Collapse => self.explorer.collapse(),
            Action::Expand => self.explorer.expand()?,
            Action::Select => {
                if let Some(selected) = self.explorer.select()? {
                    self.load_endpoint(selected);
                }
            }
            _ => unreachable!("only navigation actions reach explorer_action"),
        }
        Ok(())
    }

    /// Loads an endpoint into the request pane: body into the edtui buffer,
    /// metadata for read-only display.
    fn load_endpoint(&mut self, selected: SelectedEndpoint) {
        let body = selected
            .endpoint
            .request
            .body
            .as_ref()
            .map(|body| body.content.as_str())
            .unwrap_or("");
        self.editor = EditorState::new(Lines::from(body));
        self.selected = Some(selected);
    }

    fn open_search(&mut self) -> Result<()> {
        let items = super::components::search::open(&mut self.explorer)?;
        self.picker = Some(items.picker);
        self.search_targets = items.targets;
        self.mode = Mode::Search;
        Ok(())
    }

    fn open_palette(&mut self) {
        let items = palette::open();
        self.picker = Some(items.picker);
        self.palette_actions = items.actions;
        self.mode = Mode::Palette;
    }

    fn close_overlay(&mut self) {
        self.picker = None;
        self.mode = Mode::Normal;
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(picker) = &mut self.picker else {
            self.close_overlay();
            return Ok(());
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.close_overlay(),
            KeyCode::Enter => return self.accept_overlay(),
            KeyCode::Up => picker.move_up(),
            KeyCode::Down => picker.move_down(),
            KeyCode::Char('p') if ctrl => picker.move_up(),
            KeyCode::Char('n') if ctrl => picker.move_down(),
            KeyCode::Backspace => picker.backspace(&mut self.finder),
            KeyCode::Char(c) if !ctrl => picker.push_char(c, &mut self.finder),
            _ => {}
        }
        Ok(())
    }

    fn accept_overlay(&mut self) -> Result<()> {
        let mode = self.mode;
        let current = self.picker.as_ref().and_then(picker::PickerState::current);
        self.close_overlay();
        let Some(index) = current else {
            return Ok(());
        };
        match mode {
            Mode::Search => {
                if let Some(&(collection, endpoint)) = self.search_targets.get(index) {
                    self.focus = Pane::Explorer;
                    if let Some(selected) = self.explorer.jump_to(collection, endpoint)? {
                        self.load_endpoint(selected);
                    }
                }
            }
            Mode::Palette => {
                if let Some(&action) = self.palette_actions.get(index) {
                    self.dispatch(action, None)?;
                }
            }
            Mode::Normal => {}
        }
        Ok(())
    }
}

/// Renders the whole UI: three panes, status bar, and any open overlay.
/// Pure (no I/O) and deterministic, so `TestBackend` snapshots stay stable.
pub fn render(frame: &mut Frame, app: &mut App) {
    let [main, status] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    let [explorer_area, request_area, response_area] = Layout::horizontal([
        Constraint::Min(24),
        Constraint::Percentage(40),
        Constraint::Percentage(35),
    ])
    .areas(main);

    let has_ws = app.workspace.is_some();
    explorer::render(
        frame,
        explorer_area,
        &app.explorer,
        app.focus == Pane::Explorer && app.mode == Mode::Normal,
        has_ws,
    );
    let selected_request = app
        .selected
        .as_ref()
        .map(|selected| &selected.endpoint.request);
    request::render(
        frame,
        request_area,
        selected_request,
        &mut app.editor,
        app.focus == Pane::Request && app.mode == Mode::Normal,
    );
    response::render(
        frame,
        response_area,
        selected_request,
        app.focus == Pane::Response && app.mode == Mode::Normal,
    );
    statusline::render(
        frame,
        status,
        app.focus.name(),
        app.workspace.as_ref().map(|ws| ws.manifest().name.as_str()),
    );

    if let Some(picker_state) = &app.picker {
        picker::render(frame, main, picker_state);
    }
}
