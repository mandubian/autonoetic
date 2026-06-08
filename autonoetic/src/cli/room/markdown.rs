use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use std::sync::LazyLock;

static INLINE_SECTION_LABEL: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(
        r"([.!?\)])([ \t]*)((?:What|My|How|Why|When|Where|Who)\s+[^:\n]{2,50}:)",
    )
    .expect("inline section label regex")
});

/// Detect whether a string looks like it contains markdown formatting
/// (headers, bold, italic, code fences, lists, links).
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
            || trimmed.starts_with(|c: char| c.is_ascii_digit())
                && trimmed.contains(". ")
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
    INLINE_SECTION_LABEL
        .replace_all(input, "$1\n\n$2$3")
        .into_owned()
}

/// Full narrative normalization for list/detail panes: inline section breaks,
/// then promote standalone `Label:` lines to `###` headings.
pub(crate) fn normalize_narrative_prose(input: &str) -> String {
    normalize_prose_sections(&normalize_inline_section_labels(input))
}

/// Promote plain section labels (`What it does:`) to markdown headings when the
/// body has no `#` headers yet — common in operator-facing planner summaries.
pub(crate) fn normalize_prose_sections(input: &str) -> String {
    if input.contains("\n# ") || input.contains("\n## ") || input.starts_with('#') {
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

/// Render markdown input into styled ratatui `Line`s for the detail pane.
pub fn render_markdown(input: &str) -> Vec<Line<'static>> {
    let opts = Options::empty();
    let parser = Parser::new_ext(input, opts);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current_spans: Vec<Span<'static>> = Vec::new();
    let mut in_heading: u8 = 0;
    let mut in_bold = false;
    let mut in_italic = false;
    let mut in_code_block = false;
    let mut list_depth: usize = 0;

    for event in parser {
        match event {
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
                lines.push(Line::raw(""));
            }
            Event::Start(Tag::CodeBlock(_)) => {
                flush_line(&mut current_spans, &mut lines);
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                flush_line(&mut current_spans, &mut lines);
                in_code_block = false;
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
                let style = compute_style(in_heading, in_bold, in_italic, in_code_block);
                if in_code_block {
                    for line_text in t.lines() {
                        current_spans.push(Span::styled(line_text.to_string(), style));
                        flush_line(&mut current_spans, &mut lines);
                    }
                } else {
                    current_spans.push(Span::styled(t.to_string(), style));
                }
            }
            Event::Code(t) => {
                let style = Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD);
                current_spans.push(Span::styled(t.to_string(), style));
            }
            Event::SoftBreak | Event::HardBreak => {
                flush_line(&mut current_spans, &mut lines);
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                current_spans.push(Span::styled(
                    dest_url.to_string(),
                    Style::default().fg(Color::Blue).add_modifier(Modifier::UNDERLINED),
                ));
            }
            Event::End(TagEnd::Link) => {}
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

fn compute_style(heading: u8, bold: bool, italic: bool, code_block: bool) -> Style {
    let mut style = Style::default();
    if code_block {
        return style.fg(Color::Gray);
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
}
