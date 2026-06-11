//! V4A multi-entry patch format for `content_patch` (`mode="v4a"`).
//!
//! A custom unified-diff-like format. Entries are addressed by content **name**
//! (names are path-like). Hunks are applied with the [`fuzzy_match`] engine, so
//! trivial whitespace/indentation drift in context lines doesn't break a patch.
//!
//! ```text
//! *** Begin Patch
//! *** Update File: src/main.rs
//! @@ optional context @@
//!  unchanged context line
//! -removed line
//! +added line
//! *** Add File: src/new.rs
//! +first line of new file
//! *** End Patch
//! ```
//!
//! Application is **two-phase**: every hunk of every entry is resolved against
//! current content in memory first; only if *all* succeed are any writes made.
//! A single failing hunk aborts the whole patch with no partial state.
//!
//! `Delete File` / `Move File` are parsed but rejected — the content store has
//! no name unregister/rename yet (tracked separately). They surface a clear
//! "not yet supported" error rather than being silently dropped.

use crate::runtime::fuzzy_match::{self, FuzzyError};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_block: String,
    pub new_block: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V4aOp {
    Update { name: String, hunks: Vec<Hunk> },
    Add { name: String, content: String },
    Delete { name: String },
    Move { from: String, to: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V4aError {
    MissingBegin,
    MissingEnd,
    NoOperations,
    BadHeader(String),
    BadHunkLine(String),
    EmptyHunk(String),
}

impl fmt::Display for V4aError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            V4aError::MissingBegin => write!(f, "patch must start with '*** Begin Patch'"),
            V4aError::MissingEnd => write!(f, "patch must end with '*** End Patch'"),
            V4aError::NoOperations => write!(f, "patch contains no file operations"),
            V4aError::BadHeader(l) => write!(f, "unrecognized patch header line: '{l}'"),
            V4aError::BadHunkLine(l) => write!(
                f,
                "hunk line must start with ' ', '+', or '-' (got '{l}')"
            ),
            V4aError::EmptyHunk(name) => write!(f, "empty hunk for '{name}'"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApplyError {
    pub name: String,
    pub hunk_index: usize,
    pub source: FuzzyError,
}

impl fmt::Display for ApplyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "hunk #{} for '{}' failed: {}",
            self.hunk_index + 1,
            self.name,
            self.source
        )
    }
}

const BEGIN: &str = "*** Begin Patch";
const END: &str = "*** End Patch";
const UPDATE: &str = "*** Update File: ";
const ADD: &str = "*** Add File: ";
const DELETE: &str = "*** Delete File: ";
const MOVE: &str = "*** Move File: ";

fn strip_cr(line: &str) -> &str {
    line.strip_suffix('\r').unwrap_or(line)
}

/// Parse a V4A patch into ordered operations.
pub fn parse(patch: &str) -> Result<Vec<V4aOp>, V4aError> {
    let mut lines = patch.lines().map(strip_cr).peekable();

    // Skip leading blank lines, then require the begin marker.
    while matches!(lines.peek(), Some(l) if l.trim().is_empty()) {
        lines.next();
    }
    match lines.next() {
        Some(l) if l.trim() == BEGIN => {}
        _ => return Err(V4aError::MissingBegin),
    }

    let mut ops = Vec::new();
    let mut saw_end = false;

    // Body buffer for the current section.
    let mut current: Option<SectionBuilder> = None;

    while let Some(line) = lines.next() {
        if line.trim() == END {
            saw_end = true;
            break;
        }
        if let Some(name) = line.strip_prefix(UPDATE) {
            flush(&mut current, &mut ops)?;
            current = Some(SectionBuilder::update(name.trim().to_string()));
        } else if let Some(name) = line.strip_prefix(ADD) {
            flush(&mut current, &mut ops)?;
            current = Some(SectionBuilder::add(name.trim().to_string()));
        } else if let Some(name) = line.strip_prefix(DELETE) {
            flush(&mut current, &mut ops)?;
            ops.push(V4aOp::Delete { name: name.trim().to_string() });
        } else if let Some(spec) = line.strip_prefix(MOVE) {
            flush(&mut current, &mut ops)?;
            let (from, to) = spec.split_once("->").unwrap_or((spec, ""));
            ops.push(V4aOp::Move {
                from: from.trim().to_string(),
                to: to.trim().to_string(),
            });
        } else if line.starts_with("*** ") {
            return Err(V4aError::BadHeader(line.to_string()));
        } else {
            match current.as_mut() {
                Some(sec) => sec.push_body(line)?,
                None => {
                    // Stray content outside any section — ignore blank lines,
                    // reject anything else.
                    if !line.trim().is_empty() {
                        return Err(V4aError::BadHeader(line.to_string()));
                    }
                }
            }
        }
    }

    flush(&mut current, &mut ops)?;

    if !saw_end {
        return Err(V4aError::MissingEnd);
    }
    if ops.is_empty() {
        return Err(V4aError::NoOperations);
    }
    Ok(ops)
}

