use std::collections::HashMap;
use std::time::Duration;

use super::render::status_summary;
use super::*;
use crate::tui::theme::Theme;
use churl_core::model::{Header, Response};

fn view(body: &str) -> ResponseView {
    response_with(body, Vec::new(), false)
}

fn response_with(body: &str, headers: Vec<Header>, truncated: bool) -> ResponseView {
    let response = Response {
        status: 200,
        headers,
        body: body.as_bytes().to_vec(),
        truncated,
        timing: Timing {
            connect: None,
            total: Duration::from_millis(5),
        },
    };
    ResponseView::build(&response, 1)
}

fn json_view(body: &str) -> ResponseView {
    let response = Response {
        status: 200,
        headers: vec![Header {
            name: "Content-Type".to_owned(),
            value: "application/json".to_owned(),
            enabled: true,
        }],
        body: body.as_bytes().to_vec(),
        truncated: false,
        timing: Timing {
            connect: None,
            total: Duration::from_millis(5),
        },
    };
    ResponseView::build(&response, 1)
}

#[test]
fn line_offsets_split_multiline_body() {
    let view = view("a\nbb\nccc");
    assert_eq!(view.line_count(), 3);
    assert_eq!(view.logical_line(0), "a");
    assert_eq!(view.logical_line(1), "bb");
    assert_eq!(view.logical_line(2), "ccc");
}

#[test]
fn empty_body_has_no_lines() {
    assert_eq!(view("").line_count(), 0);
}

#[test]
fn crlf_stripped_from_logical_lines() {
    let view = view("a\r\nb\r\nc");
    assert_eq!(view.logical_line(0), "a");
    assert_eq!(view.logical_line(1), "b");
    assert_eq!(view.logical_line(2), "c");
}

#[test]
fn scroll_clamps_to_last_screenful() {
    assert_eq!(clamp_scroll(0, 10, 4), 0);
    assert_eq!(clamp_scroll(6, 10, 4), 6);
    assert_eq!(clamp_scroll(9, 10, 4), 6);
    assert_eq!(clamp_scroll(3, 2, 4), 0);
}

#[test]
fn byte_size_formatting() {
    assert_eq!(fmt_bytes(512), "512 B");
    assert_eq!(fmt_bytes(4198), "4.1 KB");
    assert_eq!(fmt_bytes(2 * 1024 * 1024), "2.0 MB");
}

#[test]
fn status_summary_markers() {
    let mut v = view("body");
    // The `· headers` view-mode marker (distinct from the `N headers` count).
    assert!(!status_summary(&v, true).contains("· headers"));
    assert!(!status_summary(&v, true).contains("wrap"));
    v.toggle_wrap();
    assert!(status_summary(&v, true).contains("· wrap"));
    v.view_mode = ViewMode::Headers;
    assert!(status_summary(&v, true).contains("· headers"));
}

#[test]
fn status_summary_shows_sorted_marker() {
    // A pretty JSON body with the A→Z key sort active surfaces `· sorted`;
    // off it does not. Uses a JSON body so pretty/sort
    // are meaningful.
    let mut v = json_view("{\"b\":1,\"a\":2}");
    assert!(v.pretty(), "JSON is pretty by default");
    assert!(!status_summary(&v, true).contains("sorted"));
    v.toggle_sort_keys();
    assert!(v.sort_keys());
    assert!(status_summary(&v, true).contains("· sorted"));
    v.toggle_sort_keys();
    assert!(!status_summary(&v, true).contains("sorted"));
}

#[test]
fn status_summary_appends_truncated_marker() {
    let mut v = view("body");
    assert!(!status_summary(&v, true).contains("truncated"));
    v.truncated = true;
    v.byte_size = 10 * 1024 * 1024;
    assert_eq!(
        status_summary(&v, true),
        "200 OK · 5 ms · 10.0 MB · 0 headers  [h] · truncated at 10.0 MB"
    );
}

#[test]
fn status_summary_pluralizes_header_count() {
    let header = |name: &str| Header {
        name: name.to_owned(),
        value: "x".to_owned(),
        enabled: true,
    };
    // 0 headers → plural.
    let zero = response_with("body", Vec::new(), false);
    assert!(status_summary(&zero, true).contains("· 0 headers"));
    // 1 header → singular.
    let one = response_with("body", vec![header("A")], false);
    assert!(status_summary(&one, true).contains("· 1 header"));
    assert!(!status_summary(&one, true).contains("1 headers"));
    // 2 headers → plural.
    let two = response_with("body", vec![header("A"), header("B")], false);
    assert!(status_summary(&two, true).contains("· 2 headers"));
    // Body view appends the `[h]` full-headers affordance after the count —
    // but only when the pane is focused.
    assert!(status_summary(&two, true).contains("2 headers  [h]"));
}

#[test]
fn status_summary_omits_hint_when_unfocused() {
    // The `[h]` full-headers affordance is focus-gated: an unfocused
    // Body-view response never shows it, so
    // embedded/collapsed viewers stay uncluttered.
    let v = view("body");
    assert_eq!(v.view_mode, ViewMode::Body);
    assert!(status_summary(&v, true).contains("[h]"));
    assert!(!status_summary(&v, false).contains("[h]"));
}

#[test]
fn headers_view_is_alternate_source() {
    let headers = vec![
        Header {
            name: "Content-Type".to_owned(),
            value: "application/json".to_owned(),
            enabled: true,
        },
        Header {
            name: "X-Req".to_owned(),
            value: "42".to_owned(),
            enabled: true,
        },
    ];
    let mut v = response_with("{}", headers, false);
    assert_eq!(v.view_mode(), ViewMode::Body);
    assert_eq!(v.toggle_view_mode(), ViewMode::Headers);
    assert_eq!(v.source_line_count(), 2);
    assert_eq!(v.logical_line(0), "Content-Type: application/json");
    assert_eq!(v.logical_line(1), "X-Req: 42");
}

#[test]
fn folding_only_for_json_body() {
    let mut plain = view("{\n1\n}");
    assert!(!plain.folding_supported());
    assert!(!plain.toggle_all_folds());
    let mut json = json_view("{\n  \"a\": 1\n}");
    assert!(json.folding_supported());
    assert!(json.toggle_all_folds());
}

#[test]
fn fold_at_cursor_hides_inner_lines() {
    let mut v = json_view("{\n  \"a\": 1,\n  \"b\": 2\n}");
    // 4 logical lines; fold the region opening at line 0.
    assert!(v.toggle_fold_at(0));
    let visible = v.visible_map();
    // Opener header + nothing else (closer is inside the folded span).
    assert_eq!(visible.len(), 1);
    assert!(matches!(
        visible[0],
        Visible::FoldHeader { line: 0, hidden: 3 }
    ));
    // Unfolding restores all 4 lines.
    assert!(v.toggle_fold_at(0));
    assert_eq!(v.visible_map().len(), 4);
}

#[test]
fn nested_fold_outer_hides_inner_state() {
    let body = "{\n  \"o\": {\n    \"i\": 1\n  }\n}";
    let mut v = json_view(body);
    // Fold inner (opener line 1) then outer (line 0).
    assert!(v.toggle_fold_at(1));
    assert!(v.toggle_fold_at(0));
    // Only the outer header shows.
    let visible = v.visible_map();
    assert_eq!(visible.len(), 1);
    assert!(matches!(visible[0], Visible::FoldHeader { line: 0, .. }));
    // Unfolding the outer restores the inner *folded* state (line 1 header).
    assert!(v.toggle_fold_at(0));
    let visible = v.visible_map();
    assert!(
        visible
            .iter()
            .any(|x| matches!(x, Visible::FoldHeader { line: 1, .. }))
    );
}

#[test]
fn toggle_all_collapses_then_expands() {
    let body = "{\n  \"a\": 1\n}\n[\n  2\n]";
    let mut v = json_view(body);
    // Two top-level regions. Collapse all.
    assert!(v.toggle_all_folds());
    let headers = v
        .visible_map()
        .iter()
        .filter(|x| matches!(x, Visible::FoldHeader { .. }))
        .count();
    assert_eq!(headers, 2);
    // Expand all.
    assert!(v.toggle_all_folds());
    assert_eq!(
        v.visible_map()
            .iter()
            .filter(|x| matches!(x, Visible::FoldHeader { .. }))
            .count(),
        0
    );
}

