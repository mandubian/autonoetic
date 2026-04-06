use super::GatewayStore;
use anyhow::Result;
use autonoetic_types::agent::{BindingScope, UserAgentBinding, UserProfileRecord};
use rusqlite::params;

impl GatewayStore {
    /// Store or update a user profile.
    pub fn upsert_user_profile(&self, profile: &UserProfileRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO user_profiles (user_id, display_name, trust_domain, origin_node_id, profile_json, profile_version, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(user_id) DO UPDATE SET
                display_name = excluded.display_name,
                trust_domain = excluded.trust_domain,
                origin_node_id = excluded.origin_node_id,
                profile_json = excluded.profile_json,
                profile_version = excluded.profile_version,
                updated_at = excluded.updated_at",
            params![
                profile.user_id,
                profile.display_name,
                profile.trust_domain,
                profile.origin_node_id,
                profile.profile_json,
                profile.profile_version,
                profile.created_at,
                profile.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Get a user profile by user_id.
    pub fn get_user_profile(&self, user_id: &str) -> Result<Option<UserProfileRecord>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT user_id, display_name, trust_domain, origin_node_id, profile_json, profile_version, created_at, updated_at
             FROM user_profiles WHERE user_id = ?1",
            params![user_id],
            |row| {
                Ok(UserProfileRecord {
                    user_id: row.get(0)?,
                    display_name: row.get(1)?,
                    trust_domain: row.get(2)?,
                    origin_node_id: row.get(3)?,
                    profile_json: row.get(4)?,
                    profile_version: row.get(5)?,
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            },
        );
        match row {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Create a user-agent binding. Fails if binding already exists.
    pub fn create_user_binding(&self, binding: &UserAgentBinding) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO user_agent_bindings (user_id, agent_id, scope, granted_at, granted_by)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(user_id, agent_id) DO UPDATE SET
                scope = excluded.scope,
                granted_at = excluded.granted_at,
                granted_by = excluded.granted_by",
            params![
                binding.user_id,
                binding.agent_id,
                binding.scope.to_string(),
                binding.granted_at,
                binding.granted_by,
            ],
        )?;
        Ok(())
    }

    /// Get a specific user-agent binding.
    pub fn get_user_binding(
        &self,
        user_id: &str,
        agent_id: &str,
    ) -> Result<Option<UserAgentBinding>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT user_id, agent_id, scope, granted_at, granted_by
             FROM user_agent_bindings WHERE user_id = ?1 AND agent_id = ?2",
            params![user_id, agent_id],
            |row| {
                let scope_str: String = row.get(2)?;
                let scope = parse_binding_scope(&scope_str);
                Ok(UserAgentBinding {
                    user_id: row.get(0)?,
                    agent_id: row.get(1)?,
                    scope,
                    granted_at: row.get(3)?,
                    granted_by: row.get(4)?,
                })
            },
        );
        match row {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// List all bindings for an agent.
    pub fn list_bindings_for_agent(&self, agent_id: &str) -> Result<Vec<UserAgentBinding>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT user_id, agent_id, scope, granted_at, granted_by
             FROM user_agent_bindings WHERE agent_id = ?1",
        )?;
        let rows = stmt.query_map(params![agent_id], |row| {
            let scope_str: String = row.get(2)?;
            let scope = parse_binding_scope(&scope_str);
            Ok(UserAgentBinding {
                user_id: row.get(0)?,
                agent_id: row.get(1)?,
                scope,
                granted_at: row.get(3)?,
                granted_by: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!(e))
    }

    /// List all bindings for a user.
    pub fn list_bindings_for_user(&self, user_id: &str) -> Result<Vec<UserAgentBinding>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT user_id, agent_id, scope, granted_at, granted_by
             FROM user_agent_bindings WHERE user_id = ?1",
        )?;
        let rows = stmt.query_map(params![user_id], |row| {
            let scope_str: String = row.get(2)?;
            let scope = parse_binding_scope(&scope_str);
            Ok(UserAgentBinding {
                user_id: row.get(0)?,
                agent_id: row.get(1)?,
                scope,
                granted_at: row.get(3)?,
                granted_by: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!(e))
    }

    /// Delete a user-agent binding. Returns true if a row was deleted.
    pub fn delete_user_binding(&self, user_id: &str, agent_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM user_agent_bindings WHERE user_id = ?1 AND agent_id = ?2",
            params![user_id, agent_id],
        )?;
        Ok(n > 0)
    }
}

fn parse_binding_scope(s: &str) -> BindingScope {
    match s {
        "full" => BindingScope::Full,
        "restricted" => BindingScope::Restricted,
        "task_only" => BindingScope::TaskOnly,
        _ => BindingScope::Restricted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_profile(user_id: &str) -> UserProfileRecord {
        UserProfileRecord {
            user_id: user_id.to_string(),
            display_name: Some("Test User".to_string()),
            trust_domain: "local".to_string(),
            origin_node_id: None,
            profile_json: Some(
                serde_json::json!({
                    "preferences": { "language": "en", "theme": "dark" },
                    "constraints": { "max_budget": 100 }
                })
                .to_string(),
            ),
            profile_version: 1,
            created_at: chrono::Utc::now().to_rfc3339(),
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    fn test_binding(user_id: &str, agent_id: &str, scope: BindingScope) -> UserAgentBinding {
        UserAgentBinding {
            user_id: user_id.to_string(),
            agent_id: agent_id.to_string(),
            scope,
            granted_at: chrono::Utc::now().to_rfc3339(),
            granted_by: Some("admin".to_string()),
        }
    }

    #[test]
    fn test_user_profile_upsert_and_get() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let profile = test_profile("user-1");
        store.upsert_user_profile(&profile)?;

        let loaded = store.get_user_profile("user-1")?;
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.user_id, "user-1");
        assert_eq!(loaded.display_name.as_deref(), Some("Test User"));
        assert_eq!(loaded.trust_domain, "local");
        assert_eq!(loaded.profile_version, 1);

        let missing = store.get_user_profile("missing")?;
        assert!(missing.is_none());

        Ok(())
    }

    #[test]
    fn test_user_profile_upsert_updates_version() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let mut profile = test_profile("user-2");
        store.upsert_user_profile(&profile)?;

        profile.profile_version = 2;
        profile.display_name = Some("Updated Name".to_string());
        profile.updated_at = chrono::Utc::now().to_rfc3339();
        store.upsert_user_profile(&profile)?;

        let loaded = store.get_user_profile("user-2")?.unwrap();
        assert_eq!(loaded.profile_version, 2);
        assert_eq!(loaded.display_name.as_deref(), Some("Updated Name"));

        Ok(())
    }

    #[test]
    fn test_user_binding_create_and_get() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let binding = test_binding("user-1", "agent-1", BindingScope::Full);
        store.create_user_binding(&binding)?;

        let loaded = store.get_user_binding("user-1", "agent-1")?;
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.user_id, "user-1");
        assert_eq!(loaded.agent_id, "agent-1");
        assert_eq!(loaded.scope, BindingScope::Full);

        Ok(())
    }

    #[test]
    fn test_user_binding_upsert_updates_scope() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        let binding = test_binding("user-1", "agent-1", BindingScope::Restricted);
        store.create_user_binding(&binding)?;

        let updated = test_binding("user-1", "agent-1", BindingScope::Full);
        store.create_user_binding(&updated)?;

        let loaded = store.get_user_binding("user-1", "agent-1")?.unwrap();
        assert_eq!(loaded.scope, BindingScope::Full);

        Ok(())
    }

    #[test]
    fn test_list_bindings_for_agent() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        store.create_user_binding(&test_binding("user-1", "agent-1", BindingScope::Full))?;
        store.create_user_binding(&test_binding("user-2", "agent-1", BindingScope::Restricted))?;
        store.create_user_binding(&test_binding("user-3", "agent-2", BindingScope::TaskOnly))?;

        let bindings = store.list_bindings_for_agent("agent-1")?;
        assert_eq!(bindings.len(), 2);

        let bindings = store.list_bindings_for_agent("agent-2")?;
        assert_eq!(bindings.len(), 1);

        let bindings = store.list_bindings_for_agent("agent-3")?;
        assert!(bindings.is_empty());

        Ok(())
    }

    #[test]
    fn test_list_bindings_for_user() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        store.create_user_binding(&test_binding("user-1", "agent-1", BindingScope::Full))?;
        store.create_user_binding(&test_binding("user-1", "agent-2", BindingScope::Restricted))?;
        store.create_user_binding(&test_binding("user-2", "agent-1", BindingScope::TaskOnly))?;

        let bindings = store.list_bindings_for_user("user-1")?;
        assert_eq!(bindings.len(), 2);

        Ok(())
    }

    #[test]
    fn test_delete_user_binding() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let store = GatewayStore::open(temp_dir.path())?;

        store.create_user_binding(&test_binding("user-1", "agent-1", BindingScope::Full))?;
        assert!(store.delete_user_binding("user-1", "agent-1")?);
        assert!(!store.delete_user_binding("user-1", "agent-1")?);

        let loaded = store.get_user_binding("user-1", "agent-1")?;
        assert!(loaded.is_none());

        Ok(())
    }
}
