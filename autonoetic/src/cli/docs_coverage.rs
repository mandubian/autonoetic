//! Guard: every clap subcommand appears in the CLI reference.
//!
//! The CLI reference was maintained by hand in *two* files (the former
//! top-level `CLI.md` and `cli-reference.md`, merged here). Between them they
//! covered 14 of 18 top-level subcommands:
//! `security`, `watchdog`, `sentinel-experiment` and `improve`
//! were in neither, `recording`/`eval`/`review` in only one, `capsule` in only
//! the other — and the surviving doc advertised a `-c` short flag for
//! `--config` that does not exist. Two partial truths, no way to notice.
//!
//! Generating the whole reference from clap was the other option. It is
//! rejected for now because the doc carries hand-written material clap cannot
//! know — workflows, environment variables, keyboard tables, the `seed` vs
//! `revision promote` distinction. Checking *coverage* keeps that prose and
//! still makes the omission class impossible: add a subcommand without
//! documenting it and this fails.
//!
//! Lives in the **bin** target: PR CI runs `--lib --bins`, so this gates a PR
//! (see `docs_link_guard` in the gateway crate for the same reasoning).

#[cfg(test)]
mod tests {
    use crate::cli::common::Cli;
    use clap::CommandFactory;

    /// Subcommands intentionally absent from the reference.
    ///
    /// `help` is clap's own, not a documented command.
    const NOT_DOCUMENTED: &[&str] = &["help"];

    fn cli_reference() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crate always has a workspace parent")
            .join("docs/reference/cli.md");
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
    }

    #[test]
    fn every_subcommand_appears_in_the_cli_reference() {
        let doc = cli_reference();
        let missing: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .filter(|name| !NOT_DOCUMENTED.contains(&name.as_str()))
            // A command counts as documented when the reference mentions it as
            // an invocation — `autonoetic <name>` — not merely in prose.
            .filter(|name| !doc.contains(&format!("autonoetic {name}")))
            .collect();

        assert!(
            missing.is_empty(),
            "{} subcommand(s) missing from docs/reference/cli.md: {}.\n\
             Every command the CLI accepts must be documented — a command that \
             exists but is documented nowhere is indistinguishable from one that \
             does not exist. Add a section using `autonoetic <name> --help` as \
             ground truth.",
            missing.len(),
            missing.join(", ")
        );
    }

    /// The reference must not advertise flags the parser rejects.
    ///
    /// Guards the specific defect found during the merge: a documented
    /// `-c, --config` short form that clap never defined.
    #[test]
    fn documented_global_options_exist() {
        let cmd = Cli::command();
        let globals: Vec<_> = cmd.get_arguments().map(|a| a.get_id().as_str()).collect();
        for expected in ["config", "log_level", "non_interactive"] {
            assert!(
                globals.contains(&expected),
                "docs/reference/cli.md documents a global `{expected}` that the \
                 parser does not define; globals are {globals:?}"
            );
        }
        assert!(
            cmd.get_arguments()
                .find(|a| a.get_id() == "config")
                .expect("config global")
                .get_short()
                .is_none(),
            "clap now defines a short form for --config; docs/reference/cli.md \
             states there is none"
        );
    }
}