#[test]
fn wrap_expansion_splits_long_lines() {
    let v = view("abcdefghij");
    let visible = v.visible_map();
    // Width 4 → 3 rows (4+4+2).
    let rows = expand_wrap(&v, &visible, true, 4, 0);
    assert_eq!(rows.len(), 3);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 4));
    assert_eq!((rows[1].char_start, rows[1].char_end), (4, 8));
    assert_eq!((rows[2].char_start, rows[2].char_end), (8, 10));
}

#[test]
fn wrap_off_is_one_row_per_line() {
    let v = view("a\nbb\nccc");
    let visible = v.visible_map();
    let rows = expand_wrap(&v, &visible, false, 2, 0);
    assert_eq!(rows.len(), 3);
}

#[test]
fn wrap_wide_chars_respect_width() {
    // Two 2-cell CJK chars per row at width 4.
    let v = view("日本語漢字");
    let visible = v.visible_map();
    let rows = expand_wrap(&v, &visible, true, 4, 0);
    // 5 wide chars * 2 cells = 10 cells; width 4 → 2,2,1 chars → 3 rows.
    assert_eq!(rows.len(), 3);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 2));
    assert_eq!((rows[1].char_start, rows[1].char_end), (2, 4));
    assert_eq!((rows[2].char_start, rows[2].char_end), (4, 5));
}

#[test]
fn wrap_exact_width_line_is_one_row() {
    let v = view("abcd");
    let visible = v.visible_map();
    let rows = expand_wrap(&v, &visible, true, 4, 0);
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].char_start, rows[0].char_end), (0, 4));
}

#[test]
fn smart_case_insensitive_and_sensitive() {
    let mut v = view("Hello hello HELLO");
    // Lowercase query → case-insensitive: 3 matches.
    v.set_search("hello".to_owned());
    assert_eq!(v.search().unwrap().count(), 3);
    // Uppercase in query → case-sensitive: only "Hello".
    v.set_search("Hello".to_owned());
    assert_eq!(v.search().unwrap().count(), 1);
}

#[test]
fn case_insensitive_match_with_length_shifting_folds() {
    // `İ` (2 bytes) lowercases to 3 bytes; `ẞ` (3 bytes) to 2 — the totals
    // cancel, which would defeat an equal-length heuristic. The mapping
    // table must yield exactly one match with the *original* byte range.
    let line = "İneedleẞ";
    let v = view(line);
    let mut v = v;
    v.set_search("needle".to_owned());
    let search = v.search().unwrap();
    assert_eq!(search.count(), 1);
    let (l, s, e) = search.matches[0];
    assert_eq!(l, 0);
    assert_eq!(&line[s..e], "needle");
    assert_eq!((s, e), (2, 8));
}

#[test]
fn case_insensitive_counts_every_occurrence() {
    let mut v = view("BOB and BOB");
    v.set_search("bob".to_owned());
    assert_eq!(v.search().unwrap().count(), 2);
    let m = &v.search().unwrap().matches;
    assert_eq!((m[0].1, m[0].2), (0, 3));
    assert_eq!((m[1].1, m[1].2), (8, 11));
}

#[test]
fn set_search_auto_unfolds_first_match() {
    // Fold everything, then start typing a query whose first match is
    // buried inside the fold — set_search itself must unfold it (the
    // incremental-typing jump, not just n/N). The reformatter preserves wire
    // key order (preserve_order), so `needle` stays on line 1 after the
    // (already-pretty) body is canonicalised.
    let body = "{\n  \"needle\": 1,\n  \"b\": 2\n}";
    let mut v = json_view(body);
    assert!(v.toggle_all_folds()); // collapse all
    assert_eq!(v.visible_map().len(), 1);
    v.set_search("needle".to_owned());
    assert_eq!(v.current_match_line(), Some(1));
    assert!(
        v.visible_map().iter().any(|x| x.logical() == 1),
        "set_search must auto-unfold the region covering match 1"
    );
}

#[test]
fn search_step_wraps_around() {
    let mut v = view("x\nx\nx");
    v.set_search("x".to_owned());
    assert_eq!(v.search().unwrap().count(), 3);
    assert_eq!(v.current_match_line(), Some(0));
    assert_eq!(v.step_match(true), Some(1));
    assert_eq!(v.step_match(true), Some(2));
    // Wrap forward → back to 0.
    assert_eq!(v.step_match(true), Some(0));
    // Wrap backward → 2.
    assert_eq!(v.step_match(false), Some(2));
}

#[test]
fn search_no_matches_keeps_search_live() {
    let mut v = view("abc");
    v.set_search("zzz".to_owned());
    assert_eq!(v.search().unwrap().count(), 0);
    assert!(v.search().is_some());
    assert!(status_summary(&v, true).contains("no matches"));
}

#[test]
fn search_auto_unfolds_match() {
    let body = "{\n  \"needle\": 1,\n  \"b\": 2\n}";
    let mut v = json_view(body);
    assert!(v.toggle_fold_at(0)); // fold the outer object
    assert_eq!(v.visible_map().len(), 1); // only the header shows
    v.set_search("needle".to_owned());
    // The only match is on line 1 (inside the fold); step to it.
    let line = v.step_match(true).or_else(|| v.current_match_line());
    assert_eq!(line, Some(1));
    // Auto-unfold happened → the match line is now visible.
    let visible = v.visible_map();
    assert!(visible.iter().any(|x| x.logical() == 1));
}

#[test]
fn view_toggle_clears_search() {
    let mut v = view("hello");
    v.set_search("hello".to_owned());
    assert!(v.search().is_some());
    v.toggle_view_mode();
    assert!(v.search().is_none());
}

#[test]
fn copy_all_and_line() {
    let v = view("line one\nline two");
    assert_eq!(v.copy_all(), "line one\nline two");
    assert_eq!(v.copy_line(1), "line two");
    assert_eq!(v.copy_line(99), "");
}

