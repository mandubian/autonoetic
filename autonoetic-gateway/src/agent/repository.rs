//! Agent Repository - unified agent loading and identity management.

use crate::runtime::parser::SkillParser;
use crate::scheduler::gateway_store::GatewayStore;
use autonoetic_types::agent::{AgentManifest, AgentMeta};
use autonoetic_types::agent_revision::{
    parse_agent_target, AgentRef, AgentRevisionRecord, AgentRevisionStatus, ParsedAgentTarget,
    SessionAgentBinding,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

/// A fully loaded agent with its manifest and instructions.
#[derive(Debug, Clone)]
pub struct LoadedAgent {
    pub dir: PathBuf,
    pub manifest: AgentManifest,
    pub instructions: String,
    /// Optional extended instructions (everything after `<!-- extended -->`
    /// in the SKILL.md body) available for on-demand retrieval.
    pub extended_instructions: Option<String>,
}

impl LoadedAgent {
    /// Returns the agent's ID from the manifest.
    pub fn id(&self) -> &str {
        &self.manifest.agent.id
    }

    /// Returns the directory name.
    pub fn dir_name(&self) -> String {
        self.dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    }
}

/// Repository for discovering and loading agents.
/// Provides unified agent loading across gateway, scheduler, router, and CLI.
pub struct AgentRepository {
    agents_dir: PathBuf,
    cache: RwLock<Vec<AgentMeta>>,
}

impl AgentRepository {
    /// Create a new repository scanning the given agents directory.
    pub fn new(agents_dir: PathBuf) -> Self {
        Self {
            agents_dir,
            cache: RwLock::new(Vec::new()),
        }
    }

    /// Create from a config's agents directory.
    pub fn from_config(config: &autonoetic_types::config::GatewayConfig) -> Self {
        Self::new(config.agents_dir.clone())
    }

    /// Refresh the agent cache by scanning the directory.
    pub async fn refresh(&self) -> anyhow::Result<Vec<AgentMeta>> {
        let agents = scan_agents(&self.agents_dir)?;
        *self.cache.write().await = agents.clone();
        Ok(agents)
    }

    /// Get cached agents (or scan if empty).
    pub async fn list(&self) -> anyhow::Result<Vec<AgentMeta>> {
        let cache = self.cache.read().await;
        if !cache.is_empty() {
            return Ok(cache.clone());
        }
        drop(cache);
        self.refresh().await
    }

    /// Load a specific agent by ID.
    /// Returns an error if the agent doesn't exist or identity mismatch.
    pub async fn get(&self, agent_id: &str) -> anyhow::Result<LoadedAgent> {
        let meta = self
            .list()
            .await?
            .into_iter()
            .find(|a| a.id == agent_id)
            .ok_or_else(|| anyhow::anyhow!("Agent '{}' not found", agent_id))?;

        self.load_from_meta(&meta)
    }

    /// Load a specific agent by ID synchronously (scans directory directly).
    /// Returns an error if the agent doesn't exist or identity mismatch.
    pub fn get_sync(&self, agent_id: &str) -> anyhow::Result<LoadedAgent> {
        let agents = scan_agents(&self.agents_dir)?;
        let meta = agents
            .into_iter()
            .find(|a| a.id == agent_id)
            .ok_or_else(|| anyhow::anyhow!("Agent '{}' not found", agent_id))?;

        self.load_from_meta(&meta)
    }

    /// Load all agents synchronously in a single directory scan.
    /// Returns a vector of LoadedAgent, or an error if any agent fails to load.
    pub fn list_loaded_sync(&self) -> anyhow::Result<Vec<LoadedAgent>> {
        let agents = scan_agents(&self.agents_dir)?;
        let mut loaded = Vec::new();
        let mut errors = Vec::new();
        for meta in agents {
            match self.load_from_meta(&meta) {
                Ok(loaded_agent) => loaded.push(loaded_agent),
                Err(e) => errors.push((meta.id.clone(), e)),
            }
        }

        if !errors.is_empty() {
            let error_details: Vec<String> = errors
                .iter()
                .map(|(id, e)| format!("  - {}: {}", id, e))
                .collect();
            anyhow::bail!(
                "Failed to load {} agent(s):\n{}",
                errors.len(),
                error_details.join("\n")
            );
        }

        Ok(loaded)
    }

    /// Load an agent from an AgentMeta, enforcing identity rules.
    pub fn load_from_meta(&self, meta: &AgentMeta) -> anyhow::Result<LoadedAgent> {
        let skill_path = meta.dir.join("SKILL.md");
        let skill_content = std::fs::read_to_string(&skill_path)?;
        let (manifest, instructions) = SkillParser::parse(&skill_content)?;
        let (core_instructions, extended) =
            crate::runtime::parser::split_extended_instructions(&instructions);

        // Enforce identity: directory name must match manifest agent ID
        let dir_name = meta
            .dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();

        if dir_name != manifest.agent.id {
            anyhow::bail!(
                "Agent identity mismatch: directory name '{}' does not match manifest agent.id '{}'. \
                Either rename the directory to match the agent ID, or fix the agent.id in SKILL.md.",
                dir_name,
                manifest.agent.id
            );
        }

        // Validate execution_mode: Script requires script_entry
        use autonoetic_types::agent::ExecutionMode;
        if matches!(manifest.execution_mode, ExecutionMode::Script) {
            if manifest.script_entry.is_none() {
                anyhow::bail!(
                    "Agent '{}' has execution_mode=script but is missing script_entry. \
                    Add 'script_entry: scripts/main.py' to the agent manifest.",
                    manifest.agent.id
                );
            }
        }

        Ok(LoadedAgent {
            dir: meta.dir.clone(),
            manifest,
            instructions: core_instructions.to_string(),
            extended_instructions: extended.map(String::from),
        })
    }

    /// Try to load an agent, returning None if not found.
    /// Returns an error only for identity mismatch or other actual errors.
    /// Useful for scenarios where missing agents are acceptable.
    pub async fn try_get(&self, agent_id: &str) -> anyhow::Result<Option<LoadedAgent>> {
        let agents = self.list().await?;

        // First check if agent exists in directory
        let exists = agents.iter().any(|a| a.id == agent_id);
        if !exists {
            return Ok(None);
        }

        // Agent exists, try to load it (this will enforce identity)
        match self.get(agent_id).await {
            Ok(loaded) => Ok(Some(loaded)),
            Err(e) => {
                // If it's a "not found" error (shouldn't happen given we checked exists), return None
                if e.to_string().contains("not found") {
                    Ok(None)
                } else {
                    // Re-throw identity mismatch or other errors
                    Err(e)
                }
            }
        }
    }

    /// Get the agents directory path.
    pub fn agents_dir(&self) -> &Path {
        &self.agents_dir
    }

    pub fn load_from_revision_dir(
        &self,
        gateway_dir: &Path,
        agent_id: &str,
        revision_id: &str,
    ) -> anyhow::Result<LoadedAgent> {
        let rev_dir = gateway_dir
            .join("revisions")
            .join("agents")
            .join(agent_id)
            .join(revision_id);
        let skill_path = rev_dir.join("SKILL.md");
        let skill_content = std::fs::read_to_string(&skill_path)?;
        let (manifest, instructions) = SkillParser::parse(&skill_content)?;
        let (core_instructions, extended) =
            crate::runtime::parser::split_extended_instructions(&instructions);

        Ok(LoadedAgent {
            dir: rev_dir,
            manifest,
            instructions: core_instructions.to_string(),
            extended_instructions: extended.map(String::from),
        })
    }

    /// Load an agent from the revision store via GatewayStore alias resolution.
    /// Fails if GatewayStore is unavailable or no alias exists for the agent.
    pub fn get_sync_from_store(
        &self,
        agent_id: &str,
        gateway_dir: &Path,
        gateway_store: Option<&GatewayStore>,
    ) -> anyhow::Result<LoadedAgent> {
        let Some(gs) = gateway_store else {
            anyhow::bail!(
                "GatewayStore is required to load agent '{}'. \
                 The gateway must be running with a gateway store.",
                agent_id
            );
        };
        let Some(alias) = gs.get_agent_alias(agent_id)? else {
            anyhow::bail!(
                "No alias found for agent '{}'. \
                 The agent must be seeded (artifact -> revision -> promote) before use.",
                agent_id
            );
        };
        self.load_from_revision_dir(gateway_dir, &alias.agent_id, &alias.revision_id)
    }

    /// Resolve an agent target string to a concrete revision.
    ///
    /// The target can be:
    /// - A plain agent_id (e.g., "planner.default") → resolved via alias lookup
    /// - A full agent_ref (e.g., "planner.default@rev_sha256:...") → parsed and used directly
    /// - A short agent_ref (e.g., "planner.default@rev_abc12345") → resolved via short ID index
    ///
    /// Targets containing '@' that don't parse as valid agent_ref and don't match
    /// a short ID in the index are rejected (not reinterpreted as alias lookups),
    /// per the spec resolution contract.
    pub fn resolve_agent(
        &self,
        target: &str,
        gateway_store: Option<&GatewayStore>,
    ) -> anyhow::Result<(AgentRef, AgentRevisionRecord)> {
        let Some(gateway_store) = gateway_store else {
            anyhow::bail!("GatewayStore is required for revision-based resolution");
        };

        match parse_agent_target(target) {
            Some(ParsedAgentTarget::ExplicitRef {
                agent_id: agent_id_part,
                revision_selector: rev_part,
            }) => {
                // Try strict agent_ref parsing first (full hex format)
                if let Some(agent_ref) = AgentRef::parse(target) {
                    let rev = match gateway_store.get_agent_revision(&agent_ref.revision_id)? {
                        Some(r) => r,
                        None => {
                            anyhow::bail!("Revision '{}' not found in store", agent_ref.revision_id)
                        }
                    };
                    anyhow::ensure!(
                        rev.agent_id == agent_ref.agent_id,
                        "Revision '{}' belongs to agent '{}', not '{}'",
                        agent_ref.revision_id,
                        rev.agent_id,
                        agent_ref.agent_id
                    );
                    return Ok((agent_ref, rev));
                }

                // Try short ID resolution: target is like "agent_id@rev_abc12345"
                // Check if rev_part looks like a short ID (rev_<alphanumeric>)
                if rev_part.starts_with("rev_") && rev_part.len() > 4 {
                    let short_id = &rev_part[4..]; // strip "rev_" prefix
                    if let Some(full_revision_id) = gateway_store.lookup_short_id(short_id)? {
                        let rev = match gateway_store.get_agent_revision(&full_revision_id)? {
                            Some(r) => r,
                            None => anyhow::bail!(
                                "Short ID '{}' resolved to revision '{}' which was not found",
                                rev_part,
                                full_revision_id
                            ),
                        };
                        // Validate agent_id matches
                        anyhow::ensure!(
                            rev.agent_id == agent_id_part,
                            "Short ref '{}' resolves to revision belonging to agent '{}', not '{}'",
                            target,
                            rev.agent_id,
                            agent_id_part
                        );
                        let agent_ref =
                            AgentRef::new(rev.agent_id.clone(), rev.revision_id.clone());
                        return Ok((agent_ref, rev));
                    }
                }

                // Neither full nor short ref worked
                anyhow::bail!(
                "Invalid agent_ref '{}': must be in format 'agent_id@rev_sha256:<64 hex chars>' or 'agent_id@rev_<short_id>' with a registered short ID",
                target
            );
            }
            Some(ParsedAgentTarget::AliasId(alias_id)) => {
                // Plain agent_id — resolve via alias
                let alias = match gateway_store.resolve_alias(&alias_id)? {
                    Some(a) => a,
                    None => {
                        // Check if a Candidate revision exists — suggest revision_id for smoke testing.
                        let candidate_hint = match gateway_store.list_agent_revisions(&alias_id) {
                            Ok(revs) => revs
                                .iter()
                                .find(|r| r.status == AgentRevisionStatus::Candidate)
                                .map(|r| r.revision_id.clone()),
                            Err(e) => {
                                tracing::warn!(
                    target: "agent_repository",
                    agent_id = %alias_id,
                    error = %e,
                    "Failed to list revisions for candidate hint while reporting missing alias"
                                );
                                None
                            }
                        };
                        match candidate_hint {
                            Some(rev_id) => anyhow::bail!(
                                "No alias '{}' found — the agent has not been promoted yet. A candidate revision exists ({}). To smoke-test it before promotion, use agent_spawn(agent_id=\"{}\", revision_id=\"{}\"). To install it, call agent_revision_promote.",
                                alias_id, rev_id, alias_id, rev_id
                            ),
                            None => anyhow::bail!(
                                "No alias '{}' found. Create a revision and promote it first.",
                                alias_id
                            ),
                        }
                    }
                };

                // NOTE: suspension is intentionally NOT checked here. Resolving
                // an agent is read-only (evaluation/diff of a suspended agent
                // must remain possible so an operator can decide whether to
                // lift the suspension). The "no new session" gate lives at the
                // session-start boundary in `resolve_and_pin_session`.

                let rev = match gateway_store.get_agent_revision(&alias.revision_id)? {
                    Some(r) => r,
                    None => anyhow::bail!(
                        "Revision '{}' referenced by alias '{}' not found",
                        alias.revision_id,
                        alias_id
                    ),
                };

                let agent_ref = AgentRef::new(alias.agent_id.clone(), alias.revision_id.clone());
                Ok((agent_ref, rev))
            }
            None => anyhow::bail!(
                "Invalid target '{}': expected '<agent_id>' or '<agent_id>@<revision>'",
                target
            ),
        }
    }

    /// Resolve and pin a session to an agent revision.
    ///
    /// This is the entry point for session start: it resolves the target,
    /// validates the revision, creates a `SessionAgentBinding`, and persists it.
    pub fn resolve_and_pin_session(
        &self,
        session_id: &str,
        root_session_id: &str,
        target: &str,
        gateway_store: Option<&GatewayStore>,
        home_node_id: &str,
    ) -> anyhow::Result<(AgentRef, AgentRevisionRecord, SessionAgentBinding)> {
        self.resolve_and_pin_session_with_revision(
            session_id,
            root_session_id,
            target,
            None,
            gateway_store,
            home_node_id,
        )
    }

    /// Resolve and pin a session to a specific revision.
    ///
    /// If `revision_id` is `Some`, the target is resolved to an agent_id but the
    /// supplied revision is used directly. This allows smoke-testing a Candidate
    /// revision before it is promoted to the active alias.
    pub fn resolve_and_pin_session_with_revision(
        &self,
        session_id: &str,
        root_session_id: &str,
        target: &str,
        revision_id: Option<&str>,
        gateway_store: Option<&GatewayStore>,
        home_node_id: &str,
    ) -> anyhow::Result<(AgentRef, AgentRevisionRecord, SessionAgentBinding)> {
        if let Some(gs) = gateway_store {
            if let Some(existing) = gs.get_session_agent_binding(session_id)? {
                let rev = gs
                    .get_agent_revision(&existing.revision_id)?
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "Session '{}' is pinned to missing revision '{}'",
                            session_id,
                            existing.revision_id
                        )
                    })?;
                let agent_ref =
                    AgentRef::new(existing.agent_id.clone(), existing.revision_id.clone());
                return Ok((agent_ref, rev, existing));
            }
        }

        let (agent_ref, rev) = if let Some(rev_id) = revision_id {
            let Some(gs) = gateway_store else {
                anyhow::bail!("GatewayStore is required for revision-based resolution");
            };

            // Resolve the caller's target to the underlying agent_id. For aliases,
            // use the alias record when one exists; otherwise fall back to the raw
            // alias id (this allows smoke-testing a Candidate revision before it
            // has been promoted to an active alias). Explicit refs already carry
            // the agent id directly.
            let agent_id = match parse_agent_target(target) {
                Some(ParsedAgentTarget::AliasId(alias_id)) => gs
                    .resolve_alias(&alias_id)?
                    .map(|a| a.agent_id)
                    .unwrap_or(alias_id),
                Some(ParsedAgentTarget::ExplicitRef { agent_id, .. }) => agent_id,
                None => target.to_string(),
            };

            let rev = match gs.get_agent_revision(rev_id)? {
                Some(r) => r,
                None => anyhow::bail!("Revision '{}' not found in store", rev_id),
            };
            anyhow::ensure!(
                rev.agent_id == agent_id,
                "Revision '{}' belongs to agent '{}', not '{}'",
                rev_id,
                rev.agent_id,
                agent_id
            );
            (AgentRef::new(agent_id, rev_id.to_string()), rev)
        } else {
            self.resolve_agent(target, gateway_store)?
        };

        // No-new-session gate: a suspended agent must not start a *new* session,
        // regardless of how it was addressed (alias OR explicit `agent@rev_…`
        // ref). Already-running sessions are unaffected — they returned above
        // via the existing-binding grace period. Keyed on the resolved
        // agent_id's alias, so suspending the agent blocks spawning any of its
        // revisions. (Read-only resolution in `resolve_agent` stays open.)
        if let Some(gs) = gateway_store {
            if let Some(alias) = gs.resolve_alias(&agent_ref.agent_id)? {
                if let Some(ref suspended_at) = alias.suspended_at {
                    let reason = alias.suspended_reason.as_deref().unwrap_or("no reason given");
                    let by = alias.suspended_by.as_deref().unwrap_or("unknown");
                    anyhow::bail!(
                        "Agent '{}' is suspended (since {}) by {}: {}. \
                         No new session can be started; unsuspend or re-promote \
                         the agent first.",
                        agent_ref.agent_id, suspended_at, by, reason,
                    );
                }
            }
        }

        // Determine alias_id from shared target parsing: explicit refs bypass aliases.
        let alias_id = match parse_agent_target(target) {
            Some(ParsedAgentTarget::AliasId(alias_id)) => Some(alias_id),
            Some(ParsedAgentTarget::ExplicitRef { .. }) | None => None,
        };

        // #821: pin the constitution (version + digest) that admitted this
        // session, mirroring runtime_lock_hash above. `None` when the
        // constitution runtime was never initialized (e.g. some tests).
        let (constitution_version, constitution_digest) =
            match crate::constitution_digest::try_constitution_pin() {
                Some((version, digest)) => (Some(version), Some(digest)),
                None => (None, None),
            };

        let binding = SessionAgentBinding {
            session_id: session_id.to_string(),
            root_session_id: root_session_id.to_string(),
            alias_id,
            agent_id: agent_ref.agent_id.clone(),
            revision_id: agent_ref.revision_id.clone(),
            runtime_lock_hash: rev.runtime_lock_hash.clone(),
            constitution_version,
            constitution_digest,
            home_node_id: home_node_id.to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            requested_target: target.to_string(),
        };

        if let Some(gs) = gateway_store {
            gs.upsert_session_agent_binding(&binding)?;
        }

        Ok((agent_ref, rev, binding))
    }
}

