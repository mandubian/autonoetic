use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use std::sync::LazyLock;

/// Internal markers emitted by [`super::render::format_detail`] for narrative
/// payload fields — consumed by the detail pane markdown renderer.
pub(crate) const NARRATIVE_MD_START: &str = "@@NARRATIVE@@";
pub(crate) const NARRATIVE_MD_END: &str = "@@/NARRATIVE@@";

static INLINE_SECTION_LABEL: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"([.!?\)])([ \t]*)((?:What|My|How|Why|When|Where|Who)\s+[^:\n]{2,50}:)",
    )
    .expect("inline section label regex")
});

/// `:Root CauseThe` → `### Root Cause` before the next sentence.
static INLINE_COLON_TITLE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r":([ \t]*)([A-Z][A-Za-z'&]+(?: [A-Z][A-Za-z'&]+)*)([A-Z][a-z][a-z]*)",
    )
    .expect("inline colon title regex")
});

/// Known section titles glued to prior prose (`…ingWhat's HappeningThe`).
static GLUED_KNOWN_TITLES: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"(?x)
        ([a-z0-9\).|])
        (Root\ Cause|What's\ Happening|Fix\ Options|Evidence|Summary|
         Recommendation|Primary\ Cause|Agent\ Overview|Diagnosis)
        ([A-Z][a-z][a-z]*|\| )
        ",
    )
    .expect("glued known titles regex")
});

/// Start a markdown table row on its own line when glued to prose.
static GLUED_TABLE_ROW: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"([^\n\|])(\s*\|[^|\n]+\|[^|\n]+\|)").expect("glued table row regex")
});

/// Detect prose that should be rendered with the markdown pipeline (headings,
/// lists, fenced code, inline `` ` `` markers, or python-ish blocks).
pub fn looks_like_narrative_content(s: &str) -> bool {
    looks_like_markdown(s) || looks_like_code(s)
}

/// Detect unfenced or fenced source code (python-heavy heuristics).
pub fn looks_like_code(s: &str) -> bool {
    if s.contains("```") {
        return true;
    }
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return false;
    }
    let signals = lines
        .iter()
        .filter(|l| line_looks_like_code(l))
        .count();
    signals >= 2 || (lines.len() >= 3 && signals >= 1)
}

fn line_looks_like_code(line: &str) -> bool {
    let t = line.trim();
    t.starts_with("def ")
        || t.starts_with("import ")
        || t.starts_with("from ")
        || t.starts_with("class ")
        || t.starts_with("@")
        || t.starts_with("if __name__")
        || t.starts_with("#!")
        || t.starts_with(">>> ")
        || t.starts_with("return ")
        || t.starts_with("raise ")
        || (t.starts_with("    ") || t.starts_with('\t'))
            && (t.contains('=') || t.contains('(') || t.contains(':'))
        || t.ends_with(':')
            && (t.starts_with("if ")
                || t.starts_with("elif ")
                || t.starts_with("else")
                || t.starts_with("for ")
                || t.starts_with("while ")
                || t.starts_with("try")
                || t.starts_with("except ")
                || t.starts_with("with ")
                || t.starts_with("async def "))
}

/// Wrap unfenced python-ish blocks in ` ```python ` fences so pulldown renders them.
pub(crate) fn normalize_fenced_code(input: &str) -> String {
    if input.contains("```") {
        return input.to_string();
    }
    let parts: Vec<&str> = input.split("\n\n").collect();
    let mut out = Vec::new();
    for part in parts {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            out.push(String::new());
            continue;
        }
        if looks_like_code(trimmed) {
            out.push(format!("```python\n{trimmed}\n```"));
        } else {
            out.push(part.to_string());
        }
    }
    out.join("\n\n")
}