#[test]
fn fold_opener_copies_block_else_line() {
    // Pretty body is three lines: `{`, `  "a": 1`, `}`. Line 0 is the `{` fold
    // opener; a leaf line is not an opener (U4).
    let mut v = json_view(r#"{"a":1}"#);
    assert_eq!(
        v.fold_region_at_opener(0),
        Some((0, 2)),
        "line 0 is the fold opener, region spans to the closer"
    );
    assert_eq!(
        v.fold_region_at_opener(1),
        None,
        "a leaf line is not an opener"
    );
    // A block copy is exactly the per-line copies joined by `\n` (byte-exact,
    // same as repeated single-line `Y`).
    let block = v.copy_region(0, 2);
    let expected = format!("{}\n{}\n{}", v.copy_line(0), v.copy_line(1), v.copy_line(2));
    assert_eq!(block, expected);
    assert!(
        block.contains("\"a\": 1"),
        "the region copy carries the inner leaf line: {block}"
    );
}

// ---- JSON response reformatter + pretty toggle ----

#[test]
fn minified_json_renders_pretty_by_default() {
    // petstore-style: minified single-line JSON must arrive multi-line.
    let v = json_view(r#"{"id":1,"tags":["a","b"],"nested":{"x":true}}"#);
    assert!(v.pretty(), "json-ish content-type defaults to pretty");
    assert!(
        v.line_count() > 1,
        "minified single-line JSON should render as multiple lines, got {}",
        v.line_count()
    );
    // The pretty text is what serde_json emits.
    let expected = serde_json::to_string_pretty(
        &serde_json::from_str::<serde_json::Value>(
            r#"{"id":1,"tags":["a","b"],"nested":{"x":true}}"#,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(v.text, expected);
}

#[test]
fn non_json_content_type_is_not_pretty() {
    // A plain-text body is never reformatted (pretty defaults off).
    let v = view("{\"a\":1}");
    assert!(!v.pretty());
    assert_eq!(v.text, "{\"a\":1}");
}

#[test]
fn toggle_pretty_to_raw_is_byte_exact() {
    let raw = r#"{"id":1,"tags":["a","b"]}"#;
    let mut v = json_view(raw);
    assert!(v.pretty());
    // Displayed text is pretty; raw source is the exact on-the-wire bytes.
    assert_ne!(v.text, raw);
    // Toggle to raw → displayed text is exactly the original bytes.
    assert!(!v.toggle_pretty());
    assert_eq!(v.text, raw);
    assert_eq!(v.copy_all(), raw);
    // Toggle back to pretty → reformatted again, and idempotent.
    assert!(v.toggle_pretty());
    assert_eq!(
        v.text,
        serde_json::to_string_pretty(&serde_json::from_str::<serde_json::Value>(raw).unwrap())
            .unwrap()
    );
}

#[test]
fn malformed_json_falls_back_to_raw_no_panic() {
    // Not valid JSON despite the json content-type: stay raw, never error.
    let raw = "{ this is not json ]";
    let v = json_view(raw);
    assert!(v.pretty(), "pretty flag is on (json content-type)");
    // But the displayed text is the untouched raw body (silent fallback).
    assert_eq!(v.text, raw);
    assert_eq!(v.line_count(), 1);
    assert_eq!(v.copy_all(), raw);
}

// ---- XML pretty-print (M8.7) ----

fn xml_view(body: &str) -> ResponseView {
    response_with(
        body,
        vec![Header {
            name: "Content-Type".to_owned(),
            value: "application/xml".to_owned(),
            enabled: true,
        }],
        false,
    )
}

#[test]
fn xml_pretty_reformats_minified_xml() {
    let raw = "<root><a>1</a><b><c>2</c></b></root>";
    let mut v = xml_view(raw);
    assert!(!v.pretty(), "xml content-type does not default to pretty");
    assert!(v.toggle_pretty(), "toggled on");
    assert!(
        v.text.lines().count() > 1,
        "pretty XML should be multi-line, got: {:?}",
        v.text
    );
    assert!(v.text.contains("<a>1</a>"));
    assert!(v.text.contains("<c>2</c>"));
}

#[test]
fn xml_pretty_toggle_is_byte_exact_on_copy() {
    // The headline invariant (M8.7): the normalized pretty view must NEVER
    // leak into what `y`/`Y` (and save) read — `raw_text`/`copy_all` stay the
    // exact on-the-wire bytes regardless of the pretty toggle.
    let raw = "<root><a>1</a></root>";
    let mut v = xml_view(raw);
    v.toggle_pretty();
    assert_ne!(v.text, raw, "displayed text is normalized/indented");
    assert_eq!(v.copy_all(), raw, "copy must stay byte-exact");
    assert_eq!(
        v.raw_bytes(),
        raw.as_bytes(),
        "raw_bytes must stay byte-exact"
    );
}

#[test]
fn malformed_xml_falls_back_to_raw_no_panic() {
    let raw = "<root><a>unclosed</root>";
    let mut v = xml_view(raw);
    v.toggle_pretty();
    assert_eq!(v.text, raw, "malformed XML must fall back to raw, silently");
}

#[test]
fn toggle_pretty_bumps_generation_and_resets_folds() {
    let mut v = json_view(r#"{"a":{"b":1},"c":2}"#);
    let gen0 = v.generation();
    // Fold something on the pretty text.
    assert!(v.toggle_all_folds());
    assert!(!v.folded.is_empty());
    // Toggle to raw: generation bumps (cache invalidation) and folds reset.
    v.toggle_pretty();
    assert_ne!(v.generation(), gen0, "generation must bump on toggle");
    assert!(v.folds.is_none(), "folds must reset to unscanned");
    assert!(v.folded.is_empty(), "folded openers must clear");
}

#[test]
fn fold_and_wrap_operate_on_pretty_text() {
    // Minified input → pretty multi-line; folding + wrap work on the result.
    let mut v = json_view(r#"{"outer":{"inner":1,"more":2},"tail":3}"#);
    assert!(v.pretty());
    assert!(v.line_count() > 1);
    // Fold the top-level object.
    assert!(v.folding_supported());
    assert!(v.toggle_all_folds());
    let headers = v
        .visible_map()
        .iter()
        .filter(|x| matches!(x, Visible::FoldHeader { .. }))
        .count();
    assert!(headers >= 1, "a fold header should appear on pretty text");
    // Wrap expansion over the pretty lines yields >= line_count rows.
    v.toggle_all_folds(); // expand back
    let visible = v.visible_map();
    let rows = expand_wrap(&v, &visible, true, 4, 0);
    assert!(rows.len() >= v.line_count());
}

#[test]
fn toggle_pretty_clears_search() {
    // A live search's matches are (logical_line, byte_start, byte_end) against
    // the OLD text; the raw↔pretty line layout differs, so a carried-over
    // search would lie about counts and mispaint overlays. Mirror
    // `view_toggle_clears_search`.
    let mut v = json_view(r#"{"needle":1,"other":2}"#);
    v.set_search("needle".to_owned());
    assert!(v.search().is_some());
    assert!(v.search().unwrap().count() >= 1);
    v.toggle_pretty();
    assert!(v.search().is_none(), "toggle_pretty must clear the search");
}

#[test]
fn truncated_json_falls_back_to_raw_and_keeps_marker() {
    // A body cut off at the size cap is not valid JSON — it must render raw
    // (no panic) while still reporting truncation.
    let response = Response {
        status: 200,
        headers: vec![Header {
            name: "Content-Type".to_owned(),
            value: "application/json".to_owned(),
            enabled: true,
        }],
        // Truncated mid-object: unparseable.
        body: br#"{"items":[{"id":1},{"id":2},{"na"#.to_vec(),
        truncated: true,
        timing: Timing {
            connect: None,
            total: Duration::from_millis(5),
        },
    };
    let v = ResponseView::build(&response, 1);
    assert!(v.pretty(), "json content-type still flags pretty");
    // But the malformed (truncated) body stays raw — no reformat, no panic.
    assert_eq!(v.text, r#"{"items":[{"id":1},{"id":2},{"na"#);
    assert_eq!(v.line_count(), 1);
    assert!(v.truncated(), "truncation marker is preserved");
}

// ---- raw_bytes / save_extension (M8.7 save-response-body) ----

#[test]
fn raw_bytes_is_byte_exact_for_non_utf8_body() {
    // The invariant `raw_bytes` exists for: `raw_text` is a LOSSY UTF-8 decode
    // (invalid bytes become U+FFFD), so it is not byte-exact for a binary
    // body. `raw_bytes` must be the literal wire bytes, unaffected by the
    // lossy decode that built `raw_text`.
    let body: Vec<u8> = vec![0xff, 0xfe, b'h', b'i', 0x00, 0x01];
    let response = Response {
        status: 200,
        headers: Vec::new(),
        body: body.clone(),
        truncated: false,
        timing: Timing {
            connect: None,
            total: Duration::from_millis(5),
        },
    };
    let v = ResponseView::build(&response, 1);
    assert_eq!(v.raw_bytes(), body.as_slice());
    // Sanity: the lossy text decode indeed differs from the raw bytes here
    // (replacement chars), proving this body actually exercises the gap.
    assert_ne!(v.raw_text.as_bytes(), body.as_slice());
}

#[test]
fn build_over_text_leaves_raw_bytes_empty() {
    // The Body-tab request-body-browse view (M8.6.1) has no *response* to
    // save — `raw_bytes` must stay empty so the save handler's emptiness
    // guard fires there.
    let v = ResponseView::build_over_text("hello", crate::tui::highlight::SyntaxToken::Plain, 1);
    assert!(v.raw_bytes().is_empty());
}

#[test]
fn save_extension_sniffed_from_content_type() {
    let json = response_with(
        r#"{"a":1}"#,
        vec![Header {
            name: "Content-Type".to_owned(),
            value: "application/json".to_owned(),
            enabled: true,
        }],
        false,
    );
    assert_eq!(json.save_extension(), "json");

    let html = response_with(
        "<html></html>",
        vec![Header {
            name: "Content-Type".to_owned(),
            value: "text/html; charset=utf-8".to_owned(),
            enabled: true,
        }],
        false,
    );
    assert_eq!(html.save_extension(), "html");

    let plain_text = response_with(
        "hello",
        vec![Header {
            name: "Content-Type".to_owned(),
            value: "text/plain".to_owned(),
            enabled: true,
        }],
        false,
    );
    assert_eq!(plain_text.save_extension(), "txt");

    let binary = response_with(
        "ignored",
        vec![Header {
            name: "Content-Type".to_owned(),
            value: "application/octet-stream".to_owned(),
            enabled: true,
        }],
        false,
    );
    assert_eq!(binary.save_extension(), "bin");

    let no_header = response_with("ignored", Vec::new(), false);
    assert_eq!(no_header.save_extension(), "bin");
}

#[test]
fn body_truncated_ignores_view_mode() {
    // `truncated()` is gated to the body view (it answers "is what's on
    // screen right now truncated"); `body_truncated()` must stay true
    // regardless, since save always saves the response BODY bytes.
    let mut v = response_with("partial", Vec::new(), true);
    assert!(v.truncated());
    assert!(v.body_truncated());
    v.toggle_view_mode();
    assert!(!v.truncated(), "headers view is never truncated");
    assert!(v.body_truncated(), "body_truncated ignores view mode");
}

#[test]
fn copy_all_is_raw_even_when_pretty() {
    // `y` copies raw on-the-wire bytes even while the body is shown pretty.
    let raw = r#"{"id":1,"name":"x"}"#;
    let v = json_view(raw);
    assert!(v.pretty());
    assert_ne!(v.text, raw, "displayed text is pretty");
    assert_eq!(v.copy_all(), raw, "copy must be the raw bytes");
}

// ---- optional A→Z key-sort toggle ----

#[test]
fn sort_keys_sorts_object_keys_recursively() {
    // Nested object with out-of-order keys at every level.
    let mut v = json_view(r#"{"z":{"b":1,"a":2},"a":3}"#);
    assert!(v.pretty());
    assert!(!v.sort_keys(), "sort defaults off");
    assert!(v.toggle_sort_keys(), "toggle turns sort on");
    // Keys A→Z at every level: top-level "a" before "z"; inner "a" before "b".
    let expected = serde_json::to_string_pretty(&serde_json::json!({
        "a": 3,
        "z": {"a": 2, "b": 1},
    }))
    .unwrap();
    assert_eq!(v.text, expected);
    // The line where each key appears confirms ordering (a before z at top).
    let a_line = v.text.find("\"a\"").unwrap();
    let z_line = v.text.find("\"z\"").unwrap();
    assert!(a_line < z_line, "top-level a must precede z when sorted");
}

#[test]
fn sort_keys_off_preserves_wire_order() {
    // Same input, sort off → preserve_order keeps server wire order (z first).
    let v = json_view(r#"{"z":{"b":1,"a":2},"a":3}"#);
    assert!(!v.sort_keys());
    let z_pos = v.text.find("\"z\"").unwrap();
    let a_pos = v.text.find("\"a\"").unwrap();
    assert!(z_pos < a_pos, "wire order: z precedes a when sort is off");
    // And inner keys stay wire order too: "b" before "a".
    let b_inner = v.text.find("\"b\"").unwrap();
    let a_inner = v.text.rfind("\"a\"").unwrap();
    assert!(b_inner < a_inner, "inner wire order: b precedes a");
}

#[test]
fn sort_keys_only_affects_pretty() {
    // Non-JSON view: `s` is a display no-op (reformat returns text unchanged),
    // so toggling the flag leaves the displayed text byte-identical.
    let mut v = view(r#"{"z":1,"a":2}"#);
    assert!(!v.pretty(), "plain-text body is not pretty");
    let before = v.text.clone();
    v.toggle_sort_keys();
    assert_eq!(v.text, before, "sort must not alter a non-pretty/raw view");

    // JSON view with pretty toggled OFF: sort flips but text stays raw bytes.
    let raw = r#"{"z":1,"a":2}"#;
    let mut v = json_view(raw);
    v.toggle_pretty(); // now raw
    assert!(!v.pretty());
    let raw_text = v.text.clone();
    assert_eq!(raw_text, raw);
    v.toggle_sort_keys();
    assert_eq!(v.text, raw, "sort is inert while pretty is off");
}

#[test]
fn toggle_sort_keys_clears_search_and_resets_folds() {
    let mut v = json_view(r#"{"needle":{"b":1},"a":2}"#);
    let gen0 = v.generation();
    // Fold something and set a live search on the pretty text.
    assert!(v.toggle_all_folds());
    assert!(!v.folded.is_empty());
    v.set_search("needle".to_owned());
    assert!(v.search().is_some());
    // Pan the horizontal window so we can prove the toggle resets it.
    v.scroll_h(true, 16);
    assert_eq!(v.h_scroll(), 16);
    // Toggle sort: search clears, folds reset, generation bumps, h_scroll resets.
    v.toggle_sort_keys();
    assert!(
        v.search().is_none(),
        "toggle_sort_keys must clear the search"
    );
    assert!(v.folds.is_none(), "folds must reset to unscanned");
    assert!(v.folded.is_empty(), "folded openers must clear");
    assert_ne!(v.generation(), gen0, "generation must bump on toggle");
    assert_eq!(
        v.h_scroll(),
        0,
        "toggle_sort_keys must reset the horizontal scroll (matching toggle_pretty/wrap)"
    );
}

#[test]
fn copy_all_is_raw_even_when_sorted() {
    // `y` copies raw on-the-wire bytes even with pretty + sort both on.
    let raw = r#"{"z":1,"a":2}"#;
    let mut v = json_view(raw);
    assert!(v.pretty());
    assert!(v.toggle_sort_keys());
    assert_ne!(v.text, raw, "displayed text is pretty + sorted");
    assert_eq!(v.copy_all(), raw, "copy must be the exact raw bytes");
}

#[test]
fn sort_keys_preserves_array_order() {
    // Arrays keep element order; only object keys sort. Elements that are
    // objects have their own keys sorted, but the array sequence is intact.
    let mut v = json_view(r#"{"list":[{"z":1,"a":2},{"y":3,"b":4}]}"#);
    assert!(v.toggle_sort_keys());
    let expected = serde_json::to_string_pretty(&serde_json::json!({
        "list": [{"a": 2, "z": 1}, {"b": 4, "y": 3}],
    }))
    .unwrap();
    assert_eq!(v.text, expected);
}

#[test]
fn build_search_spans_splits_around_match() {
    // "ab[cd]ef": match bytes 2..4 of the slice starting at byte 0.
    let line = build_search_spans("abcdef", 0, &[(2, 4, false)]);
    assert_eq!(line.spans.len(), 3);
    assert_eq!(line.spans[0].content, "ab");
    assert_eq!(line.spans[1].content, "cd");
    assert_eq!(line.spans[2].content, "ef");
}

// ---- control-char / ANSI sanitize + explicit tab-width ----

#[test]
fn sanitize_strips_ansi_csi_osc_and_lone_esc() {
    // CSI colour + cursor-move sequences, an OSC-52 clipboard write (BEL- and
    // ST-terminated), and a bare ESC — all must vanish, leaving only the text.
    let csi = "a\u{1B}[31mred\u{1B}[0m\u{1B}[2Jb";
    assert_eq!(sanitize_for_display(csi), "aredb");
    // OSC-52 clipboard hijack, BEL-terminated.
    let osc_bel = "x\u{1B}]52;c;aGVsbG8=\u{07}y";
    assert_eq!(sanitize_for_display(osc_bel), "xy");
    // OSC terminated by ST (ESC \).
    let osc_st = "x\u{1B}]0;title\u{1B}\\y";
    assert_eq!(sanitize_for_display(osc_st), "xy");
    // A lone ESC with no introducer is dropped; the following char survives.
    assert_eq!(sanitize_for_display("a\u{1B}b"), "ab");
}

#[test]
fn sanitize_expands_tabs_to_next_stop() {
    // Tab at column 0 → 4 spaces (next multiple of 4).
    assert_eq!(sanitize_for_display("\tx"), "    x");
    // "ab\t" → "ab" is 2 cols, tab advances to col 4 → 2 spaces.
    assert_eq!(sanitize_for_display("ab\tx"), "ab  x");
    // "abcd\t" sits exactly on a stop (col 4) → advances a full TAB_WIDTH.
    assert_eq!(sanitize_for_display("abcd\tx"), "abcd    x");
    // Tab stops reset at each newline: second line's tab starts from col 0.
    assert_eq!(sanitize_for_display("ab\tx\n\ty"), "ab  x\n    y");
    // Two consecutive tabs from col 0 → 4 then 4 more = 8 spaces.
    assert_eq!(sanitize_for_display("\t\tx"), "        x");
}

#[test]
fn sanitize_replaces_control_chars_with_placeholder() {
    // NUL, BEL, VT, and a lone CR → the visible middle-dot placeholder.
    assert_eq!(sanitize_for_display("a\u{00}b"), "a·b");
    assert_eq!(sanitize_for_display("a\u{07}b"), "a·b");
    assert_eq!(sanitize_for_display("a\u{0B}b"), "a·b");
    // A lone CR (not part of CRLF) is a control char → placeholder, NOT a
    // newline (so it must not add a phantom line).
    let out = sanitize_for_display("a\rb");
    assert_eq!(out, "a·b");
    assert_eq!(index_lines(&out).len(), 1);
    // A C1 control (0x85 NEL) is also replaced.
    assert_eq!(sanitize_for_display("a\u{85}b"), "a·b");
}

#[test]
fn sanitize_preserves_newlines_and_line_count() {
    // `\n` survives untouched; line count is exactly the newline count + 1.
    let out = sanitize_for_display("one\ntwo\nthree");
    assert_eq!(out, "one\ntwo\nthree");
    assert_eq!(index_lines(&out).len(), 3);
}

#[test]
fn sanitize_collapses_crlf_without_phantom_blank_lines() {
    // `\r\n` → `\n`: three CRLF lines become three lines, not six.
    let out = sanitize_for_display("a\r\nb\r\nc");
    assert_eq!(out, "a\nb\nc");
    assert_eq!(index_lines(&out).len(), 3);
}

#[test]
fn sanitize_returns_clean_text_byte_identical() {
    // A body with no control chars/ANSI/tabs is returned unchanged.
    let clean = "the quick brown fox\njumps over 123 {\"a\": 1}";
    assert_eq!(sanitize_for_display(clean), clean);
}

#[test]
fn build_sanitizes_displayed_text_but_copy_stays_raw() {
    // A text/plain body carrying ANSI + a tab + a NUL: the DISPLAYED text is
    // sanitized (ANSI gone, tab expanded, NUL → ·) but copy returns the exact
    // on-the-wire bytes including the raw ANSI/tab/control.
    let raw = "a\u{1B}[31m\tb\u{00}c";
    let v = view(raw);
    assert!(!v.pretty(), "text/plain is not pretty");
    // Displayed: ESC[31m stripped, tab at col 1 → 3 spaces (to col 4), NUL → ·.
    assert_eq!(v.text, "a   b·c");
    // Copy is byte-exact, controls and all — BOTH copy paths.
    assert_eq!(v.copy_all(), raw, "copy-all (y) is the raw bytes");
    assert_eq!(
        v.copy_line(0),
        raw,
        "copy-line (Y) must ALSO be the raw bytes, not the sanitized display"
    );
}

#[test]
fn copy_line_is_raw_on_multiline_text_plain_body() {
    // Multi-line text/plain: line 1 carries ANSI + tab + NUL. copy-line must
    // return the EXACT raw line bytes (the byte-exact invariant), never the sanitized
    // display. `copy_all` returns the whole raw body.
    let raw = "plain first\nsecond\u{1B}[31m\tx\u{00}y\nthird";
    let v = view(raw);
    assert!(!v.pretty());
    // Displayed line 1 is sanitized...
    assert_eq!(v.copy_line(0), "plain first");
    // ...but the mangled line's copy is byte-exact.
    assert_eq!(v.copy_line(1), "second\u{1B}[31m\tx\u{00}y");
    assert_eq!(v.copy_line(2), "third");
    assert_eq!(v.copy_all(), raw);
}

#[test]
fn copy_line_over_pretty_json_returns_displayed_line() {
    // On the reformatted (pretty-JSON) path the raw line index no longer maps
    // to the displayed line, so copy-line returns the DISPLAYED pretty line
    // (serde-escaped JSON has no raw controls to leak). This is the
    // pre-existing behaviour the fix must not regress.
    let v = json_view(r#"{"a":1,"b":2}"#);
    assert!(v.pretty());
    assert!(v.line_count() > 1, "minified input rendered pretty");
    // Line 0 of the pretty text is `{`.
    assert_eq!(v.copy_line(0), "{");
    // A middle line is the displayed pretty content (indented).
    let line1 = v.copy_line(1);
    assert!(
        line1.contains("\"a\": 1"),
        "copy-line returns the displayed pretty line, got {line1:?}"
    );
}

#[test]
fn copy_line_preserves_raw_crlf_bytes() {
    // A CRLF body: the display collapses `\r\n`→`\n`, but copy-line returns the
    // raw line INCLUDING its `\r` (exactly what the wire carried).
    let raw = "a\r\nb\r\nc";
    let v = view(raw);
    assert_eq!(v.line_count(), 3);
    assert_eq!(v.copy_line(0), "a\r");
    assert_eq!(v.copy_line(1), "b\r");
    assert_eq!(v.copy_line(2), "c");
}

// ---- horizontal-window slice for unwrapped long lines ----

/// A response whose single body line is `n` chars wide (unwrapped test fixture).
fn wide_line_view(width_chars: usize) -> ResponseView {
    view(&"x".repeat(width_chars))
}

#[test]
fn unwrapped_long_line_renders_bounded_slice() {
    // A 100-char line in a 10-wide viewport: the built row spans at most the
    // viewport width, NOT the whole line.
    let v = wide_line_view(100);
    let visible = v.visible_map();
    let rows = expand_wrap(&v, &visible, false, 10, 0);
    assert_eq!(rows.len(), 1, "still one row per unwrapped line");
    let (s, e) = (rows[0].char_start, rows[0].char_end);
    assert_eq!((s, e), (0, 10));
    assert!(e - s <= 10, "slice width must be bounded by the viewport");
    // Panned right by 30: the window is [30, 40).
    let rows = expand_wrap(&v, &visible, false, 10, 30);
    assert_eq!((rows[0].char_start, rows[0].char_end), (30, 40));
}

#[test]
fn h_scroll_pans_and_clamps_both_ends() {
    let mut v = wide_line_view(50);
    assert_eq!(v.h_scroll(), 0);
    // Pan left at the left edge stays clamped at 0.
    v.scroll_h(false, 8);
    assert_eq!(v.h_scroll(), 0);
    // Pan right accumulates.
    v.scroll_h(true, 8);
    assert_eq!(v.h_scroll(), 8);
    v.scroll_h(true, 8);
    assert_eq!(v.h_scroll(), 16);
    // The right-edge clamp is max_h_scroll(width) = 50 - width.
    assert_eq!(v.max_h_scroll(10), 40);
    assert_eq!(
        v.max_h_scroll(60),
        0,
        "line narrower than viewport → no scroll"
    );
    // Over-pan is clamped by render via min(max_h_scroll); simulate it.
    v.set_h_scroll(999);
    let clamped = v.h_scroll().min(v.max_h_scroll(10));
    assert_eq!(clamped, 40);
}

#[test]
fn h_scroll_is_inert_when_wrap_on() {
    let mut v = wide_line_view(50);
    v.toggle_wrap(); // wrap on, resets h_scroll to 0
    assert!(v.wrap());
    v.scroll_h(true, 8);
    assert_eq!(v.h_scroll(), 0, "scroll_h is a no-op while wrap is on");
    assert_eq!(v.max_h_scroll(10), 0, "no horizontal scroll under wrap");
}

#[test]
fn toggle_wrap_resets_h_scroll() {
    let mut v = wide_line_view(50);
    v.scroll_h(true, 24);
    assert_eq!(v.h_scroll(), 24);
    v.toggle_wrap();
    assert_eq!(
        v.h_scroll(),
        0,
        "toggling wrap must reset the horizontal scroll"
    );
}

#[test]
fn search_jump_brings_far_right_match_into_window() {
    // A single line with a needle far to the right; a 10-wide window at
    // h_scroll 0 would not show it. ensure_column_visible must pan so the
    // match columns fall inside [h_scroll, h_scroll+width).
    let body = format!("{}NEEDLE tail", "x".repeat(40));
    let mut v = view(&body);
    v.set_search("NEEDLE".to_owned());
    let (start, end) = v.current_match_columns().expect("a match");
    assert_eq!(start, 40, "match starts at column 40");
    assert_eq!(end, 46);
    // Off the right edge of a 10-wide window at h_scroll 0.
    assert!(end > 10);
    v.ensure_column_visible(start, end, 10);
    // The match end is now the last visible column: window = [36, 46).
    assert_eq!(v.h_scroll(), 36);
    assert!(v.h_scroll() <= start && end <= v.h_scroll() + 10);
    // A subsequent left-edge match pans back left.
    v.set_h_scroll(30);
    v.ensure_column_visible(2, 8, 10); // match at cols 2..8, left of window
    assert_eq!(v.h_scroll(), 2);
}

#[test]
fn copy_line_returns_full_raw_line_despite_h_scroll() {
    // Panning the window must never truncate NOR sanitize what copy-line
    // returns. The body carries ANSI + a tab + a NUL amid a long run so both
    // the h_scroll window AND the sanitize pass would corrupt a naive copy —
    // copy-line must return the exact raw line bytes regardless.
    let body = format!("{}\u{1B}[31m\tmid\u{00}{}", "x".repeat(60), "y".repeat(60));
    let mut v = view(&body);
    // Sanitize actually changed the display (so this can catch the leak).
    assert_ne!(
        v.text, body,
        "displayed text is sanitized, not the raw body"
    );
    v.scroll_h(true, 50);
    assert_eq!(
        v.copy_line(0),
        body,
        "copy-line returns the full RAW line, unaffected by h_scroll or sanitize"
    );
    assert_eq!(v.copy_all(), body, "copy-all returns the full raw body");
}

// ---- line-number gutter ----

/// Renders a `Done` view into a `TestBackend` of the given size and returns
/// the body rows as strings (the whole inner area, borders included). Used to
/// assert the gutter is actually painted (not just the geometry math).
fn render_rows(view: ResponseView, w: u16, h: u16) -> Vec<String> {
    render_rows_scrolled(view, w, h, 0)
}

/// Like [`render_rows`] but with a starting display-row `scroll` — needed to
/// exercise a wrap-continuation row *at the viewport top* (the gutter
/// first-row seed is only wrong when `scroll > 0`).
fn render_rows_scrolled(view: ResponseView, w: u16, h: u16, scroll: usize) -> Vec<String> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let state = ResponseState::Done { view };
    let cache = HashMap::new();
    let theme = Theme::default();
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                RenderCtx {
                    state: &state,
                    request: None,
                    focused: false,
                    scroll,
                    cursor: scroll,
                    cache: &cache,
                    theme: &theme,
                    jump_label: None,
                    tick_count: 0,
                },
            );
        })
        .expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..h)
        .map(|y| (0..w).map(|x| buffer[(x, y)].symbol().to_owned()).collect())
        .collect()
}

#[test]
fn gutter_is_on_by_default() {
    // The observable contract: a freshly built view shows the gutter.
    let v = view("a\nb\nc");
    assert!(v.line_numbers(), "the gutter defaults ON");
}

#[test]
fn toggle_flips_gutter_visibility() {
    let mut v = view("a\nb");
    assert!(v.line_numbers());
    v.toggle_line_numbers();
    assert!(!v.line_numbers(), "toggle hides the gutter");
    v.toggle_line_numbers();
    assert!(v.line_numbers(), "toggle shows it again");
}

#[test]
fn gutter_width_scales_with_line_count_and_is_zero_when_off() {
    // 1–9 lines → 1 digit + 1 pad = 2; 10+ lines → 2 digits + 1 pad = 3.
    let mut v = view("a\nb\nc"); // 3 lines
    assert_eq!(v.gutter_width(), 2);
    let big: String = (0..12)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let mut v2 = view(&big); // exactly 12 lines
    assert_eq!(v2.source_line_count(), 12);
    assert_eq!(v2.gutter_width(), 3, "two-digit count → width 3");
    // Off → the gutter consumes nothing.
    v.toggle_line_numbers();
    assert_eq!(v.gutter_width(), 0);
    v2.toggle_line_numbers();
    assert_eq!(v2.gutter_width(), 0);
}

#[test]
fn gutter_renders_right_aligned_1_based_numbers() {
    // Ten lines so the width is 3 (two digits + pad): line 9 is ` 9 `,
    // line 10 is `10 ` — right-aligned in the same column.
    let body: String = (0..10)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let rows = render_rows(view(&body), 40, 14);
    // Body starts at inner row 1 (row 0 is the top border). Content rows begin
    // after the border; find the row carrying "line0".
    let first = rows
        .iter()
        .find(|r| r.contains("line0"))
        .expect("line0 row");
    assert!(
        first.contains(" 1 line0"),
        "row shows a right-aligned 1-based number, got {first:?}"
    );
    let tenth = rows
        .iter()
        .find(|r| r.contains("line9"))
        .expect("line9 row");
    assert!(
        tenth.contains("10 line9"),
        "the 10th logical line is numbered 10, got {tenth:?}"
    );
}

#[test]
fn wrapped_continuation_rows_have_a_blank_gutter() {
    // One long logical line, wrap on: the first display row shows the number;
    // the continuation rows show a blank (aligned) gutter. Asserted at the
    // render level — the painted body rows must carry the gutter only once.
    let mut v = view(&"w".repeat(60));
    v.toggle_wrap();
    assert!(v.wrap());
    // 40-wide pane: inner 38, gutter 2 → 36-col body, so 60 chars → 2 rows.
    let rows = render_rows(v, 40, 12);
    // Body rows carry the `www…` run; the title (`… wrap`) has a lone `w`, so
    // filter on the run to avoid matching the title row.
    let content: Vec<&String> = rows.iter().filter(|r| r.contains("ww")).collect();
    assert!(content.len() >= 2, "the long line wrapped to ≥2 rows");
    // The first content row carries the number `1 ` before the text.
    assert!(
        content[0].contains("1 w"),
        "first row numbered, got {:?}",
        content[0]
    );
    // Continuation rows: the text after the left border (thin `│` unfocused)
    // is preceded by the blank 2-col gutter (`  w…`), never a digit.
    for cont in &content[1..] {
        let after_border = cont.trim_start_matches('│');
        assert!(
            after_border.starts_with("  w"),
            "continuation row has a blank gutter then text, got {cont:?}"
        );
    }
}

/// Renders a focused `Done` view and returns the raw `TestBackend` buffer so a
/// caller can inspect per-cell *style* (not just symbols). Cursor sits at the
/// given display row.
fn render_buffer_focused(
    view: ResponseView,
    w: u16,
    h: u16,
    cursor: usize,
) -> ratatui::buffer::Buffer {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let state = ResponseState::Done { view };
    let cache = HashMap::new();
    let theme = Theme::default();
    let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
    terminal
        .draw(|frame| {
            render(
                frame,
                frame.area(),
                RenderCtx {
                    state: &state,
                    request: None,
                    focused: true,
                    scroll: 0,
                    cursor,
                    cache: &cache,
                    theme: &theme,
                    jump_label: None,
                    tick_count: 0,
                },
            );
        })
        .expect("draw");
    terminal.backend().buffer().clone()
}

#[test]
fn cursor_row_content_is_selection_highlighted_with_the_gutter_on() {
    // Regression: the response cursor was invisible whenever the line-number
    // gutter was on (its default) — `prepend_gutter`'s `Line::from(spans)`
    // dropped the line-level selection style `apply_cursor` had set, so `j`/`k`
    // moved an unseen cursor and the pane read as "scroll broken". The contract:
    // with the gutter ON and the pane focused, the cursor row carries the theme
    // selection background as a full-width cursorline (gutter number included).
    let sel_bg = Theme::default().selection.bg.expect("selection has a bg");
    let body: String = (0..10)
        .map(|i| format!("line{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let v = view(&body);
    assert!(
        v.line_numbers(),
        "gutter defaults on — the buggy configuration"
    );
    let buf = render_buffer_focused(v, 40, 14, 0);

    // Locate the cursor row (row carrying "line0") and the column where its
    // content ("line0") begins, past the border + gutter.
    let (row_y, text_x) = (0..buf.area.height)
        .find_map(|y| {
            let row: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_owned())
                .collect();
            row.find("line0").map(|byte_x| (y, byte_x as u16))
        })
        .expect("cursor row with line0");

    assert_eq!(
        buf[(text_x, row_y)].style().bg,
        Some(sel_bg),
        "cursor-row content must carry the selection background"
    );
    // Full-width cursorline: the gutter number ('1', two cols left of the text)
    // is part of the highlight too.
    assert_eq!(
        buf[(text_x - 2, row_y)].style().bg,
        Some(sel_bg),
        "the gutter number shares the cursor row's selection background"
    );
    // A non-cursor row's content is unstyled by the selection.
    let (other_y, other_x) = (0..buf.area.height)
        .find_map(|y| {
            let row: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_owned())
                .collect();
            row.find("line1").map(|bx| (y, bx as u16))
        })
        .expect("row with line1");
    assert_ne!(
        buf[(other_x, other_y)].style().bg,
        Some(sel_bg),
        "a non-cursor row must not be selection-highlighted"
    );
}

#[test]
fn continuation_row_at_viewport_top_has_a_blank_gutter() {
    // P1 regression guard: line 1 is long (wraps to 9 rows at a 36-col body),
    // line 2 is short. Scroll so the TOP visible row is a wrap-continuation of
    // line 1 (its numbered first row scrolled off). With a `None` seed the top
    // row would wrongly show `1`; the seed-from-`rows[scroll-1]` fix must make
    // it a blank gutter. Row below (still a continuation) is also blank; the
    // first row of line 2 further down is correctly numbered `2`.
    let body = format!("{}\nSECONDLINE", "a".repeat(300));
    let mut v = view(&body);
    v.toggle_wrap();
    assert!(v.wrap());
    // 40-wide pane → inner 38, gutter 2 → 36-col body. 300 chars → 9 rows for
    // line 1 (rows 0..9), then line 2 at row 9 (10 rows total). Pane height 10 →
    // 8 body rows, so `clamp_scroll(2, 10, 8) == 2` holds (a too-tall pane would
    // clamp scroll back to 0 and hide the bug) while the window [2, 10) still
    // reaches line 2's row. Top two visible rows are line-1 continuations; line
    // 2's numbered first row is at the bottom.
    let rows = render_rows_scrolled(v, 40, 10, 2);
    // The `a`-run rows are line-1 continuations (blank gutter); the SECONDLINE
    // row is line 2's first row (numbered `2`).
    let a_rows: Vec<&String> = rows.iter().filter(|r| r.contains("aa")).collect();
    assert!(
        a_rows.len() >= 2,
        "two line-1 continuation rows visible at the top, got {a_rows:?}"
    );
    for cont in &a_rows {
        let body_cols = cont.trim_start_matches('│');
        assert!(
            body_cols.starts_with("  a"),
            "continuation row at/after the viewport top has a BLANK gutter (not \
                 a line number), got {cont:?}"
        );
        assert!(
            !body_cols.starts_with("1 "),
            "a continuation row must never show line 1's number, got {cont:?}"
        );
    }
    // Line 2's first row is numbered `2` (the seed didn't break real first rows).
    let second = rows
        .iter()
        .find(|r| r.contains("SECONDLINE"))
        .expect("line 2 visible");
    assert!(
        second.contains("2 SECONDLINE"),
        "line 2's first row is numbered 2, got {second:?}"
    );
}

#[test]
fn unwrapped_panned_line_at_scroll_still_shows_its_number() {
    // Seed-fix guard for the unwrapped path: every unwrapped line is exactly
    // one row per visible_idx, so even with `scroll > 0` AND `h_scroll > 0`
    // (the row's char_start == h_scroll > 0) the top row is still a first row
    // and must carry its number. Ten distinct wide lines; pan right; scroll to
    // line 3 (index 2 → number 3).
    let body: String = (0..10)
        .map(|i| format!("L{i}-{}", "x".repeat(60)))
        .collect::<Vec<_>>()
        .join("\n");
    let mut v = view(&body);
    v.scroll_h(true, 5); // h_scroll > 0
    assert_eq!(v.h_scroll(), 5);
    assert!(!v.wrap());
    // Scroll to display row 2 (0-based) → logical line index 2 → number 3.
    // 10 lines → gutter width 3 (2 digits + pad), so line 3 renders as ` 3 `.
    let rows = render_rows_scrolled(v, 40, 8, 2);
    // The top content row shows number `3` right-aligned in the 3-col gutter.
    let numbered = rows
        .iter()
        .find(|r| r.trim_start_matches('│').starts_with(" 3 "))
        .expect("top unwrapped row numbered 3 despite scroll>0 and h_scroll>0");
    // And it shows the panned window (char_start == 5), so `L3-` is off-screen
    // left; the visible text is the `x` run.
    assert!(
        numbered.contains('x'),
        "the numbered row shows the panned window, got {numbered:?}"
    );
}

#[test]
fn fold_header_shows_the_opener_line_number() {
    // A JSON body folded at the top: the fold-header row carries the opener
    // line's number (line 1), then the `⋯ N lines` suffix.
    let mut v = json_view("{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}");
    assert!(v.line_count() > 1);
    v.toggle_all_folds(); // collapse the top-level object
    let rows = render_rows(v, 60, 10);
    let header = rows
        .iter()
        .find(|r| r.contains('⋯'))
        .expect("a fold-header row");
    assert!(
        header.contains("1 {"),
        "fold-header carries the opener's number (1), got {header:?}"
    );
}

#[test]
fn gutter_shrinks_effective_body_width_for_wrap_and_h_scroll() {
    // The width interaction is the risk area: the gutter must be subtracted
    // from the body width BEFORE wrap/h_scroll geometry. We assert the wrap
    // boundary shifts by exactly the gutter width when it is on vs off.
    let long = "z".repeat(100);
    // Gutter OFF: with an 18-col body, the first wrap row is 18 chars.
    let mut off = view(&long);
    off.toggle_line_numbers();
    off.toggle_wrap();
    let visible = off.visible_map();
    let rows_off = expand_wrap(&off, &visible, true, 18, 0);
    assert_eq!(
        rows_off[0].char_end - rows_off[0].char_start,
        18,
        "gutter off: full 18-col body wrap"
    );
    // Gutter ON: same 20-col area, but render subtracts gutter (2) → 18-col
    // body. We emulate render's seam: body width = area - gutter_width.
    let mut on = view(&long);
    on.toggle_wrap();
    assert!(on.line_numbers());
    let area_width = 20usize;
    let body_width = area_width - on.gutter_width();
    assert_eq!(body_width, 18, "gutter on: body = area(20) - gutter(2)");
    let visible = on.visible_map();
    let rows_on = expand_wrap(&on, &visible, true, body_width, 0);
    assert_eq!(
        rows_on[0].char_end - rows_on[0].char_start,
        18,
        "wrap splits at the REDUCED body width, not the full area"
    );

    // And h_scroll: with the gutter on, max_h_scroll must be computed against
    // the reduced body width. widest visible line = 100.
    let pan = view(&long); // wrap off, gutter on
    assert!(!pan.wrap());
    // If a caller passes the reduced width (as render does), the clamp uses it.
    assert_eq!(pan.gutter_width(), 2);
    let reduced = 30 - pan.gutter_width();
    assert_eq!(pan.max_h_scroll(reduced), 100 - reduced);
}

#[test]
fn copy_stays_byte_exact_with_the_gutter_on() {
    // Regression guard: the gutter is render-only. Copy must be byte-exact even
    // with the gutter on AND a mangled line (ANSI/tab/NUL) that sanitize would
    // corrupt if copy ever read the displayed text.
    let raw = "clean\nmid\u{1B}[31m\tx\u{00}y\ntail";
    let v = view(raw);
    assert!(v.line_numbers(), "gutter on");
    assert_eq!(v.copy_all(), raw, "copy-all is the raw body, gutter or not");
    assert_eq!(
        v.copy_line(1),
        "mid\u{1B}[31m\tx\u{00}y",
        "copy-line is the raw line bytes, unaffected by the gutter"
    );
}

#[test]
fn viewport_width_reported_by_render_excludes_the_gutter() {
    // The reported viewport_width (fed back into cursor→logical mapping) must be
    // the reduced body width. Render a 40-wide pane: inner width = 38, minus the
    // 2-col gutter = 36.
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let state = ResponseState::Done { view: view("a\nb") };
    let cache = HashMap::new();
    let theme = Theme::default();
    let backend = TestBackend::new(40, 8);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut captured = 0usize;
    terminal
        .draw(|frame| {
            let outcome = render(
                frame,
                frame.area(),
                RenderCtx {
                    state: &state,
                    request: None,
                    focused: false,
                    scroll: 0,
                    cursor: 0,
                    cache: &cache,
                    theme: &theme,
                    jump_label: None,
                    tick_count: 0,
                },
            );
            captured = outcome.viewport_width;
        })
        .expect("draw");
    // inner width = 40 - 2 borders = 38; gutter = 2 → body = 36.
    assert_eq!(captured, 36, "viewport_width excludes the gutter");
}

// ---- A3: render-geometry cache ----

#[test]
fn geometry_cache_hits_on_unchanged_signature_and_misses_on_change() {
    // A multi-line body so the geometry is non-trivial to compute.
    let mut v = view("alpha\nbravo\ncharlie\ndelta\necho");

    // Cold: nothing cached yet, so the first expand is a miss.
    assert!(
        !v.rows_cache_hits(false, 20, 0),
        "cold cache: no rows memo yet"
    );
    let first = v.cached_expand_wrap(false, 20, 0);
    // Warm: the identical signature is now a hit — a re-render on an idle tick
    // (same generation/mode/fold/wrap/width/h_scroll) reuses the memo instead of
    // re-walking the body.
    assert!(
        v.rows_cache_hits(false, 20, 0),
        "unchanged signature must hit the rows memo"
    );
    let second = v.cached_expand_wrap(false, 20, 0);
    assert_eq!(first, second, "the cached rows are byte-identical");

    // Changing the width (a resize) is a miss — a different geometry input.
    assert!(
        !v.rows_cache_hits(false, 10, 0),
        "a different width must miss"
    );
    // Changing h_scroll (a horizontal pan) is a miss for the rows memo — the char
    // window each row shows differs.
    let _ = v.cached_expand_wrap(false, 20, 0); // repopulate the (20,0) slot
    assert!(
        !v.rows_cache_hits(false, 20, 4),
        "a different h_scroll must miss the rows memo"
    );

    // Toggling wrap changes the geometry ⇒ miss under the new signature.
    v.toggle_wrap();
    assert!(
        !v.rows_cache_hits(true, 20, 0),
        "wrap toggle changes the signature ⇒ miss"
    );

    // A generation bump (pretty/sort toggle path) invalidates via the signature.
    let mut jv = json_view("{\"b\":1,\"a\":2}");
    let _ = jv.cached_expand_wrap(false, 30, 0);
    assert!(jv.rows_cache_hits(false, 30, 0));
    jv.toggle_sort_keys(); // bumps generation + resets folds/search
    assert!(
        !jv.rows_cache_hits(false, 30, 0),
        "generation bump (sort toggle) must invalidate the memo"
    );
}

#[test]
fn max_h_scroll_cache_survives_horizontal_pan() {
    // The widest-line scan is independent of h_scroll, so a pure pan (viewport-
    // only movement) must NOT invalidate it. We assert the *result* is stable
    // across pans (the memo returns the same widest-line bound), which is the
    // observable contract of caching on a key that omits h_scroll.
    let mut v = wide_line_view(50);
    assert_eq!(v.max_h_scroll(10), 40);
    v.scroll_h(true, 8); // pan the viewport; geometry (widest line) unchanged
    assert_eq!(
        v.max_h_scroll(10),
        40,
        "widest-line bound is stable across a horizontal pan"
    );
}

/// The owner's example body, pretty-reformatted by the view on build (JSON
/// defaults to pretty). Structural navigation walks its collapsible nodes.
fn owner_example_view() -> ResponseView {
    json_view(
        r#"{ "name": "ali",
  "data": { "age": 30, "certs": [1, 2, 3] },
  "history": [ { "date": "12-12-2012" }, { "date": "13-12-2012" } ] }"#,
    )
}

/// The trimmed text of a logical line, for content-based (indent-agnostic)
/// assertions on where a jump landed.
fn landed(v: &ResponseView, logical: usize) -> String {
    v.logical_line(logical).trim().to_owned()
}

#[test]
fn structural_forward_walks_collapsible_nodes() {
    let mut v = owner_example_view();
    // From the root opener (line 0), J visits each collapsible node in
    // pre-order, skipping leaf lines: data → certs → history → each item.
    let mut cur = 0;
    let mut walk = Vec::new();
    while let Some(next) = v.structural_target(cur, true) {
        walk.push(landed(&v, next));
        cur = next;
    }
    assert_eq!(
        walk,
        vec![
            "\"data\": {".to_owned(),
            "\"certs\": [".to_owned(),
            "\"history\": [".to_owned(),
            "{".to_owned(),
            "{".to_owned(),
        ],
    );
    // At the last collapsible node the forward jump is a no-op.
    assert_eq!(v.structural_target(cur, true), None);
}

#[test]
fn structural_backward_climbs_to_enclosing_nodes() {
    let mut v = owner_example_view();
    // A leaf line inside `certs` (the `1` element). K climbs to the nearest
    // preceding/enclosing collapsible: certs → data → root.
    let inside_certs = (0..v.source_line_count())
        .find(|&i| v.logical_line(i).trim() == "1,")
        .expect("the `1` array element");
    let certs = v.structural_target(inside_certs, false).unwrap();
    assert_eq!(landed(&v, certs), "\"certs\": [");
    let data = v.structural_target(certs, false).unwrap();
    assert_eq!(landed(&v, data), "\"data\": {");
    let root = v.structural_target(data, false).unwrap();
    assert_eq!(landed(&v, root), "{");
    // At the root there is nothing further out: backward is a no-op.
    assert_eq!(v.structural_target(root, false), None);
}

#[test]
fn structural_forward_skips_hidden_subtree_of_folded_node() {
    let mut v = owner_example_view();
    let data = v.structural_target(0, true).unwrap();
    assert_eq!(landed(&v, data), "\"data\": {");
    v.toggle_fold_at(data);
    // The folded node's own header stays a valid stop…
    assert_eq!(v.structural_target(0, true), Some(data));
    // …but its hidden subtree (`certs`) is skipped: from data, J lands on
    // `history`, not the elided `certs`.
    let next = v.structural_target(data, true).unwrap();
    assert_eq!(landed(&v, next), "\"history\": [");
}

#[test]
fn structural_navigation_is_none_on_non_json() {
    // A non-JSON body has no collapsible nodes — the handler notifies instead.
    let mut v = view("plain text\nsecond line");
    assert_eq!(v.structural_target(0, true), None);
    assert_eq!(v.structural_target(0, false), None);
}

#[test]
fn structural_navigation_single_node_has_no_stops() {
    // A one-line JSON object with no inner collapsible nodes: the sole opener is
    // the root, so a jump from it (or into it) finds nothing.
    let mut v = json_view("{}");
    assert_eq!(v.structural_target(0, true), None);
    assert_eq!(v.structural_target(0, false), None);
}
