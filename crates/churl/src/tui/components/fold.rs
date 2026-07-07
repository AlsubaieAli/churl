//! JSON fold-region scanning for the response viewer.
//!
//! A lightweight, string-aware scanner over a JSON body computes the foldable
//! regions **once per response** (see [`scan_regions`]): every `{`/`[` opener
//! line paired with its matching `}`/`]` closer line, when the two differ. It
//! tracks in-string/escape state so braces inside string literals never open a
//! region, and it never panics on unbalanced or truncated input — a JSON body
//! cut off at the size cap simply yields fewer closed regions.
//!
//! The scanner is intentionally decoupled from `ResponseView`: it takes the line
//! index (byte offsets) and the backing text and returns `(opener, closer)` line
//! pairs. Folding itself (which regions are collapsed, and how the visible line
//! map is derived) lives in `response.rs`.

/// A foldable region: the opener line and its matching closer line (both
/// zero-based logical line indices), guaranteed `opener < closer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldRegion {
    /// The line carrying the opening `{` or `[`.
    pub opener: usize,
    /// The line carrying the matching `}` or `]`.
    pub closer: usize,
}

impl FoldRegion {
    /// The number of inner lines hidden when this region is folded (everything
    /// between opener and closer, inclusive of the closer line).
    pub fn hidden_count(self) -> usize {
        self.closer - self.opener
    }
}

/// Scans `text` for JSON fold regions, using `line_offsets` (byte offset of each
/// line start) to map byte positions back to line indices. Returns regions in
/// the order their openers close (innermost-first is *not* guaranteed; callers
/// that need innermost-at-cursor filter by containment). Unbalanced closers are
/// ignored; unclosed openers at EOF yield no region.
pub fn scan_regions(text: &str, line_offsets: &[usize]) -> Vec<FoldRegion> {
    let mut regions = Vec::new();
    // Stack of (opener line, opener byte) for each unmatched opener.
    let mut stack: Vec<(usize, u8)> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    let bytes = text.as_bytes();
    let mut line = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        // Advance the current line index as we cross newlines. `line_offsets`
        // has the start of each line; a byte at or past the next offset is on
        // the next line.
        while line + 1 < line_offsets.len() && i >= line_offsets[line + 1] {
            line += 1;
        }
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' | b'[' => stack.push((line, b)),
            b'}' | b']' => {
                // A closer only forms a region with a *matching* opener kind
                // (`}`↔`{`, `]`↔`[`); on a mismatch the opener is dropped
                // without a region — malformed input never folds wrongly.
                let expected = if b == b'}' { b'{' } else { b'[' };
                if let Some((opener, kind)) = stack.pop()
                    && kind == expected
                    && opener < line
                {
                    regions.push(FoldRegion {
                        opener,
                        closer: line,
                    });
                }
            }
            _ => {}
        }
    }
    regions
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the line-offset index the way `ResponseView::build` does.
    fn offsets(text: &str) -> Vec<usize> {
        let mut v = Vec::new();
        if !text.is_empty() {
            v.push(0);
            for (i, b) in text.bytes().enumerate() {
                if b == b'\n' {
                    v.push(i + 1);
                }
            }
        }
        v
    }

    fn scan(text: &str) -> Vec<FoldRegion> {
        scan_regions(text, &offsets(text))
    }

    #[test]
    fn single_object_folds_opener_to_closer() {
        let text = "{\n  \"a\": 1\n}";
        let regions = scan(text);
        assert_eq!(regions.len(), 1);
        assert_eq!(
            regions[0],
            FoldRegion {
                opener: 0,
                closer: 2
            }
        );
        assert_eq!(regions[0].hidden_count(), 2);
    }

    #[test]
    fn nested_regions_both_reported() {
        let text = "{\n  \"a\": {\n    \"b\": 1\n  }\n}";
        let mut regions = scan(text);
        regions.sort_by_key(|r| r.opener);
        assert_eq!(regions.len(), 2);
        // Outer object 0..4, inner object 1..3.
        assert_eq!(
            regions[0],
            FoldRegion {
                opener: 0,
                closer: 4
            }
        );
        assert_eq!(
            regions[1],
            FoldRegion {
                opener: 1,
                closer: 3
            }
        );
    }

    #[test]
    fn arrays_fold_too() {
        let text = "[\n  1,\n  2\n]";
        let regions = scan(text);
        assert_eq!(
            regions,
            vec![FoldRegion {
                opener: 0,
                closer: 3
            }]
        );
    }

    #[test]
    fn single_line_object_yields_no_region() {
        // opener == closer → not foldable.
        assert!(scan("{\"a\": 1}").is_empty());
    }

    #[test]
    fn braces_inside_strings_are_ignored() {
        let text = "{\n  \"s\": \"a { b [ c\",\n  \"t\": \"} ]\"\n}";
        let regions = scan(text);
        // Only the real outer object folds; the string braces are inert.
        assert_eq!(
            regions,
            vec![FoldRegion {
                opener: 0,
                closer: 3
            }]
        );
    }

    #[test]
    fn escaped_quote_in_string_does_not_end_it() {
        let text = "{\n  \"s\": \"a\\\"{\",\n  \"t\": 1\n}";
        let regions = scan(text);
        assert_eq!(
            regions,
            vec![FoldRegion {
                opener: 0,
                closer: 3
            }]
        );
    }

    #[test]
    fn unbalanced_truncated_json_does_not_panic() {
        // Truncated at the cap: openers never close.
        let text = "{\n  \"a\": [\n    1,\n    2";
        let regions = scan(text);
        // No closers → no regions, and crucially no panic.
        assert!(regions.is_empty());
    }

    #[test]
    fn mismatched_bracket_kinds_form_no_region() {
        // `[` closed by `}`: the opener is dropped, no region.
        assert!(scan("[\n1,\n2\n}").is_empty());
        // `{` closed by `]`: likewise.
        assert!(scan("{\n1\n]").is_empty());
        // A well-formed sibling still folds despite an earlier mismatch.
        let text = "[\n1\n}\n{\n2\n}";
        let regions = scan(text);
        assert_eq!(
            regions,
            vec![FoldRegion {
                opener: 3,
                closer: 5
            }]
        );
    }

    #[test]
    fn extra_closers_do_not_panic() {
        let text = "}\n]\n}";
        assert!(scan(text).is_empty());
    }

    #[test]
    fn empty_text_yields_no_regions() {
        assert!(scan("").is_empty());
    }
}
