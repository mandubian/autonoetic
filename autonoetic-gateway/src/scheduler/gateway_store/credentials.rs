use super::GatewayStore;
use anyhow::Result;
use rusqlite::params;

impl GatewayStore {
    /// Store or update a credential record.
    pub fn upsert_credential(
        &self,
        cred: &autonoetic_types::agent::CredentialRecord,
    ) -> Result<()> {
        let shared_with_json = serde_json::to_string(&cred.shared_with).unwrap_or_default();
        let allowed_hosts_json = serde_json::to_string(&cred.allowed_hosts).unwrap_or_default();
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO credentials (credential_id, service, secret_name, inject_as, created_by_agent, expires_at, shared_with, allowed_hosts, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(credential_id) DO UPDATE SET
                service = excluded.service,
                secret_name = excluded.secret_name,
                inject_as = excluded.inject_as,
                expires_at = excluded.expires_at,
                shared_with = excluded.shared_with,
                allowed_hosts = excluded.allowed_hosts,
                updated_at = excluded.updated_at",
            params![
                cred.credential_id,
                cred.service,
                cred.secret_name,
                cred.inject_as,
                cred.created_by_agent,
                cred.expires_at,
                shared_with_json,
                allowed_hosts_json,
                now,
                now,
            ],
        )?;
        Ok(())
    }

    /// Get a credential by ID.
    pub fn get_credential(
        &self,
        credential_id: &str,
    ) -> Result<Option<autonoetic_types::agent::CredentialRecord>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT credential_id, service, secret_name, inject_as, created_by_agent, expires_at, shared_with, allowed_hosts
             FROM credentials WHERE credential_id = ?1",
            params![credential_id],
            |row| {
                let shared_with: Vec<String> = row
                    .get::<_, String>(6)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                let allowed_hosts: Vec<String> = row
                    .get::<_, String>(7)
                    .ok()
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default();
                Ok(autonoetic_types::agent::CredentialRecord {
                    credential_id: row.get(0)?,
                    service: row.get(1)?,
                    secret_name: row.get(2)?,
                    inject_as: row.get(3)?,
                    created_by_agent: row.get(4)?,
                    expires_at: row.get(5)?,
                    shared_with,
                    allowed_hosts,
                })
            },
        );
        match row {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// List credentials for a service.
    pub fn list_credentials_by_service(
        &self,
        service: &str,
    ) -> Result<Vec<autonoetic_types::agent::CredentialRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT credential_id, service, secret_name, inject_as, created_by_agent, expires_at, shared_with, allowed_hosts
             FROM credentials WHERE service = ?1",
        )?;
        let rows = stmt.query_map(params![service], |row| {
            let shared_with: Vec<String> = row
                .get::<_, String>(6)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            let allowed_hosts: Vec<String> = row
                .get::<_, String>(7)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            Ok(autonoetic_types::agent::CredentialRecord {
                credential_id: row.get(0)?,
                service: row.get(1)?,
                secret_name: row.get(2)?,
                inject_as: row.get(3)?,
                created_by_agent: row.get(4)?,
                expires_at: row.get(5)?,
                shared_with,
                allowed_hosts,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!(e))
    }

    /// Delete a credential by ID.
    pub fn delete_credential(&self, credential_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM credentials WHERE credential_id = ?1",
            params![credential_id],
        )?;
        Ok(n > 0)
    }
}
