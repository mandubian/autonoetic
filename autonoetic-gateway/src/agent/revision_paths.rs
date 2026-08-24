//! On-disk layout of the promotion-gated agent revision store.
//!
//! Every promoted agent revision lives at
//! `<gateway_dir>/revisions/agents/<agent_id>/<revision_id>/`, holding the
//! immutable `SKILL.md` + `runtime.lock` that the gateway actually executes.
//! This is the read path #1136 made authoritative: `agents_dir` is the ingest
//! copy (rewritten in place at every bootstrap), the revision store is what
//! runs.
//!
//! These three functions are the **only** place that layout is spelled out.
//! Before this module the path was open-coded at 19 sites in two different
//! spellings (`join("revisions").join("agents")` and `join("revisions/agents")`),
//! which meant any change to the layout — see #2 — was a 19-site edit with no
//! way to prove it was complete. `revision_store_layout_is_centralized` in
//! `tests/guard/revision_store_layout.rs` fails the build if a new call site
//! open-codes it again.
//!
//! `gateway_dir` is the authoritative one the execution engine passes down, not
//! a path re-derived from an agent dir. Deriving it from `agent_dir.parent()`
//! is wrong — agents execute *from inside* this store, so their parent is
//! `<gateway_dir>/revisions/agents/<agent_id>`, not `agents_dir`. That mistake
//! is #1145.

use std::path::{Path, PathBuf};

/// `<gateway_dir>/revisions/agents` — the root holding every agent's revisions.
pub fn agent_revisions_root(gateway_dir: &Path) -> PathBuf {
    gateway_dir.join("revisions").join("agents")
}

/// `<gateway_dir>/revisions/agents/<agent_id>` — all revisions of one agent,
/// plus the best-effort `latest` symlink. Not itself a runnable directory.
pub fn agent_revisions_dir(gateway_dir: &Path, agent_id: &str) -> PathBuf {
    agent_revisions_root(gateway_dir).join(agent_id)
}

/// `<gateway_dir>/revisions/agents/<agent_id>/<revision_id>` — one immutable
/// revision: the directory an `AgentExecutor` receives as its `agent_dir`.
pub fn agent_revision_dir(gateway_dir: &Path, agent_id: &str, revision_id: &str) -> PathBuf {
    agent_revisions_dir(gateway_dir, agent_id).join(revision_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_nest_root_agent_revision() {
        let gw = Path::new("/srv/agents/.gateway");
        assert_eq!(
            agent_revisions_root(gw),
            Path::new("/srv/agents/.gateway/revisions/agents")
        );
        assert_eq!(
            agent_revisions_dir(gw, "coder.default"),
            Path::new("/srv/agents/.gateway/revisions/agents/coder.default")
        );
        assert_eq!(
            agent_revision_dir(gw, "coder.default", "rev-abc123"),
            Path::new("/srv/agents/.gateway/revisions/agents/coder.default/rev-abc123")
        );
    }

    /// Each accessor is the previous one plus one component, so a layout change
    /// in `agent_revisions_root` propagates to all three.
    #[test]
    fn deeper_paths_extend_shallower_ones() {
        let gw = Path::new("/gw");
        assert!(agent_revisions_dir(gw, "a").starts_with(agent_revisions_root(gw)));
        assert!(agent_revision_dir(gw, "a", "r").starts_with(agent_revisions_dir(gw, "a")));
    }
}