fn flush(current: &mut Option<SectionBuilder>, ops: &mut Vec<V4aOp>) -> Result<(), V4aError> {
    if let Some(sec) = current.take() {
        ops.push(sec.finish()?);
    }
    Ok(())
}

enum SectionKind {
    Update,
    Add,
}

struct SectionBuilder {
    kind: SectionKind,
    name: String,
    /// Hunks accumulated so far (Update only).
    hunks: Vec<Hunk>,
    /// Lines of the current hunk being built: (marker, text).
    cur: Vec<(char, String)>,
    /// Add-file content lines.
    add_lines: Vec<String>,
}

impl SectionBuilder {
    fn update(name: String) -> Self {
        Self { kind: SectionKind::Update, name, hunks: Vec::new(), cur: Vec::new(), add_lines: Vec::new() }
    }
    fn add(name: String) -> Self {
        Self { kind: SectionKind::Add, name, hunks: Vec::new(), cur: Vec::new(), add_lines: Vec::new() }
    }

    fn push_body(&mut self, line: &str) -> Result<(), V4aError> {
        match self.kind {
            SectionKind::Add => {
                // Every line should be an addition; tolerate a bare line as one.
                let text = line.strip_prefix('+').unwrap_or(line);
                self.add_lines.push(text.to_string());
                Ok(())
            }
            SectionKind::Update => {
                // `@@` lines are context anchors → start a new hunk.
                if line.starts_with("@@") {
                    self.close_hunk();
                    return Ok(());
                }
                if line.is_empty() {
                    // Empty line = empty context line in both sides.
                    self.cur.push((' ', String::new()));
                    return Ok(());
                }
                let (marker, rest) = line.split_at(1);
                let m = marker.chars().next().unwrap();
                if m != ' ' && m != '+' && m != '-' {
                    return Err(V4aError::BadHunkLine(line.to_string()));
                }
                self.cur.push((m, rest.to_string()));
                Ok(())
            }
        }
    }

    fn close_hunk(&mut self) {
        if self.cur.is_empty() {
            return;
        }
        let mut old_block = Vec::new();
        let mut new_block = Vec::new();
        for (m, text) in self.cur.drain(..) {
            match m {
                ' ' => {
                    old_block.push(text.clone());
                    new_block.push(text);
                }
                '-' => old_block.push(text),
                '+' => new_block.push(text),
                _ => unreachable!(),
            }
        }
        self.hunks.push(Hunk {
            old_block: old_block.join("\n"),
            new_block: new_block.join("\n"),
        });
    }

    fn finish(mut self) -> Result<V4aOp, V4aError> {
        match self.kind {
            SectionKind::Add => Ok(V4aOp::Add {
                name: self.name,
                content: self.add_lines.join("\n"),
            }),
            SectionKind::Update => {
                self.close_hunk();
                if self.hunks.is_empty() {
                    return Err(V4aError::EmptyHunk(self.name));
                }
                Ok(V4aOp::Update { name: self.name, hunks: self.hunks })
            }
        }
    }
}

