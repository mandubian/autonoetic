//! Fuzzy find-and-replace for targeted content edits (`content_patch`).
//!
//! A model-authored `old_string` frequently differs trivially from the stored
//! bytes — trailing whitespace, re-indentation, collapsed spacing. Rather than
//! force the agent to re-emit the whole file, we widen the match tolerance in
//! ordered strategies and stop at the first that yields a *unique* match:
//!
//!   1. `Exact`                — literal substring.
//!   2. `LineTrimmed`          — lines equal after trimming each end.
//!   3. `WhitespaceNormalized` — lines equal after collapsing internal runs.
//!
//! The two line-based strategies also **re-indent** the replacement to the
//! matched region's base indentation, so an edit authored at the wrong indent
//! still lands correctly (indentation handling is folded into replacement
//! rather than a separate strategy). Exact never re-indents — it matched
//! verbatim.
//!
//! Uniqueness is enforced: a non-`replace_all` edit that matches more than once
//! is an error, never a guess. `replace_all` is honored for `Exact` only —
//! replacing every fuzzy match is too dangerous.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchStrategy {
    Exact,
    LineTrimmed,
    WhitespaceNormalized,
}

impl MatchStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            MatchStrategy::Exact => "exact",
            MatchStrategy::LineTrimmed => "line-trimmed",
            MatchStrategy::WhitespaceNormalized => "whitespace-normalized",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FuzzyError {
    /// `old_string` was empty — nothing to anchor on.
    EmptyPattern,
    /// No strategy located the snippet.
    NotFound,
    /// A strategy matched more than once and `replace_all` was not set.
    Ambiguous { strategy: MatchStrategy, count: usize },
}