/// Scan a directory for agent subdirectories.
///
/// Each subdirectory containing a `SKILL.md` file is treated as an agent.
pub fn scan_agents(dir: &Path) -> anyhow::Result<Vec<AgentMeta>> {
    let mut agents = Vec::new();

    if !dir.exists() {
        tracing::warn!("Agents directory {} does not exist", dir.display());
        return Ok(agents);
    }

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let skill_md = path.join("SKILL.md");
            if skill_md.exists() {
                let id = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                agents.push(AgentMeta { id, dir: path });
            }
        }
    }

    tracing::info!("Discovered {} agent(s)", agents.len());
    Ok(agents)
}

/// Create a cached agent repository wrapper.
pub fn cached(agents_dir: PathBuf) -> Arc<AgentRepository> {
    Arc::new(AgentRepository::new(agents_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use autonoetic_types::principal::PrincipalKind;
    use tempfile::tempdir;

    fn create_test_agent(temp_dir: &Path, agent_id: &str) -> anyhow::Result<PathBuf> {
        let agent_dir = temp_dir.join(agent_id);
        std::fs::create_dir_all(agent_dir.join("state"))?;
        std::fs::create_dir_all(agent_dir.join("skills"))?;

        let skill_md = format!(
            r#"---
name: "{agent_id}"
description: "Test agent"
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      sandbox: "bubblewrap"
      runtime_lock: "runtime.lock"
    agent:
      id: "{agent_id}"
      name: "{agent_id}"
      description: "Test agent"
    capabilities: []
---
# {agent_id}
Test instructions.
"#
        );
        std::fs::write(agent_dir.join("SKILL.md"), skill_md)?;
        Ok(agent_dir)
    }

    #[test]
    fn test_agent_repository_loads_agent() {
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agents dir should create");

        create_test_agent(&agents_dir, "test-agent").expect("agent should create");

        let repo = AgentRepository::new(agents_dir);
        let loaded = repo.get_sync("test-agent").expect("should load agent");

        assert_eq!(loaded.id(), "test-agent");
        assert!(loaded.instructions.contains("Test instructions"));
    }

    #[test]
    fn test_agent_repository_identity_mismatch() {
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agents dir should create");

        // Create agent with directory name "dir-agent" but manifest says "different-id"
        let agent_dir = agents_dir.join("dir-agent");
        std::fs::create_dir_all(agent_dir.join("state")).expect("agent dir should create");

        let skill_md = r#"---
name: "different-id"
description: "Test agent"
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      sandbox: "bubblewrap"
      runtime_lock: "runtime.lock"
    agent:
      id: "different-id"
      name: "different-id"
      description: "Test agent"
    capabilities: []
---
# different-id
Test instructions.
"#;
        std::fs::write(agent_dir.join("SKILL.md"), skill_md).expect("skill.md should write");

        let repo = AgentRepository::new(agents_dir);
        let result = repo.get_sync("dir-agent");

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("identity mismatch"));
    }

    #[test]
    fn test_agent_repository_script_mode_requires_script_entry() {
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agents dir should create");

        let agent_dir = agents_dir.join("script-agent");
        std::fs::create_dir_all(agent_dir.join("state")).expect("agent dir should create");

        let skill_md = r#"---
version: "1.0"
runtime:
  engine: "autonoetic"
  gateway_version: "0.1.0"
  sdk_version: "0.1.0"
  type: "stateful"
  sandbox: "bubblewrap"
  runtime_lock: "runtime.lock"
agent:
  id: "script-agent"
  name: "Script Agent"
  description: "A script-only agent"
execution_mode: script
# Missing script_entry!
capabilities: []
---
# Script Agent
"#;
        std::fs::write(agent_dir.join("SKILL.md"), skill_md).expect("skill.md should write");

        let repo = AgentRepository::new(agents_dir);
        let result = repo.get_sync("script-agent");

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err
            .to_string()
            .contains("execution_mode=script but is missing script_entry"));
    }

    #[tokio::test]
    async fn test_agent_repository_list() {
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agents dir should create");

        create_test_agent(&agents_dir, "agent-a").expect("agent-a should create");
        create_test_agent(&agents_dir, "agent-b").expect("agent-b should create");

        let repo = AgentRepository::new(agents_dir);
        let agents = repo.list().await.expect("should list agents");

        assert_eq!(agents.len(), 2);
        let ids: Vec<_> = agents.iter().map(|a| a.id.clone()).collect();
        assert!(ids.contains(&"agent-a".to_string()));
        assert!(ids.contains(&"agent-b".to_string()));
    }

    #[test]
    fn test_list_loaded_sync_fails_on_identity_mismatch() {
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agents dir should create");

        create_test_agent(&agents_dir, "good-agent").expect("good agent should create");

        let bad_agent_dir = agents_dir.join("bad-dir");
        std::fs::create_dir_all(bad_agent_dir.join("state")).expect("bad agent dir should create");
        std::fs::create_dir_all(bad_agent_dir.join("skills")).expect("skills dir should create");

        let skill_md = r#"---
name: "bad-dir"
description: "Test agent"
metadata:
  autonoetic:
    version: "1.0"
    runtime:
      engine: "autonoetic"
      gateway_version: "0.1.0"
      sdk_version: "0.1.0"
      type: "stateful"
      sandbox: "bubblewrap"
      runtime_lock: "runtime.lock"
    agent:
      id: "different-id"
      name: "Test Agent"
      description: "Test agent"
    capabilities: []
---
# different-id
Test instructions.
"#;
        std::fs::write(bad_agent_dir.join("SKILL.md"), skill_md).expect("skill.md should write");

        let repo = AgentRepository::new(agents_dir);
        let result = repo.list_loaded_sync();

        assert!(
            result.is_err(),
            "list_loaded_sync should fail on identity mismatch"
        );
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("identity mismatch"),
            "Error should mention identity mismatch: {}",
            err
        );
    }

    #[test]
    fn test_resolve_agent_requires_gateway_store() {
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agents dir should create");

        let repo = AgentRepository::new(agents_dir);
        let result = repo.resolve_agent("planner.default", None);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("GatewayStore is required"));
    }

    #[test]
    fn test_resolve_agent_rejects_invalid_ref_format() {
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agents dir should create");

        let repo = AgentRepository::new(agents_dir);
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
        let store = GatewayStore::open(&gateway_dir).expect("should open store");

        // Invalid revision format (contains @ but not a valid agent_ref) — must be rejected, not fall back to alias lookup
        let result = repo.resolve_agent("planner.default@not-a-revision", Some(&store));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Invalid agent_ref"),
            "Should reject invalid @ targets, got: {}",
            err
        );
    }

    #[test]
    fn test_resolve_agent_rejects_mismatched_agent_id() {
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agents dir should create");

        let repo = AgentRepository::new(agents_dir);
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
        let store = GatewayStore::open(&gateway_dir).expect("should open store");

        // Insert a revision for "other-agent"
        let rev = autonoetic_types::agent_revision::AgentRevisionRecord {
            revision_id:
                "rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234"
                    .to_string(),
            agent_id: "other-agent".to_string(),
            base_revision_id: None,
            artifact_id: None,
            content_digest: "sha256:test".to_string(),
            runtime_lock_hash: "sha256:lock".to_string(),
            manifest_hash: "sha256:manifest".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            created_by_type: PrincipalKind::Human.tag().to_string(),
            created_by_id: "admin".to_string(),
            requested_by_type: None,
            requested_by_id: None,
            source_kind: "artifact".to_string(),
            source_ref: None,
            origin_node_id: "gateway".to_string(),
            trust_domain: "local".to_string(),
            status: autonoetic_types::agent_revision::AgentRevisionStatus::Ready,
            metadata_json: serde_json::Value::Null,
            short_id: "abcd1234".to_string(),
        detected_network_hosts: None,
            signature: None,
            signer_id: None,
        };
        store
            .insert_agent_revision(&rev)
            .expect("should insert revision");

        // Try to resolve with wrong agent_id prefix
        let result = repo.resolve_agent(
            "planner.default@rev_sha256:abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234abcd1234",
            Some(&store),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("belongs to agent"));
    }

    #[test]
    fn test_resolve_agent_no_alias_returns_helpful_error() {
        let temp = tempdir().expect("tempdir should create");
        let agents_dir = temp.path().join("agents");
        std::fs::create_dir_all(&agents_dir).expect("agents dir should create");

        let repo = AgentRepository::new(agents_dir);
        let gateway_dir = temp.path().join(".gateway");
        std::fs::create_dir_all(&gateway_dir).expect("gateway dir should create");
        let store = GatewayStore::open(&gateway_dir).expect("should open store");

        let result = repo.resolve_agent("planner.default", Some(&store));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err
            .to_string()
            .contains("Create a revision and promote it first"));
    }
}
