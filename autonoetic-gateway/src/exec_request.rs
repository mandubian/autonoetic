//! Intent-based execution requests (RFC `docs/archived/portable-wasm-execution-tier.md`, P2).
//!
//! Replaces the bare `entrypoint: &str` handed to the sandbox with a structured
//! description of *what to run* rather than *a shell line*. The Process backend
//! (bubblewrap/docker) renders it back to `sh -c …` via
//! [`ExecutionKind::render_process_command`]; a future WASM backend (P4) will
//! dispatch `Code { language: Some(_) }` to an in-process interpreter instead.

/// What to run inside the sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionKind {
    /// Free-form shell command — today's `sandbox.exec`. Native tier only;
    /// the WASM tier rejects it (no shell).
    Shell { command: String },
    /// A program to run. `language = None` execs the entry directly
    /// (shebang-driven, as script-mode does today); `Some(lang)` runs it via
    /// that interpreter — required for the WASM tier, where the interpreter
    /// must be explicit.
    Code {
        language: Option<CodeLanguage>,
        source: CodeSource,
        args: Vec<String>,
    },
}

/// Language whose interpreter runs a [`CodeSource`]. Extensible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeLanguage {
    Python,
    JavaScript,
}

/// Where the code to run comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeSource {
    /// Path to an entry file inside the sandbox workspace.
    Entry(String),
    /// Inline source text (requires a `language` to know the interpreter).
    Inline(String),
}

impl CodeLanguage {
    /// Interpreter binary for the native (process) tier.
    fn process_interpreter(self) -> &'static str {
        match self {
            CodeLanguage::Python => "python3",
            CodeLanguage::JavaScript => "node",
        }
    }
}

impl ExecutionKind {
    /// Convenience constructor for a free-form shell command.
    pub fn shell(command: impl Into<String>) -> Self {
        ExecutionKind::Shell {
            command: command.into(),
        }
    }

    /// Render to a shell command line for the Process backend (bubblewrap/docker).
    /// Errors on combinations the native tier can't express — inline source with
    /// no language has no interpreter to feed it.
    pub fn render_process_command(&self) -> anyhow::Result<String> {
        match self {
            ExecutionKind::Shell { command } => Ok(command.clone()),
            ExecutionKind::Code {
                language,
                source,
                args,
            } => {
                let suffix = render_args_suffix(args);
                match (language, source) {
                    // Exec the entry directly (shebang-driven) — script-mode parity.
                    // The path is shell-quoted so spaces/metacharacters in the
                    // workspace path can't word-split or expand.
                    (None, CodeSource::Entry(path)) => Ok(format!("{}{suffix}", shell_quote(path))),
                    (Some(lang), CodeSource::Entry(path)) => Ok(format!(
                        "{} {}{suffix}",
                        lang.process_interpreter(),
                        shell_quote(path)
                    )),
                    (Some(CodeLanguage::Python), CodeSource::Inline(src)) => {
                        Ok(format!("python3 -c {}{suffix}", shell_quote(src)))
                    }
                    (Some(CodeLanguage::JavaScript), CodeSource::Inline(src)) => {
                        Ok(format!("node -e {}{suffix}", shell_quote(src)))
                    }
                    (None, CodeSource::Inline(_)) => anyhow::bail!(
                        "inline code requires a language (no interpreter to exec it directly)"
                    ),
                }
            }
        }
    }
}

fn render_args_suffix(args: &[String]) -> String {
    if args.is_empty() {
        return String::new();
    }
    let quoted = args
        .iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    format!(" {quoted}")
}

/// POSIX single-quote shell escaping: wrap in single quotes, and replace any
/// embedded single quote with the `'\''` close-reopen idiom.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_passthrough() {
        let k = ExecutionKind::shell("echo hi && ls");
        assert_eq!(k.render_process_command().unwrap(), "echo hi && ls");
    }

    #[test]
    fn entry_no_language_execs_directly() {
        // Script-mode parity: run the file via its shebang. The entry path is
        // shell-quoted (robust against spaces/metacharacters in the path).
        let k = ExecutionKind::Code {
            language: None,
            source: CodeSource::Entry("/tmp/main.py".into()),
            args: vec![],
        };
        assert_eq!(k.render_process_command().unwrap(), "'/tmp/main.py'");
    }

    #[test]
    fn entry_path_with_spaces_is_quoted() {
        let k = ExecutionKind::Code {
            language: None,
            source: CodeSource::Entry("/tmp/my agent/run.py".into()),
            args: vec![],
        };
        assert_eq!(
            k.render_process_command().unwrap(),
            "'/tmp/my agent/run.py'"
        );
    }

    #[test]
    fn entry_with_args_are_quoted() {
        let k = ExecutionKind::Code {
            language: None,
            source: CodeSource::Entry("/tmp/main.py".into()),
            args: vec!["a b".into(), "x'y".into()],
        };
        assert_eq!(
            k.render_process_command().unwrap(),
            r"'/tmp/main.py' 'a b' 'x'\''y'"
        );
    }

    #[test]
    fn backslashes_survive_single_quoting() {
        // Regression (PR #445 review): inside POSIX single quotes a backslash is
        // literal, so a JSON-ish arg keeps exactly one backslash. The prior
        // `shell_words_quote` doubled backslashes (`\n` → `\\n`) — a bug this
        // path fixes. The shell will hand the script `{"k":"a\nb"}` verbatim.
        let k = ExecutionKind::Code {
            language: None,
            source: CodeSource::Entry("/tmp/main.py".into()),
            args: vec![r#"{"k":"a\nb"}"#.into()],
        };
        assert_eq!(
            k.render_process_command().unwrap(),
            r#"'/tmp/main.py' '{"k":"a\nb"}'"#
        );
    }

    #[test]
    fn python_entry_uses_interpreter() {
        let k = ExecutionKind::Code {
            language: Some(CodeLanguage::Python),
            source: CodeSource::Entry("/workspace/run.py".into()),
            args: vec!["--flag".into()],
        };
        assert_eq!(
            k.render_process_command().unwrap(),
            "python3 '/workspace/run.py' '--flag'"
        );
    }

    #[test]
    fn python_inline_uses_dash_c() {
        let k = ExecutionKind::Code {
            language: Some(CodeLanguage::Python),
            source: CodeSource::Inline("print('hi')".into()),
            args: vec![],
        };
        assert_eq!(
            k.render_process_command().unwrap(),
            r"python3 -c 'print('\''hi'\'')'"
        );
    }

    #[test]
    fn inline_without_language_is_rejected() {
        let k = ExecutionKind::Code {
            language: None,
            source: CodeSource::Inline("whatever".into()),
            args: vec![],
        };
        assert!(k.render_process_command().is_err());
    }
}