impl fmt::Display for FuzzyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FuzzyError::EmptyPattern => write!(f, "old_string is empty"),
            FuzzyError::NotFound => write!(f, "old_string not found in content"),
            FuzzyError::Ambiguous { strategy, count } => {
                // replace_all only applies to the exact strategy; don't suggest
                // it for line-based matches, where it can't help.
                let tail = if matches!(strategy, MatchStrategy::Exact) {
                    "; pass a longer, more unique snippet or set replace_all"
                } else {
                    "; pass a longer, more unique snippet with surrounding context lines"
                };
                write!(
                    f,
                    "old_string matched {count} times ({} strategy){tail}",
                    strategy.as_str()
                )
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReplaceOutcome {
    pub content: String,
    pub strategy: MatchStrategy,
    pub replacements: usize,
}

/// Find `old` in `source` and substitute `new`, widening tolerance per strategy.
pub fn find_and_replace(
    source: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<ReplaceOutcome, FuzzyError> {
    if old.is_empty() {
        return Err(FuzzyError::EmptyPattern);
    }

    // --- Strategy 1: exact substring ---
    let exact_count = source.matches(old).count();
    if exact_count > 0 {
        if replace_all {
            return Ok(ReplaceOutcome {
                content: source.replace(old, new),
                strategy: MatchStrategy::Exact,
                replacements: exact_count,
            });
        }
        if exact_count == 1 {
            return Ok(ReplaceOutcome {
                content: source.replacen(old, new, 1),
                strategy: MatchStrategy::Exact,
                replacements: 1,
            });
        }
        // Multiple literal matches without replace_all is a clear ambiguity —
        // surface it rather than falling through to fuzzier strategies.
        return Err(FuzzyError::Ambiguous {
            strategy: MatchStrategy::Exact,
            count: exact_count,
        });
    }

    // --- Line-based strategies ---
    let src_lines = line_offsets(source);
    let old_trimmed = old.strip_suffix('\n').unwrap_or(old);
    let pat_lines: Vec<&str> = old_trimmed.split('\n').collect();

    for strategy in [MatchStrategy::LineTrimmed, MatchStrategy::WhitespaceNormalized] {
        let windows = find_line_windows(&src_lines, &pat_lines, strategy);
        match windows.len() {
            0 => continue,
            1 => {
                let content = apply_line_replacement(source, &src_lines, &pat_lines, windows[0], new);
                return Ok(ReplaceOutcome {
                    content,
                    strategy,
                    replacements: 1,
                });
            }
            count => {
                return Err(FuzzyError::Ambiguous { strategy, count });
            }
        }
    }

    Err(FuzzyError::NotFound)
}

/// Byte offset + content (without trailing newline) for each line of `src`.
fn line_offsets(src: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, c) in src.char_indices() {
        if c == '\n' {
            lines.push((start, &src[start..i]));
            start = i + 1;
        }
    }
    lines.push((start, &src[start..]));
    lines
}

fn line_eq(a: &str, b: &str, strategy: MatchStrategy) -> bool {
    match strategy {
        MatchStrategy::Exact => a == b,
        MatchStrategy::LineTrimmed => a.trim() == b.trim(),
        MatchStrategy::WhitespaceNormalized => normalize_ws(a) == normalize_ws(b),
    }
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Start indices of every window of `src_lines` matching all `pat_lines` under `strategy`.
fn find_line_windows(
    src_lines: &[(usize, &str)],
    pat_lines: &[&str],
    strategy: MatchStrategy,
) -> Vec<usize> {
    let n = pat_lines.len();
    if n == 0 || n > src_lines.len() {
        return Vec::new();
    }
    let mut hits = Vec::new();
    for w in 0..=(src_lines.len() - n) {
        if (0..n).all(|k| line_eq(src_lines[w + k].1, pat_lines[k], strategy)) {
            hits.push(w);
        }
    }
    hits
}

/// Splice `new` into `source` over the matched line window, re-indenting `new`
/// from the pattern's base indent to the matched region's base indent.
fn apply_line_replacement(
    source: &str,
    src_lines: &[(usize, &str)],
    pat_lines: &[&str],
    w: usize,
    new: &str,
) -> String {
    let n = pat_lines.len();
    let start_byte = src_lines[w].0;
    let last = w + n - 1;
    let end_byte = src_lines[last].0 + src_lines[last].1.len();

    let src_base = leading_ws(src_lines[w].1);
    let old_base = leading_ws(pat_lines[0]);
    let new_trimmed = new.strip_suffix('\n').unwrap_or(new);
    let reindented = reindent(new_trimmed, old_base, src_base);

    format!("{}{}{}", &source[..start_byte], reindented, &source[end_byte..])
}

/// Leading run of spaces/tabs on a line.
fn leading_ws(line: &str) -> &str {
    let end = line.len() - line.trim_start_matches([' ', '\t']).len();
    &line[..end]
}

/// Re-base each line's indentation from `old_base` to `src_base`.
fn reindent(new: &str, old_base: &str, src_base: &str) -> String {
    if old_base == src_base {
        return new.to_string();
    }
    new.split('\n')
        .map(|line| match line.strip_prefix(old_base) {
            Some(rest) => format!("{src_base}{rest}"),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_single_match() {
        let out = find_and_replace("let x = 1;\nlet y = 2;\n", "let x = 1;", "let x = 42;", false).unwrap();
        assert_eq!(out.strategy, MatchStrategy::Exact);
        assert_eq!(out.replacements, 1);
        assert_eq!(out.content, "let x = 42;\nlet y = 2;\n");
    }

    #[test]
    fn exact_ambiguous_without_replace_all() {
        let err = find_and_replace("a\na\n", "a", "b", false).unwrap_err();
        assert!(matches!(err, FuzzyError::Ambiguous { strategy: MatchStrategy::Exact, count: 2 }));
    }

    #[test]
    fn exact_replace_all() {
        let out = find_and_replace("a\na\n", "a", "b", true).unwrap();
        assert_eq!(out.replacements, 2);
        assert_eq!(out.content, "b\nb\n");
    }

    #[test]
    fn empty_pattern_errors() {
        assert_eq!(find_and_replace("x", "", "y", false).unwrap_err(), FuzzyError::EmptyPattern);
    }

    #[test]
    fn not_found() {
        assert_eq!(find_and_replace("hello\n", "world", "x", false).unwrap_err(), FuzzyError::NotFound);
    }

    #[test]
    fn line_trimmed_matches_trailing_whitespace_drift() {
        // Source has trailing spaces the model's old_string lacks.
        let src = "fn main() {  \n    println!(\"hi\");  \n}\n";
        let out = find_and_replace(
            src,
            "fn main() {\n    println!(\"hi\");\n}",
            "fn main() {\n    println!(\"bye\");\n}",
            false,
        )
        .unwrap();
        assert_eq!(out.strategy, MatchStrategy::LineTrimmed);
        assert!(out.content.contains("println!(\"bye\");"));
        assert!(!out.content.contains("\"hi\""));
    }

    #[test]
    fn line_trimmed_reindents_replacement_to_source() {
        // Source block is indented 8 spaces; model authored old/new at zero indent.
        let src = "        foo();\n        bar();\n";
        let out = find_and_replace(src, "foo();\nbar();", "foo();\nbaz();", false).unwrap();
        // baz line must inherit the source's 8-space indent.
        assert!(out.content.contains("        baz();"), "got: {:?}", out.content);
        assert!(!out.content.contains("\nbaz();"));
    }

    #[test]
    fn whitespace_normalized_matches_collapsed_spacing() {
        let src = "let    x   =    1;\n";
        let out = find_and_replace(src, "let x = 1;", "let x = 2;", false).unwrap();
        assert_eq!(out.strategy, MatchStrategy::WhitespaceNormalized);
        assert!(out.content.contains("let x = 2;"));
    }

    #[test]
    fn fuzzy_ambiguous_is_error() {
        let src = "  a();\n  a();\n";
        let err = find_and_replace(src, "a();", "b();", false).unwrap_err();
        // Exact already matches twice here → ambiguous on exact.
        assert!(matches!(err, FuzzyError::Ambiguous { .. }));
    }

    #[test]
    fn preserves_surrounding_content() {
        let src = "header\n    target line\nfooter\n";
        let out = find_and_replace(src, "target line", "new line", false).unwrap();
        assert!(out.content.starts_with("header\n"));
        assert!(out.content.ends_with("footer\n"));
        assert!(out.content.contains("    new line"));
    }
}
