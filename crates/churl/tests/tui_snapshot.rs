use ratatui::Terminal;
use ratatui::backend::TestBackend;

#[test]
fn placeholder_screen_snapshot() {
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).expect("terminal init failed");

    terminal.draw(churl::tui::render).expect("draw failed");

    let buffer = terminal.backend().buffer().clone();

    // Convert the buffer to a string grid for snapshotting
    let lines: Vec<String> = (0..24)
        .map(|y| {
            (0..80)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect()
        })
        .collect();

    let screen = lines.join("\n");
    insta::assert_snapshot!(screen);
}