/// Apply a sequence of hunks to `current` in order, returning the final text.
/// Each hunk must anchor uniquely (no `replace_all`); the first failure aborts.
pub fn apply_hunks(name: &str, current: &str, hunks: &[Hunk]) -> Result<String, ApplyError> {
    let mut content = current.to_string();
    for (i, hunk) in hunks.iter().enumerate() {
        match fuzzy_match::find_and_replace(&content, &hunk.old_block, &hunk.new_block, false) {
            Ok(outcome) => content = outcome.content,
            Err(source) => {
                return Err(ApplyError { name: name.to_string(), hunk_index: i, source });
            }
        }
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_update() {
        let patch = "*** Begin Patch\n*** Update File: a.rs\n@@\n ctx\n-old\n+new\n*** End Patch\n";
        let ops = parse(patch).unwrap();
        assert_eq!(ops.len(), 1);
        match &ops[0] {
            V4aOp::Update { name, hunks } => {
                assert_eq!(name, "a.rs");
                assert_eq!(hunks.len(), 1);
                assert_eq!(hunks[0].old_block, "ctx\nold");
                assert_eq!(hunks[0].new_block, "ctx\nnew");
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn parse_add_file() {
        let patch = "*** Begin Patch\n*** Add File: new.txt\n+line one\n+line two\n*** End Patch";
        let ops = parse(patch).unwrap();
        match &ops[0] {
            V4aOp::Add { name, content } => {
                assert_eq!(name, "new.txt");
                assert_eq!(content, "line one\nline two");
            }
            _ => panic!("expected Add"),
        }
    }

    #[test]
    fn parse_multi_file() {
        let patch = "*** Begin Patch\n*** Update File: a\n-x\n+y\n*** Add File: b\n+z\n*** End Patch";
        let ops = parse(patch).unwrap();
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn missing_begin_errors() {
        assert_eq!(parse("*** Update File: a\n*** End Patch").unwrap_err(), V4aError::MissingBegin);
    }

    #[test]
    fn missing_end_errors() {
        assert_eq!(
            parse("*** Begin Patch\n*** Update File: a\n-x\n+y\n").unwrap_err(),
            V4aError::MissingEnd
        );
    }

    #[test]
    fn delete_and_move_parsed() {
        let patch = "*** Begin Patch\n*** Delete File: gone\n*** Move File: a -> b\n*** End Patch";
        let ops = parse(patch).unwrap();
        assert_eq!(ops[0], V4aOp::Delete { name: "gone".into() });
        assert_eq!(ops[1], V4aOp::Move { from: "a".into(), to: "b".into() });
    }

    #[test]
    fn apply_sequential_hunks() {
        let current = "one\ntwo\nthree\n";
        let hunks = vec![
            Hunk { old_block: "one".into(), new_block: "ONE".into() },
            Hunk { old_block: "three".into(), new_block: "THREE".into() },
        ];
        let out = apply_hunks("f", current, &hunks).unwrap();
        assert_eq!(out, "ONE\ntwo\nTHREE\n");
    }

    #[test]
    fn apply_aborts_on_missing_hunk() {
        let current = "one\ntwo\n";
        let hunks = vec![
            Hunk { old_block: "one".into(), new_block: "ONE".into() },
            Hunk { old_block: "nope".into(), new_block: "x".into() },
        ];
        let err = apply_hunks("f", current, &hunks).unwrap_err();
        assert_eq!(err.hunk_index, 1);
    }

    #[test]
    fn two_hunks_split_by_at_markers() {
        let patch = "*** Begin Patch\n*** Update File: a\n@@ fn one @@\n-a\n+A\n@@ fn two @@\n-b\n+B\n*** End Patch";
        let ops = parse(patch).unwrap();
        match &ops[0] {
            V4aOp::Update { hunks, .. } => assert_eq!(hunks.len(), 2),
            _ => panic!(),
        }
    }
}