/// Detect whether a string looks like it contains markdown formatting
/// (headers, bold, italic, code fences, lists, links, Unicode rules).
pub fn looks_like_markdown(s: &str) -> bool {
    for line in s.lines().take(20) {
        let trimmed = line.trim();
        if trimmed.starts_with("# ")
            || trimmed.starts_with("## ")
            || trimmed.starts_with("### ")
            || trimmed.starts_with("#### ")
            || trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("> ")
            || trimmed.starts_with("```")
            || trimmed.contains("**")
            || trimmed.contains('`')
            || trimmed.starts_with("| ")
                && trimmed.contains(" |")
            || trimmed.starts_with(|c: char| c.is_ascii_digit())
                && trimmed.contains(". ")
        {
            return true;
        }
        // Unicode horizontal rule (──, ═══, etc.)
        if trimmed.len() >= 4
            && trimmed
                .chars()
                .all(|c| matches!(c, '\u{2500}' | '\u{2501}' | '\u{2550}' | '\u{2594}' | '\u{2014}' | '\u{2013}' | '─' | '━' | '═'))
        {
            return true;
        }
        // Short section labels: "What it does:" / "**Input:**"
        if trimmed.len() <= 64
            && trimmed.ends_with(':')
            && trimmed
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_uppercase() || c == '*')
        {
            return true;
        }
    }
    false
}

/// Strip markdown formatting to produce a plain-text summary suitable for
/// single-line display (the `↳` preview or headline).
pub fn strip_markdown(input: &str) -> String {
    let opts = Options::empty();
    let parser = Parser::new_ext(input, opts);
    let mut out = String::new();
    for event in parser {
        match event {
            Event::Text(t) | Event::Code(t) => out.push_str(&t),
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            Event::Start(Tag::Link { .. }) => out.push('['),
            Event::End(TagEnd::Link) => out.push(')'),
            _ => {}
        }
    }
    // Collapse whitespace
    let collapsed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
}

/// Break run-on section labels glued to preceding prose (`agents.What I do:Plan`).
pub(crate) fn normalize_inline_section_labels(input: &str) -> String {
    let mut out = INLINE_SECTION_LABEL
        .replace_all(input, "$1\n\n$2$3")
        .into_owned();
    out = INLINE_COLON_TITLE
        .replace_all(&out, ":\n\n### $2\n\n$3")
        .into_owned();
    out = GLUED_KNOWN_TITLES
        .replace_all(&out, "$1\n\n### $2\n\n$3")
        .into_owned();
    out = GLUED_TABLE_ROW.replace_all(&out, "$1\n$2").into_owned();
    out
}

/// Full narrative normalization for list/detail panes: inline section breaks,
/// Unicode rule conversion, unfenced code fences, then promote standalone
/// `Label:` lines to headings.
pub(crate) fn normalize_narrative_prose(input: &str) -> String {
    let s = normalize_unicode_rules(input);
    normalize_prose_sections(&normalize_fenced_code(&normalize_inline_section_labels(
        &s,
    )))
}

/// Convert Unicode box-drawing horizontal rules (runs of `─`, `━`, `═`, `▔`,
/// or `─`-like chars ≥ 4 long) to markdown `---` so pulldown-cmark renders
/// them as thematic breaks.
fn normalize_unicode_rules(input: &str) -> String {
    static RULE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?m)^\s*[\u2500\u2501\u2502\u2503\u2504\u2505\u2506\u2507\u2508\u2509\u250a\u250b\u254c\u254d═▔▬─]{4,}\s*$")
            .expect("unicode rule regex")
    });
    RULE.replace_all(input, "---").into_owned()
}

