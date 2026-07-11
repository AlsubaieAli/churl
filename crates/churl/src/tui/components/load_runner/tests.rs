use super::*;
use churl_core::load::ReqOutcome;

fn runner() -> LoadRunnerState {
    LoadRunnerState::new(
        "List users".to_owned(),
        "https://api.test/users".to_owned(),
        Some("users/list.toml".to_owned()),
        LoadConfig {
            total: 10,
            concurrency: 5,
            interval: Duration::ZERO,
        },
    )
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ch(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

#[test]
fn tab_cycles_focus() {
    let mut r = runner();
    assert_eq!(r.focus, RunnerFocus::ConfigHeader);
    r.handle_key(key(KeyCode::Tab));
    assert_eq!(r.focus, RunnerFocus::Results);
    r.handle_key(key(KeyCode::Tab));
    assert_eq!(r.focus, RunnerFocus::Response);
    r.handle_key(key(KeyCode::Tab));
    assert_eq!(r.focus, RunnerFocus::ConfigHeader);
}

#[test]
fn backtab_cycles_focus_reverse() {
    let mut r = runner();
    assert_eq!(r.focus, RunnerFocus::ConfigHeader);
    r.handle_key(key(KeyCode::BackTab));
    assert_eq!(r.focus, RunnerFocus::Response);
    r.handle_key(key(KeyCode::BackTab));
    assert_eq!(r.focus, RunnerFocus::Results);
    r.handle_key(key(KeyCode::BackTab));
    assert_eq!(r.focus, RunnerFocus::ConfigHeader);
}

/// The derived req/s annotation reads honestly across gaps (review
/// finding #1): sub-1-req/s rates must not truncate to `≈0 req/s`.
#[test]
fn derived_rate_reads_honestly_across_gaps() {
    let theme = Theme::default();
    let text = |r: &LoadRunnerState| -> String {
        config_spans(r, &theme)
            .into_iter()
            .map(|s| s.content.into_owned())
            .collect()
    };
    let mut r = runner();
    r.cfg.interval = Duration::from_millis(0);
    assert!(text(&r).contains("min gap=0ms · max rate"));
    r.cfg.interval = Duration::from_millis(500);
    assert!(text(&r).contains("≈2.0 req/s"), "got: {}", text(&r));
    r.cfg.interval = Duration::from_millis(1);
    assert!(text(&r).contains("≈1000 req/s"), "got: {}", text(&r));
    // gaps ≥ 1s were reading as ≈0 req/s under integer division.
    r.cfg.interval = Duration::from_millis(2000);
    let t = text(&r);
    assert!(t.contains("≈0.50 req/s"), "got: {t}");
    assert!(!t.contains("≈0 req/s"), "sub-1 rate truncated to zero: {t}");
}

#[test]
fn config_field_nav_is_h_l() {
    let mut r = runner();
    assert_eq!(r.field, LoadField::Total);
    // l / Right advance; h / Left go back (fields are a horizontal row).
    r.handle_key(ch('l'));
    assert_eq!(r.field, LoadField::Concurrency);
    r.handle_key(key(KeyCode::Right));
    assert_eq!(r.field, LoadField::Interval);
    r.handle_key(ch('h'));
    assert_eq!(r.field, LoadField::Concurrency);
    r.handle_key(key(KeyCode::Left));
    assert_eq!(r.field, LoadField::Total);
    // j/k step the value now, not the field (see step_focused_field_value).
    r.handle_key(ch('j'));
    r.handle_key(ch('k'));
    assert_eq!(r.field, LoadField::Total);
}

#[test]
fn step_focused_field_value() {
    let mut r = runner(); // total=10, concurrency=5, interval=0ms
    // k/Up increments the focused field; j/Down decrements it.
    r.handle_key(ch('k'));
    assert_eq!(r.cfg.total, 11);
    r.handle_key(key(KeyCode::Up));
    assert_eq!(r.cfg.total, 12);
    r.handle_key(ch('j'));
    assert_eq!(r.cfg.total, 11);
    r.handle_key(key(KeyCode::Down));
    assert_eq!(r.cfg.total, 10);
    // Concurrency steps too.
    r.handle_key(ch('l')); // → Concurrency
    r.handle_key(ch('k'));
    assert_eq!(r.cfg.concurrency, 6);
    // Interval steps by 1ms.
    r.handle_key(ch('l')); // → Interval
    r.handle_key(ch('k'));
    assert_eq!(r.cfg.interval, Duration::from_millis(1));
    r.handle_key(ch('j'));
    assert_eq!(r.cfg.interval, Duration::ZERO);
}

#[test]
fn step_field_clamps_at_minimums() {
    let mut r = runner();
    r.cfg.total = 1;
    r.handle_key(ch('j')); // can't go below 1
    assert_eq!(r.cfg.total, 1);
    r.handle_key(ch('l')); // → Concurrency
    r.cfg.concurrency = 1;
    r.handle_key(ch('j'));
    assert_eq!(r.cfg.concurrency, 1);
    r.handle_key(ch('l')); // → Interval
    r.cfg.interval = Duration::ZERO;
    r.handle_key(ch('j')); // interval down-clamps at 0ms
    assert_eq!(r.cfg.interval, Duration::ZERO);
}

#[test]
fn step_field_is_noop_while_editing_or_running() {
    // While editing, j/k feed the editor (digits ignored), not the stepper.
    let mut r = runner();
    r.handle_key(ch('i')); // begin edit of total (seed "10")
    assert!(r.editing.is_some());
    r.handle_key(ch('k'));
    r.handle_key(ch('j'));
    assert_eq!(r.cfg.total, 10, "stepping is disabled while editing");
    // While running, config is locked entirely.
    let mut r = runner();
    r.reset_for_run();
    let before = r.cfg.total;
    r.handle_key(ch('k'));
    r.handle_key(ch('j'));
    assert_eq!(r.cfg.total, before, "stepping is disabled during a run");
}

#[test]
fn config_field_pick_and_numeric_edit_clamps() {
    let mut r = runner();
    // Pick the concurrency field, edit it to 0 → clamps to 1.
    r.handle_key(ch('l')); // Total → Concurrency
    assert_eq!(r.field, LoadField::Concurrency);
    r.handle_key(ch('i')); // begin edit
    assert!(r.editing.is_some());
    // Clear the seed and type 0.
    r.handle_key(key(KeyCode::Backspace));
    r.handle_key(ch('0'));
    r.handle_key(key(KeyCode::Enter));
    assert!(r.editing.is_none());
    assert_eq!(r.cfg.concurrency, 1, "concurrency clamps to >= 1");
}

#[test]
fn edit_accepts_only_digits() {
    let mut r = runner();
    r.handle_key(ch('i')); // edit total (seed "10")
    r.handle_key(ch('x')); // ignored
    r.handle_key(ch('5')); // "105"
    r.handle_key(key(KeyCode::Enter));
    assert_eq!(r.cfg.total, 105);
}

#[test]
fn interval_edits_to_zero_allowed() {
    let mut r = runner();
    r.handle_key(ch('l'));
    r.handle_key(ch('l')); // → Interval
    assert_eq!(r.field, LoadField::Interval);
    r.handle_key(ch('i'));
    r.handle_key(ch('2'));
    r.handle_key(ch('5'));
    r.handle_key(key(KeyCode::Enter));
    assert_eq!(r.cfg.interval, Duration::from_millis(25));
}

#[test]
fn ctrl_r_requests_run() {
    // Run is Ctrl-R (matches the sequence surface); plain `r` does not run.
    let mut r = runner();
    let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(r.handle_key(ctrl_r), LoadOutcome::Run);
    // Plain `r` is now inert at the config header (no run).
    let mut r2 = runner();
    assert_ne!(r2.handle_key(ch('r')), LoadOutcome::Run);
}

#[test]
fn q_closes_when_idle() {
    let mut r = runner();
    assert_eq!(r.handle_key(ch('q')), LoadOutcome::Close);
}

#[test]
fn q_confirms_then_closes_while_running() {
    let mut r = runner();
    r.reset_for_run();
    assert_eq!(r.handle_key(ch('q')), LoadOutcome::Consumed);
    assert!(r.confirming_close);
    assert_eq!(r.handle_key(key(KeyCode::Esc)), LoadOutcome::Consumed);
    assert!(!r.confirming_close);
    r.handle_key(ch('q'));
    assert_eq!(r.handle_key(ch('y')), LoadOutcome::Close);
}

#[test]
fn ctrl_c_cancels_only_while_running() {
    let mut r = runner();
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(r.handle_key(ctrl_c), LoadOutcome::Consumed);
    r.reset_for_run();
    assert_eq!(r.handle_key(ctrl_c), LoadOutcome::Cancel);
}

#[test]
fn guardrail_confirm_yes_runs_no_aborts() {
    let mut r = runner();
    r.pending_confirm = Some("Fire 500 requests?".to_owned());
    // `n` aborts.
    assert_eq!(r.handle_key(ch('n')), LoadOutcome::Consumed);
    assert!(r.pending_confirm.is_none());
    // `y` confirms.
    r.pending_confirm = Some("Fire 500 requests?".to_owned());
    assert_eq!(r.handle_key(ch('y')), LoadOutcome::ConfirmedRun);
    assert!(r.pending_confirm.is_none());
}

#[test]
fn record_result_updates_stats_and_follows() {
    let mut r = runner();
    r.reset_for_run(); // 10 pending rows
    let done = r.record_result(
        0,
        LoadStatus::Ok(200),
        Some(Duration::from_millis(12)),
        ResponseState::Idle,
        ReqOutcome::Ok { status: 200 },
    );
    assert!(!done);
    assert_eq!(r.completed, 1);
    assert_eq!(r.stats.ok, 1);
    assert_eq!(r.stats.min, Some(Duration::from_millis(12)));
    assert_eq!(r.selected, 0, "follow selected the completed row");
}

#[test]
fn record_result_reports_done_on_last() {
    let mut r = runner();
    r.cfg.total = 2;
    r.reset_for_run();
    assert!(!r.record_result(
        0,
        LoadStatus::Ok(200),
        Some(Duration::from_millis(5)),
        ResponseState::Idle,
        ReqOutcome::Ok { status: 200 },
    ));
    assert!(r.record_result(
        1,
        LoadStatus::Failed(500),
        Some(Duration::from_millis(7)),
        ResponseState::Idle,
        ReqOutcome::Failed { status: 500 },
    ));
    assert_eq!(r.stats.ok, 1);
    assert_eq!(r.stats.failed, 1);
}

#[test]
fn manual_navigation_disables_follow() {
    let mut r = runner();
    r.reset_for_run();
    r.focus = RunnerFocus::Results;
    r.handle_key(ch('j'));
    assert!(!r.follow);
    assert_eq!(r.selected, 1);
    // A later completion no longer moves the selection.
    r.record_result(
        5,
        LoadStatus::Ok(200),
        Some(Duration::from_millis(3)),
        ResponseState::Idle,
        ReqOutcome::Ok { status: 200 },
    );
    assert_eq!(r.selected, 1, "follow off keeps the user's selection");
}

#[test]
fn cannot_edit_config_while_running() {
    let mut r = runner();
    r.reset_for_run();
    r.handle_key(ch('i'));
    assert!(r.editing.is_none(), "config is locked during a run");
}

#[test]
fn stats_line_shows_counts_and_percentiles() {
    let mut r = runner();
    r.cfg.total = 2;
    r.reset_for_run();
    r.record_result(
        0,
        LoadStatus::Ok(200),
        Some(Duration::from_millis(10)),
        ResponseState::Idle,
        ReqOutcome::Ok { status: 200 },
    );
    r.record_result(
        1,
        LoadStatus::Failed(500),
        Some(Duration::from_millis(30)),
        ResponseState::Idle,
        ReqOutcome::Failed { status: 500 },
    );
    let line = stats_line(&r);
    assert!(line.contains("2/2 done"), "{line}");
    assert!(line.contains("1 ok"), "{line}");
    assert!(line.contains("1 failed"), "{line}");
    // Grouped latency parts (L3): range (en dash), p50/p95, avg.
    assert!(line.contains("range 10–30ms"), "{line}");
    assert!(line.contains("p50/p95"), "{line}");
    assert!(line.contains("avg "), "{line}");
    // Old separate labels are gone.
    assert!(!line.contains("min "), "{line}");
    assert!(!line.contains("mean "), "{line}");
}

// ---- render snapshots (TestBackend → deterministic plain text) ----

use churl_core::model::{Header, Response, Timing};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn snapshot(state: &mut LoadRunnerState) -> String {
    let backend = TestBackend::new(116, 24);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let theme = Theme::default();
    let cache = HashMap::new();
    terminal
        .draw(|frame| {
            let _ = render(frame, frame.area(), state, 0, &cache, &theme);
        })
        .expect("draw");
    format!("{}", terminal.backend())
}

fn json_response(status: u16, body: &str) -> Response {
    Response {
        status,
        headers: vec![Header {
            name: "Content-Type".into(),
            value: "application/json".into(),
            enabled: true,
        }],
        body: body.as_bytes().to_vec(),
        truncated: false,
        timing: Timing {
            connect: None,
            total: Duration::from_millis(42),
        },
    }
}

#[test]
fn snapshot_config_header_pre_run() {
    let mut r = runner();
    insta::assert_snapshot!(snapshot(&mut r));
}

/// C2: the top-left block reads name → url → config → stats in that row order,
/// and a dim one-line purpose hint explains the pane at a glance.
#[test]
fn top_left_block_ordered_name_url_config_stats_with_hint() {
    let mut r = runner();
    let out = snapshot(&mut r);
    let name = out.find("List users").expect("name row");
    let url = out.find("https://api.test/users").expect("url row");
    let config = out.find("total=10").expect("config row");
    let stats = out.find("0/0 done").expect("stats row");
    assert!(
        name < url && url < config && config < stats,
        "expected name → url → config → stats order:\n{out}"
    );
    assert!(
        out.contains("measure throughput and latency"),
        "one-line purpose hint missing:\n{out}"
    );
}

/// L1: the Response pane must be a bordered, titled pane with a placeholder
/// from the moment the runner opens — never a blank right half — even though
/// there is no selectable results row yet (`results` is empty pre-run).
#[test]
fn response_pane_renders_pre_run() {
    let mut r = runner();
    assert!(r.results.is_empty(), "no rows before the first run");
    let out = snapshot(&mut r);
    assert!(
        out.contains("Response"),
        "titled Response pane missing pre-run:\n{out}"
    );
    assert!(
        out.contains("no response yet"),
        "idle placeholder missing pre-run:\n{out}"
    );
}

#[test]
fn snapshot_mid_run_live_stats_mixed_glyphs() {
    let mut r = runner();
    r.cfg.total = 8;
    r.reset_for_run();
    r.record_result(
        0,
        LoadStatus::Ok(200),
        Some(Duration::from_millis(12)),
        ResponseState::Idle,
        ReqOutcome::Ok { status: 200 },
    );
    r.record_result(
        1,
        LoadStatus::Failed(503),
        Some(Duration::from_millis(88)),
        ResponseState::Idle,
        ReqOutcome::Failed { status: 503 },
    );
    r.record_result(
        2,
        LoadStatus::Error("connection refused".into()),
        None,
        ResponseState::Idle,
        ReqOutcome::Error("connection refused".into()),
    );
    // Two copies still in flight; the rest pending.
    r.results[3].status = LoadStatus::Running;
    r.results[4].status = LoadStatus::Running;
    r.focus = RunnerFocus::Results;
    insta::assert_snapshot!(snapshot(&mut r));
}

#[test]
fn snapshot_finished_with_failures_stats_line() {
    let mut r = runner();
    r.cfg.total = 5;
    r.reset_for_run();
    for (i, (status, outcome, ms)) in [
        (LoadStatus::Ok(200), ReqOutcome::Ok { status: 200 }, 10u64),
        (LoadStatus::Ok(200), ReqOutcome::Ok { status: 200 }, 20),
        (
            LoadStatus::Failed(500),
            ReqOutcome::Failed { status: 500 },
            30,
        ),
        (LoadStatus::Ok(200), ReqOutcome::Ok { status: 200 }, 40),
        (
            LoadStatus::Failed(500),
            ReqOutcome::Failed { status: 500 },
            50,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        r.record_result(
            i,
            status,
            Some(Duration::from_millis(ms)),
            ResponseState::Idle,
            outcome,
        );
    }
    r.running = false;
    r.finished = true;
    insta::assert_snapshot!(snapshot(&mut r));
}

/// Memory bound: a high-`total` run holds a bounded number of live
/// `Done` views (`K + 1`: the window plus the selected row), and — regardless
/// of eviction — the stats computed over *all* outcomes stay correct.
#[test]
fn high_total_run_bounds_retained_views() {
    let mut r = runner();
    r.cfg.total = 500;
    r.reset_for_run();
    r.focus = RunnerFocus::Results;

    for i in 0..500 {
        let view = response::ResponseView::build(
            &json_response(200, &format!("{{\"i\":{i}}}")),
            r.next_view_gen(),
        );
        r.record_result(
            i,
            LoadStatus::Ok(200),
            Some(Duration::from_millis((i % 100) as u64 + 1)),
            ResponseState::Done { view },
            ReqOutcome::Ok { status: 200 },
        );
    }

    let live = r
        .results
        .iter()
        .filter(|row| matches!(row.response, ResponseState::Done { .. }))
        .count();
    assert!(
        live <= LIVE_VIEW_WINDOW + 1,
        "retained views {live} exceed the K+1 bound ({})",
        LIVE_VIEW_WINDOW + 1
    );
    // Evicted rows are downgraded honestly, not lost or left Idle.
    let dropped = r
        .results
        .iter()
        .filter(|row| matches!(row.response, ResponseState::Dropped { .. }))
        .count();
    assert_eq!(
        live + dropped,
        500,
        "every completed row is Done or Dropped"
    );

    // Stats span every outcome, not just retained ones.
    assert_eq!(r.completed, 500);
    assert_eq!(r.stats.ok, 500);
    assert_eq!(r.stats.failed, 0);
    assert_eq!(r.stats.min, Some(Duration::from_millis(1)));
    assert_eq!(r.stats.max, Some(Duration::from_millis(100)));
}

/// The currently-selected row is never evicted, even when it is the oldest
/// retained view (follow off, then many later completions land).
#[test]
fn selected_row_is_never_evicted() {
    let mut r = runner();
    r.cfg.total = 200;
    r.reset_for_run();
    r.focus = RunnerFocus::Results;

    // Complete + retain row 0, then pin the selection there (follow off).
    let done_row = |r: &mut LoadRunnerState, i: usize| {
        let view =
            response::ResponseView::build(&json_response(200, "{\"ok\":true}"), r.next_view_gen());
        r.record_result(
            i,
            LoadStatus::Ok(200),
            Some(Duration::from_millis(5)),
            ResponseState::Done { view },
            ReqOutcome::Ok { status: 200 },
        );
    };
    done_row(&mut r, 0);
    r.handle_key(key(KeyCode::Home)); // select row 0, follow off
    assert_eq!(r.selected, 0);

    for i in 1..200 {
        done_row(&mut r, i);
    }
    // Row 0 held a view and was the selection the whole time → still live.
    assert!(
        matches!(r.results[0].response, ResponseState::Done { .. }),
        "the selected row was evicted"
    );
}

#[test]
fn snapshot_selected_response_shown() {
    let mut r = runner();
    r.cfg.total = 2;
    r.reset_for_run();
    let view =
        response::ResponseView::build(&json_response(200, "{\"ok\":true}"), r.next_view_gen());
    r.record_result(
        0,
        LoadStatus::Ok(200),
        Some(Duration::from_millis(42)),
        ResponseState::Done { view },
        ReqOutcome::Ok { status: 200 },
    );
    r.running = false;
    r.finished = true;
    r.selected = 0;
    r.focus = RunnerFocus::Response;
    insta::assert_snapshot!(snapshot(&mut r));
}
