use super::GatewayStore;
use anyhow::Result;
use rusqlite::params;

fn row_to_credential(
    row: &rusqlite::Row,
) -> std::result::Result<autonoetic_types::agent::CredentialRecord, rusqlite::Error> {
    use autonoetic_types::agent::CredentialRecord;
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
    let refresh_headers: Option<std::collections::HashMap<String, String>> = row
        .get::<_, Option<String>>(12)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());
    Ok(CredentialRecord {
        credential_id: row.get(0)?,
        service: row.get(1)?,
        secret_name: row.get(2)?,
        inject_as: row.get(3)?,
        created_by_agent: row.get(4)?,
        expires_at: row.get(5)?,
        shared_with,
        allowed_hosts,
        refresh_token_secret_name: row.get(8)?,
        refresh_url: row.get(9)?,
        refresh_method: row.get(10)?,
        refresh_headers,
        refresh_extract_access_token: row.get(11)?,
        refresh_extract_refresh_token: row.get(13)?,
        refresh_extract_expires_in: row.get(14)?,
    })
}

const CREDENTIAL_COLUMNS: &str = "credential_id, service, secret_name, inject_as, created_by_agent, expires_at, shared_with, allowed_hosts, refresh_token_secret_name, refresh_url, refresh_method, refresh_headers, refresh_extract_access_token, refresh_extract_refresh_token, refresh_extract_expires_in";

impl GatewayStore {
    pub fn upsert_credential(
        &self,
        cred: &autonoetic_types::agent::CredentialRecord,
    ) -> Result<()> {
        let shared_with_json = serde_json::to_string(&cred.shared_with).unwrap_or_default();
        let allowed_hosts_json = serde_json::to_string(&cred.allowed_hosts).unwrap_or_default();
        let refresh_headers_json = cred
            .refresh_headers
            .as_ref()
            .map(|h| serde_json::to_string(h).unwrap_or_default())
            .unwrap_or_default();
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            &format!(
                "INSERT INTO credentials ({CREDENTIAL_COLUMNS}, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
             ON CONFLICT(credential_id) DO UPDATE SET
                service = excluded.service,
                secret_name = excluded.secret_name,
                inject_as = excluded.inject_as,
                expires_at = excluded.expires_at,
                shared_with = excluded.shared_with,
                allowed_hosts = excluded.allowed_hosts,
                refresh_token_secret_name = excluded.refresh_token_secret_name,
                refresh_url = excluded.refresh_url,
                refresh_method = excluded.refresh_method,
                refresh_headers = excluded.refresh_headers,
                refresh_extract_access_token = excluded.refresh_extract_access_token,
                refresh_extract_refresh_token = excluded.refresh_extract_refresh_token,
                refresh_extract_expires_in = excluded.refresh_extract_expires_in,
                updated_at = excluded.updated_at"
            ),
            params![
                cred.credential_id,
                cred.service,
                cred.secret_name,
                cred.inject_as,
                cred.created_by_agent,
                cred.expires_at,
                shared_with_json,
                allowed_hosts_json,
                cred.refresh_token_secret_name,
                cred.refresh_url,
                cred.refresh_method,
                refresh_headers_json,
                cred.refresh_extract_access_token,
                cred.refresh_extract_refresh_token,
                cred.refresh_extract_expires_in,
                now,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn get_credential(
        &self,
        credential_id: &str,
    ) -> Result<Option<autonoetic_types::agent::CredentialRecord>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            &format!("SELECT {CREDENTIAL_COLUMNS} FROM credentials WHERE credential_id = ?1"),
            params![credential_id],
            row_to_credential,
        );
        match row {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn list_credentials_by_service(
        &self,
        service: &str,
    ) -> Result<Vec<autonoetic_types::agent::CredentialRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM credentials WHERE service = ?1 ORDER BY created_at ASC"
        ))?;
        let rows = stmt.query_map(params![service], row_to_credential)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!(e))
    }

    pub fn list_all_credentials(&self) -> Result<Vec<autonoetic_types::agent::CredentialRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {CREDENTIAL_COLUMNS} FROM credentials ORDER BY service, credential_id"
        ))?;
        let rows = stmt.query_map([], row_to_credential)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!(e))
    }

    /// Delete a credential by ID.
    pub fn delete_credential(&self, credential_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        let n = tx.execute(
            "DELETE FROM credentials WHERE credential_id = ?1",
            params![credential_id],
        )?;
        // Best-effort cleanup for any in-progress onboarding state tied to this credential.
        let _ = tx.execute(
            "DELETE FROM credential_setup_state WHERE credential_id = ?1",
            params![credential_id],
        );
        tx.commit()?;
        Ok(n > 0)
    }

    // -----------------------------------------------------------------------
    // credential_setup_state — persists multi-step onboarding resume state
    // -----------------------------------------------------------------------

    /// Persist (insert or replace) the in-progress setup state for a credential.
    pub fn save_credential_setup_state(&self, credential_id: &str, state_json: &str) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO credential_setup_state (credential_id, state_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(credential_id) DO UPDATE SET
                state_json = excluded.state_json,
                updated_at = excluded.updated_at",
            params![credential_id, state_json, now, now],
        )?;
        Ok(())
    }

    /// Load the in-progress setup state for a credential, if any.
    pub fn load_credential_setup_state(&self, credential_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT state_json FROM credential_setup_state WHERE credential_id = ?1",
            params![credential_id],
            |row| row.get::<_, String>(0),
        );
        match row {
            Ok(s) => Ok(Some(s)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Delete the setup state when onboarding is complete or the credential is deleted.
    pub fn delete_credential_setup_state(&self, credential_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM credential_setup_state WHERE credential_id = ?1",
            params![credential_id],
        )?;
        Ok(n > 0)
    }
}
