//! Off-thread, viewport-only syntax highlighting.
//!
//! A dedicated `std::thread` owns the `syntect` [`SyntaxSet`] and theme (loaded
//! lazily on first job so the cold-start budget is untouched) and receives
//! [`HighlightJob`]s over a `std::sync::mpsc` channel. Each job carries just the
//! lines of one viewport; the worker highlights them and returns the coloured
//! [`Line`]s to the app as [`AppMsg::Highlighted`]. Highlighting starts stateless
//! at the top of every viewport — a known imperfection for multi-line constructs
//! (block comments/strings that begin above the viewport), accepted in M3 for the
//! render-loop budget it buys.

use std::sync::mpsc::{Receiver, Sender};

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::Theme;
use syntect::parsing::{SyntaxReference, SyntaxSet};
use tokio::sync::mpsc::UnboundedSender;
use two_face::theme::EmbeddedThemeName;

use super::app::AppMsg;

/// The syntax family a response body is highlighted as, derived from its
/// `Content-Type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SyntaxToken {
    /// `application/json` and friends.
    Json,
    /// `application/xml`, `text/xml`.
    Xml,
    /// `text/html`.
    Html,
    /// Anything else — highlighted as plain text.
    Plain,
}

impl SyntaxToken {
    /// Derives a token from a response `Content-Type` header value.
    pub fn from_content_type(content_type: Option<&str>) -> Self {
        let content_type = content_type.unwrap_or_default().to_ascii_lowercase();
        if content_type.contains("json") {
            Self::Json
        } else if content_type.contains("html") {
            Self::Html
        } else if content_type.contains("xml") {
            Self::Xml
        } else {
            Self::Plain
        }
    }
}

/// One unit of highlighting work: the lines of a single viewport, tagged with the
/// viewport hash they belong to so a late result can be matched to (or discarded
/// against) the current view.
#[derive(Debug)]
pub struct HighlightJob {
    /// Viewport hash this job highlights (the cache key).
    pub hash: u64,
    /// Syntax family to highlight as.
    pub syntax: SyntaxToken,
    /// The viewport's lines (without trailing newlines).
    pub lines: Vec<String>,
}

/// Spawns the highlight worker thread and returns the job sender. `light`
/// selects the embedded syntect theme (Nord for dark, InspiredGithub for light)
/// so response bodies match the pane palette. Runs until the sender drops or the
/// app channel closes.
pub fn spawn(app_tx: UnboundedSender<AppMsg>, light: bool) -> Sender<HighlightJob> {
    let (job_tx, job_rx) = std::sync::mpsc::channel::<HighlightJob>();
    std::thread::Builder::new()
        .name("churl-highlight".to_owned())
        .spawn(move || worker(job_rx, app_tx, light))
        .expect("spawn highlight worker thread");
    job_tx
}

/// The worker loop: lazily loads the syntax/theme sets, then highlights each job.
fn worker(jobs: Receiver<HighlightJob>, app_tx: UnboundedSender<AppMsg>, light: bool) {
    // Lazy load inside the thread so cold start never pays for it.
    let syntax_set = two_face::syntax::extra_newlines();
    let theme_name = if light {
        EmbeddedThemeName::InspiredGithub
    } else {
        EmbeddedThemeName::Nord
    };
    let theme = two_face::theme::extra().get(theme_name).clone();
    for job in jobs {
        let lines = highlight(&syntax_set, &theme, job.syntax, &job.lines);
        if app_tx
            .send(AppMsg::Highlighted {
                hash: job.hash,
                lines,
            })
            .is_err()
        {
            break; // the app is gone
        }
    }
}

/// Highlights `lines` with `HighlightLines`, converting syntect styles to ratatui
/// (RGB foreground only — background is skipped so the theme does not fight the
/// pane).
fn highlight(
    syntax_set: &SyntaxSet,
    theme: &Theme,
    token: SyntaxToken,
    lines: &[String],
) -> Vec<Line<'static>> {
    let syntax = syntax_for(syntax_set, token);
    let mut highlighter = HighlightLines::new(syntax, theme);
    lines
        .iter()
        .map(|line| match highlighter.highlight_line(line, syntax_set) {
            Ok(ranges) => Line::from(
                ranges
                    .iter()
                    .map(|(style, text)| {
                        Span::styled(
                            (*text).to_owned(),
                            Style::default().fg(Color::Rgb(
                                style.foreground.r,
                                style.foreground.g,
                                style.foreground.b,
                            )),
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
            Err(_) => Line::from(line.clone()),
        })
        .collect()
}

/// Resolves the syntect syntax for a token, falling back to plain text.
fn syntax_for(syntax_set: &SyntaxSet, token: SyntaxToken) -> &SyntaxReference {
    let extension = match token {
        SyntaxToken::Json => "json",
        SyntaxToken::Xml => "xml",
        SyntaxToken::Html => "html",
        SyntaxToken::Plain => return syntax_set.find_syntax_plain_text(),
    };
    syntax_set
        .find_syntax_by_extension(extension)
        .unwrap_or_else(|| syntax_set.find_syntax_plain_text())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_type_maps_to_syntax_token() {
        assert_eq!(
            SyntaxToken::from_content_type(Some("application/json; charset=utf-8")),
            SyntaxToken::Json
        );
        assert_eq!(
            SyntaxToken::from_content_type(Some("text/html")),
            SyntaxToken::Html
        );
        assert_eq!(
            SyntaxToken::from_content_type(Some("application/xml")),
            SyntaxToken::Xml
        );
        assert_eq!(
            SyntaxToken::from_content_type(Some("text/plain")),
            SyntaxToken::Plain
        );
        assert_eq!(SyntaxToken::from_content_type(None), SyntaxToken::Plain);
    }

    #[test]
    fn highlights_json_into_spans() {
        let syntax_set = two_face::syntax::extra_newlines();
        let theme = two_face::theme::extra()
            .get(EmbeddedThemeName::Nord)
            .clone();
        let lines = vec![r#"{"a": 1}"#.to_owned()];
        let highlighted = highlight(&syntax_set, &theme, SyntaxToken::Json, &lines);
        assert_eq!(highlighted.len(), 1);
        // A highlighted JSON line splits into more than one styled span.
        assert!(highlighted[0].spans.len() > 1);
    }
}
