use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Paragraph},
};

/// Initialise the terminal (raw mode + alternate screen, via `ratatui::init`).
pub fn init() -> DefaultTerminal {
    ratatui::init()
}

/// Restore the terminal to its original state. Safe to call multiple times.
pub fn restore() {
    ratatui::restore();
}

/// Run the placeholder TUI event loop.
pub fn run() -> Result<()> {
    let mut terminal = init();
    let result = event_loop(&mut terminal);
    restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal) -> Result<()> {
    loop {
        terminal.draw(render)?;

        if let Event::Key(key) = event::read()?
            && should_quit(key)
        {
            break;
        }
    }
    Ok(())
}

fn should_quit(key: KeyEvent) -> bool {
    matches!(
        key,
        KeyEvent {
            code: KeyCode::Char('q'),
            ..
        } | KeyEvent {
            code: KeyCode::Esc,
            ..
        } | KeyEvent {
            code: KeyCode::Char('c'),
            modifiers: KeyModifiers::CONTROL,
            ..
        }
    )
}

/// Render the placeholder welcome screen.
pub fn render(frame: &mut Frame) {
    let version = churl_core::VERSION;
    let area = frame.area();

    // Outer block
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Line::from(vec![Span::styled(
            " churl ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )]))
        .title_alignment(Alignment::Center);

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Split inner area: centre for the welcome text, bottom strip for version
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(3),
            Constraint::Fill(1),
            Constraint::Length(1),
        ])
        .split(inner);

    // Centre block: title + subtitle
    let welcome = Paragraph::new(vec![
        Line::from(Span::styled(
            "churl",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "press q to quit",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .alignment(Alignment::Center);
    frame.render_widget(welcome, chunks[1]);

    // Bottom strip: version, right-aligned
    let version_line = Paragraph::new(Span::styled(version, Style::default().fg(Color::DarkGray)))
        .alignment(Alignment::Right);
    frame.render_widget(version_line, chunks[3]);
}