/// Promote plain section labels (`What it does:`) to markdown headings when the
/// body has no `#` headers yet — common in operator-facing planner summaries.
pub(crate) fn normalize_prose_sections(input: &str) -> String {
    if input.contains("\n# ")
        || input.contains("\n## ")
        || input.contains("\n### ")
        || input.starts_with('#')
    {
        return input.to_string();
    }
    input
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                String::new()
            } else if trimmed.len() <= 64
                && trimmed.ends_with(':')
                && !trimmed.starts_with('-')
                && !trimmed.starts_with('*')
                && trimmed.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            {
                format!("### {}", trimmed.trim_end_matches(':'))
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Render markdown input into styled ratatui `Line`s for list/detail panes.
pub fn render_markdown(input: &str) -> Vec<Line<'static>> {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(input, opts);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut in_heading: u8 = 0;
    let mut in_bold = false;
    let mut in_italic = false;
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut list_depth: usize = 0;

    // Table collection state
    let mut table_alignments: Vec<Alignment> = Vec::new();
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut in_table = false;
    let mut in_table_cell = false;
    let mut current_cell: String = String::new();
    let mut current_row: Vec<String> = Vec::new();

    for event in parser {
        match event {
            // ── Table events ──
            Event::Start(Tag::Table(aligns)) => {
                flush_line(&mut current_spans, &mut lines);
                in_table = true;
                table_alignments = aligns;
                table_rows.clear();
            }
            Event::End(TagEnd::Table) => {
                flush_line(&mut current_spans, &mut lines);
                lines.extend(render_table(&table_rows, &table_alignments));
                lines.push(Line::raw(""));
                in_table = false;
                table_rows.clear();
                table_alignments.clear();
            }
            Event::Start(Tag::TableHead) | Event::Start(Tag::TableRow) => {
                current_row.clear();
            }
            Event::End(TagEnd::TableHead) | Event::End(TagEnd::TableRow) => {
                table_rows.push(std::mem::take(&mut current_row));
            }
            Event::Start(Tag::TableCell) => {
                in_table_cell = true;
                current_cell.clear();
            }
            Event::End(TagEnd::TableCell) => {
                in_table_cell = false;
                current_row.push(std::mem::take(&mut current_cell));
            }
            Event::Start(Tag::Heading { level, .. }) => {
                flush_line(&mut current_spans, &mut lines);
                in_heading = level as u8;
            }
            Event::End(TagEnd::Heading(_)) => {
                flush_line(&mut current_spans, &mut lines);
                lines.push(Line::raw(""));
                in_heading = 0;
            }
            Event::Start(Tag::Paragraph) => {
                flush_line(&mut current_spans, &mut lines);
            }
            Event::End(TagEnd::Paragraph) => {
                flush_line(&mut current_spans, &mut lines);
                if !in_table {
                    lines.push(Line::raw(""));
                }
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                flush_line(&mut current_spans, &mut lines);
                code_lang = match kind {
                    CodeBlockKind::Fenced(info) => info.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                if !code_lang.is_empty() {
                    lines.push(Line::from(Span::styled(
                        format!("── {} ──", code_lang.trim()),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::DIM),
                    )));
                }
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                flush_line(&mut current_spans, &mut lines);
                in_code_block = false;
                code_lang.clear();
                lines.push(Line::raw(""));
            }
            Event::Start(Tag::List(_)) => {
                list_depth += 1;
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
            }
            Event::Start(Tag::Item) => {
                flush_line(&mut current_spans, &mut lines);
                let indent = "  ".repeat(list_depth.saturating_sub(1));
                current_spans.push(Span::styled(
                    format!("{indent}- "),
                    Style::default().fg(Color::Yellow),
                ));
            }
            Event::End(TagEnd::Item) => {
                flush_line(&mut current_spans, &mut lines);
            }
            Event::Start(Tag::Strong) => in_bold = true,
            Event::End(TagEnd::Strong) => in_bold = false,
            Event::Start(Tag::Emphasis) => in_italic = true,
            Event::End(TagEnd::Emphasis) => in_italic = false,
            Event::Text(t) => {
                if in_table_cell {
                    current_cell.push_str(&t);
                } else {
                    let style = compute_style(in_heading, in_bold, in_italic, in_code_block, &code_lang);
                    if in_code_block {
                        for line_text in t.lines() {
                            current_spans.push(Span::styled(line_text.to_string(), style));
                            flush_line(&mut current_spans, &mut lines);
                        }
                    } else {
                        current_spans.push(Span::styled(t.to_string(), style));
                    }
                }
            }
            Event::Code(t) => {
                if in_table_cell {
                    current_cell.push_str(&t);
                } else {
                    let style = Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD);
                    current_spans.push(Span::styled(t.to_string(), style));
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_table_cell {
                    current_cell.push(' ');
                } else {
                    flush_line(&mut current_spans, &mut lines);
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                current_spans.push(Span::styled(
                    dest_url.to_string(),
                    Style::default().fg(Color::Blue).add_modifier(Modifier::UNDERLINED),
                ));
            }
            Event::End(TagEnd::Link) => {}
            Event::Rule => {
                flush_line(&mut current_spans, &mut lines);
                lines.push(Line::from(Span::styled(
                    "─".repeat(32),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            _ => {}
        }
    }
    flush_line(&mut current_spans, &mut lines);
    // Remove trailing empty lines
    while lines.last().map_or(false, |l| l.to_string().trim().is_empty()) {
        lines.pop();
    }
    lines
}

/// Render collected table rows as aligned, styled ratatui lines.
/// The first row is the header (bold + cyan), followed by a separator,
/// then data rows.
fn render_table(rows: &[Vec<String>], alignments: &[Alignment]) -> Vec<Line<'static>> {
    if rows.is_empty() {
        return Vec::new();
    }
    let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if ncols == 0 {
        return Vec::new();
    }

    // Calculate column widths (character count).
    let mut col_widths = vec![0usize; ncols];
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            col_widths[i] = col_widths[i].max(cell.chars().count());
        }
    }

    let mut out = Vec::new();

    for (row_idx, row) in rows.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let is_header = row_idx == 0;

        for (col_idx, cell) in row.iter().enumerate() {
            if col_idx > 0 {
                spans.push(Span::raw("  "));
            }
            let align = alignments.get(col_idx).copied().unwrap_or(Alignment::None);
            let cell_width = cell.chars().count();
            let target_w = col_widths[col_idx];
            let (left_pad, right_pad) = match align {
                Alignment::Center => {
                    let total = target_w.saturating_sub(cell_width);
                    (total / 2, total - total / 2)
                }
                Alignment::Right => (target_w.saturating_sub(cell_width), 0),
                _ => (0, target_w.saturating_sub(cell_width)),
            };
            if left_pad > 0 {
                spans.push(Span::raw(" ".repeat(left_pad)));
            }
            let style = if is_header {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            spans.push(Span::styled(cell.clone(), style));
            if right_pad > 0 {
                spans.push(Span::raw(" ".repeat(right_pad)));
            }
        }
        out.push(Line::from(spans));

        // Separator after header row
        if is_header {
            let sep: String = col_widths
                .iter()
                .enumerate()
                .map(|(i, &w)| {
                    let bar = "─".repeat(w);
                    if i + 1 < ncols {
                        format!("{bar}  ")
                    } else {
                        bar
                    }
                })
                .collect();
            out.push(Line::from(Span::styled(
                sep,
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    out
}

/// True when a rendered line belongs to a fenced/indented code block.
pub fn line_is_code_block(line: &Line) -> bool {
    line.spans.iter().any(|s| s.style.bg == Some(Color::Black))
}

fn compute_style(
    heading: u8,
    bold: bool,
    italic: bool,
    code_block: bool,
    code_lang: &str,
) -> Style {
    let mut style = Style::default();
    if code_block {
        let fg = if code_lang.eq_ignore_ascii_case("python") || code_lang.eq_ignore_ascii_case("py")
        {
            Color::Green
        } else {
            Color::Gray
        };
        return style.fg(fg).bg(Color::Black).add_modifier(Modifier::DIM);
    }
    if heading > 0 {
        style = style
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
    }
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    style
}

fn flush_line(spans: &mut Vec<Span<'static>>, lines: &mut Vec<Line<'static>>) {
    if !spans.is_empty() {
        lines.push(Line::from(spans.drain(..).collect::<Vec<_>>()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_markdown_preserves_heading_and_list_structure() {
        let md = "## What it does\n\nA stateless script.\n\n- input: JSON\n- output: JSON";
        let lines = render_markdown(md);
        let text = lines
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("What it does"));
        assert!(text.contains("stateless"));
        assert!(text.contains("input"));
    }

    #[test]
    fn normalize_prose_sections_promotes_labels_to_headings() {
        let out = normalize_prose_sections("What it does:\n\nSome prose.\n\nHow it works:\n\nDetails.");
        assert!(out.contains("### What it does"));
        assert!(out.contains("### How it works"));
    }

    #[test]
    fn normalize_inline_section_labels_breaks_run_on_headers() {
        let raw = "I'm your planner for specialist agents.What I do:Plan & coordinate.";
        let out = normalize_narrative_prose(raw);
        assert!(out.contains("What I do"));
        assert!(out.contains("### What I do") || out.contains("What I do:"));
        assert!(out.contains("Plan & coordinate"));
    }

    #[test]
    fn normalize_narrative_prose_breaks_glued_diagnosis_sections() {
        let raw = "I found the problem. Here's the diagnosis:Root CauseThe fibonacci-next agent fails.What's HappeningThe agent was built to use sdk.state.Evidence| Date | Status | Details | |---|---|---| | Jun 6 | ok | one run |Fix OptionsRewrite to use file-based state.";
        let out = normalize_narrative_prose(raw);
        assert!(out.contains("### Root Cause"), "out={out}");
        assert!(out.contains("### What's Happening"), "out={out}");
        assert!(out.contains("### Evidence"), "out={out}");
        assert!(out.contains("### Fix Options"), "out={out}");
        assert!(out.contains("| Date |"), "table row preserved: {out}");
    }

    #[test]
    fn normalize_fenced_code_wraps_python_blocks() {
        let raw = "Here is the fix:\n\ndef next_fib(state):\n    return state + 1\n\nThat should work.";
        let out = normalize_narrative_prose(raw);
        assert!(out.contains("```python"));
        assert!(out.contains("def next_fib"));
    }

    #[test]
    fn render_markdown_styles_python_fence_with_code_background() {
        let md = "### Fix\n\n```python\nimport autonoetic_sdk\nsdk = autonoetic_sdk.init()\n```";
        let lines = render_markdown(md);
        assert!(
            lines.iter().any(|l| line_is_code_block(l)),
            "expected code block lines"
        );
        let joined = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("python"));
        assert!(joined.contains("import autonoetic_sdk"));
    }

    #[test]
    fn normalize_unicode_rules_converted_to_markdown() {
        let raw = "Some text\n\n──────────────────────\n\nMore text";
        let out = normalize_narrative_prose(raw);
        assert!(out.contains("---"), "expected markdown rule, got: {out}");
        assert!(!out.contains('\u{2500}'), "unicode rule should be gone: {out}");
    }

    #[test]
    fn looks_like_markdown_detects_unicode_rule() {
        assert!(looks_like_markdown("text\n\n──────────────────\n\nmore"));
        assert!(looks_like_markdown("════════════════════"));
    }

    #[test]
    fn looks_like_markdown_detects_table_row() {
        assert!(looks_like_markdown("| Time | Temp |\n|------|------|"));
    }

    #[test]
    fn render_markdown_table_produces_aligned_columns() {
        let md = "| Time | Temp | Conditions |\n|------|------|------------|\n| 5 PM | 24.6C | Cloudy |\n| 6 PM | 24.0C | Clear |\n";
        let lines = render_markdown(md);
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        // Header row
        assert!(
            text.iter().any(|t| t.contains("Time") && t.contains("Temp") && t.contains("Conditions")),
            "header row should contain all column names: {text:?}"
        );
        // Separator line (───)
        assert!(
            text.iter().any(|t| t.contains('─')),
            "should have separator line: {text:?}"
        );
        // Data rows
        assert!(
            text.iter().any(|t| t.contains("5 PM") && t.contains("24.6C") && t.contains("Cloudy")),
            "data row 1 should be on one line: {text:?}"
        );
        assert!(
            text.iter().any(|t| t.contains("6 PM") && t.contains("24.0C") && t.contains("Clear")),
            "data row 2 should be on one line: {text:?}"
        );
    }
}
