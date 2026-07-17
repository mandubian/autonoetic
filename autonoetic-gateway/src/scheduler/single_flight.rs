use crate::scheduler::gateway_store::GatewayStore;
use crate::scheduler::store::{read_json_file, write_json_file};
use crate::scheduler::workflow_store::{load_task_run, workflow_run_dir, workflows_root};
use autonoetic_types::config::GatewayConfig;
use autonoetic_types::workflow::TaskRunStatus;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

const PENDING_RESERVATION_FRESHNESS_MINUTES: i64 = 5;

#[derive(Debug, Clone)]
pub struct DurableOperationSpec {
    pub workflow_id: String,
    pub dedupe_key: String,
    pub stage_kind: String,
    pub agent_id: String,
    pub artifact_ref: Option<String>,
    pub intent_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SingleFlightReservation {
    pub workflow_id: String,
    pub dedupe_key: String,
    pub stage_kind: String,
    pub agent_id: String,
    pub artifact_ref: Option<String>,
    pub intent_digest: Option<String>,
    pub task_id: Option<String>,
    pub approval_request_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct CoalescedOperation {
    pub dedupe_key: String,
    pub stage_kind: String,
    pub existing_task_id: Option<String>,
    pub approval_request_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum AcquireOutcome {
    Acquired(SingleFlightReservation),
    Coalesced(CoalescedOperation),
}

fn coalesced_operation(existing: SingleFlightReservation) -> AcquireOutcome {
    AcquireOutcome::Coalesced(CoalescedOperation {
        dedupe_key: existing.dedupe_key,
        stage_kind: existing.stage_kind,
        existing_task_id: existing.task_id,
        approval_request_id: existing.approval_request_id,
    })
}

pub fn durable_operation_for_spawn(
    workflow_id: &str,
    agent_id: &str,
    metadata: Option<&serde_json::Value>,
    artifact_ref: Option<&str>,
    message: &str,
) -> Option<DurableOperationSpec> {
    let object = metadata?.as_object()?;
    let stage_kind = object.get("stage_kind")?.as_str()?;
    if !is_durable_stage_kind(stage_kind) {
        return None;
    }

    let artifact_ref = object
        .get("artifact_ref")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
        .or_else(|| artifact_ref.map(ToOwned::to_owned));
    let intent_digest = if artifact_ref.is_none() && stage_kind == "install" {
        Some(normalize_intent_digest(message))
    } else {
        None
    };

    Some(build_spec(
        workflow_id,
        stage_kind,
        agent_id,
        artifact_ref,
        intent_digest,
    ))
}

pub fn build_spec(
    workflow_id: &str,
    stage_kind: &str,
    agent_id: &str,
    artifact_ref: Option<String>,
    intent_digest: Option<String>,
) -> DurableOperationSpec {
    let dedupe_subject = artifact_ref
        .clone()
        .or_else(|| intent_digest.clone())
        .unwrap_or_else(|| "none".to_string());
    DurableOperationSpec {
        workflow_id: workflow_id.to_string(),
        dedupe_key: format!(
            "{}:{}:{}:{}",
            workflow_id, stage_kind, agent_id, dedupe_subject
        ),
        stage_kind: stage_kind.to_string(),
        agent_id: agent_id.to_string(),
        artifact_ref,
        intent_digest,
    }
}

pub fn try_acquire_reservation(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    spec: &DurableOperationSpec,
    task_id: Option<&str>,
    ignore_approval_request_id: Option<&str>,
) -> anyhow::Result<AcquireOutcome> {
    let reservation = SingleFlightReservation {
        workflow_id: spec.workflow_id.clone(),
        dedupe_key: spec.dedupe_key.clone(),
        stage_kind: spec.stage_kind.clone(),
        agent_id: spec.agent_id.clone(),
        artifact_ref: spec.artifact_ref.clone(),
        intent_digest: spec.intent_digest.clone(),
        task_id: task_id.map(ToOwned::to_owned),
        approval_request_id: None,
        created_at: Utc::now().to_rfc3339(),
        updated_at: Utc::now().to_rfc3339(),
    };

    let path = reservation_path(config, &spec.workflow_id, &spec.dedupe_key);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    for _ in 0..2 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                let bytes = serde_json::to_vec_pretty(&reservation)?;
                file.write_all(&bytes)?;
                file.sync_all()?;
                return Ok(AcquireOutcome::Acquired(reservation.clone()));
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                let existing: SingleFlightReservation = match read_json_file(&path) {
                    Ok(existing) => existing,
                    Err(_) => {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                };
                if reservation_is_active(config, store, &existing, ignore_approval_request_id)? {
                    return Ok(coalesced_operation(existing));
                }
                let _ = fs::remove_file(&path);
            }
            Err(error) => return Err(error.into()),
        }
    }

    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            let bytes = serde_json::to_vec_pretty(&reservation)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            return Ok(AcquireOutcome::Acquired(reservation));
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            if let Ok(existing) = read_json_file::<SingleFlightReservation>(&path) {
                if reservation_is_active(config, store, &existing, ignore_approval_request_id)? {
                    return Ok(coalesced_operation(existing));
                }
                let _ = fs::remove_file(&path);
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                {
                    let bytes = serde_json::to_vec_pretty(&reservation)?;
                    file.write_all(&bytes)?;
                    file.sync_all()?;
                    return Ok(AcquireOutcome::Acquired(reservation));
                }
            }
        }
        Err(error) => return Err(error.into()),
    }

    anyhow::bail!(
        "single-flight reservation for '{}' could not be acquired",
        spec.dedupe_key
    )
}

pub fn attach_approval_request(
    config: &GatewayConfig,
    workflow_id: &str,
    dedupe_key: &str,
    approval_request_id: &str,
) -> anyhow::Result<()> {
    let path = reservation_path(config, workflow_id, dedupe_key);
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?;
    let mut body = String::new();
    file.read_to_string(&mut body)?;
    let mut reservation: SingleFlightReservation = serde_json::from_str(&body)?;
    reservation.approval_request_id = Some(approval_request_id.to_string());
    reservation.updated_at = Utc::now().to_rfc3339();
    let encoded = serde_json::to_vec_pretty(&reservation)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    Ok(())
}

pub fn release_reservation(
    config: &GatewayConfig,
    workflow_id: &str,
    dedupe_key: &str,
) -> anyhow::Result<()> {
    let path = reservation_path(config, workflow_id, dedupe_key);
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

pub fn cleanup_stale_reservations(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
) -> anyhow::Result<u64> {
    let runs_dir = workflows_root(config).join("runs");
    if !runs_dir.is_dir() {
        return Ok(0);
    }

    let mut removed = 0u64;
    for workflow_entry in fs::read_dir(&runs_dir)? {
        let workflow_entry = workflow_entry?;
        if !workflow_entry.file_type()?.is_dir() {
            continue;
        }
        let workflow_id = workflow_entry.file_name().to_string_lossy().to_string();
        let dir = single_flight_dir(config, &workflow_id);
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let reservation: SingleFlightReservation = match read_json_file(&path) {
                Ok(reservation) => reservation,
                Err(_) => {
                    let _ = fs::remove_file(&path);
                    removed += 1;
                    continue;
                }
            };
            if !reservation_is_active(config, store, &reservation, None)? {
                fs::remove_file(&path)?;
                removed += 1;
            }
        }
    }

    Ok(removed)
}

fn reservation_is_active(
    config: &GatewayConfig,
    store: Option<&GatewayStore>,
    reservation: &SingleFlightReservation,
    ignore_approval_request_id: Option<&str>,
) -> anyhow::Result<bool> {
    if let Some(approval_request_id) = reservation.approval_request_id.as_deref() {
        if ignore_approval_request_id == Some(approval_request_id) {
            return Ok(false);
        }
        if let Some(store) = store {
            if let Some(approval) = store.get_approval(approval_request_id)? {
                let status = approval
                    .status
                    .as_ref()
                    .map(|value| value.as_str())
                    .unwrap_or("pending");
                if matches!(status, "pending" | "approved") {
                    return Ok(true);
                }
            }
        }
    }

    if let Some(task_id) = reservation.task_id.as_deref() {
        if let Some(task) = load_task_run(config, store, &reservation.workflow_id, task_id)? {
            return Ok(!task.status.is_terminal());
        }
    }

    Ok(reservation_is_fresh(reservation))
}

fn reservation_is_fresh(reservation: &SingleFlightReservation) -> bool {
    parse_timestamp(&reservation.updated_at)
        .map(|updated_at| {
            updated_at + Duration::minutes(PENDING_RESERVATION_FRESHNESS_MINUTES) > Utc::now()
        })
        .unwrap_or(false)
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

fn is_durable_stage_kind(stage_kind: &str) -> bool {
    matches!(
        stage_kind,
        "install" | "promote" | "rollback" | "durable_build"
    )
}

fn normalize_intent_digest(message: &str) -> String {
    let normalized = message
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    format!("intent:{}", hex::encode(hasher.finalize()))
}

fn single_flight_dir(config: &GatewayConfig, workflow_id: &str) -> PathBuf {
    workflow_run_dir(config, workflow_id).join("single_flight")
}

fn reservation_path(config: &GatewayConfig, workflow_id: &str, dedupe_key: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(dedupe_key.as_bytes());
    let key = hex::encode(hasher.finalize());
    single_flight_dir(config, workflow_id).join(format!("{key}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scheduler::workflow_store::{ensure_workflow_for_root_session, save_task_run};
    use autonoetic_types::workflow::{TaskRun, TaskRunStatus};
    use tempfile::tempdir;

    fn test_config(agents_dir: &std::path::Path) -> GatewayConfig {
        GatewayConfig {
            agents_dir: agents_dir.to_path_buf(),
            ..GatewayConfig::default()
        }
    }

    #[test]
    fn active_task_reservation_coalesces_duplicate_request() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let workflow = ensure_workflow_for_root_session(&cfg, None, "root-coalesce", None).unwrap();

        let spec = build_spec(
            &workflow.workflow_id,
            "durable_build",
            "builder.default",
            Some("ar.test-build".to_string()),
            None,
        );
        let first_task_id = "task-existing";
        match try_acquire_reservation(&cfg, None, &spec, Some(first_task_id), None).unwrap() {
            AcquireOutcome::Acquired(_) => {}
            AcquireOutcome::Coalesced(_) => panic!("first reservation must acquire"),
        }

        let now = Utc::now().to_rfc3339();
        save_task_run(
            &cfg,
            None,
            &TaskRun {
                task_id: first_task_id.to_string(),
                workflow_id: workflow.workflow_id.clone(),
                agent_id: "builder.default".to_string(),
                session_id: "root-coalesce/builder".to_string(),
                parent_session_id: "root-coalesce".to_string(),
                status: TaskRunStatus::Running,
                created_at: now.clone(),
                updated_at: now,
                source_agent_id: Some("planner.default".to_string()),
                result_summary: None,
                join_group: None,
                message: Some("build durable artifact".to_string()),
                metadata: Some(serde_json::json!({
                    "stage_kind": "durable_build",
                    "artifact_ref": "ar.test-build"
                })),
                retry_count: 0,
                last_failure_class: None,
                retry_policy: None,
                side_effect_state: None,
                dedupe_key: Some(spec.dedupe_key.clone()),
            },
        )
        .unwrap();

        match try_acquire_reservation(&cfg, None, &spec, Some("task-duplicate"), None).unwrap() {
            AcquireOutcome::Coalesced(existing) => {
                assert_eq!(existing.existing_task_id.as_deref(), Some(first_task_id));
                assert_eq!(existing.stage_kind, "durable_build");
            }
            AcquireOutcome::Acquired(_) => panic!("duplicate request should coalesce"),
        }
    }

    #[test]
    fn cleanup_stale_reservation_removes_terminal_task_entry() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let workflow = ensure_workflow_for_root_session(&cfg, None, "root-cleanup", None).unwrap();

        let spec = build_spec(
            &workflow.workflow_id,
            "promote",
            "specialized_builder.default",
            Some("ar.promote".to_string()),
            None,
        );
        let task_id = "task-terminal";
        match try_acquire_reservation(&cfg, None, &spec, Some(task_id), None).unwrap() {
            AcquireOutcome::Acquired(_) => {}
            AcquireOutcome::Coalesced(_) => panic!("first reservation must acquire"),
        }

        let now = Utc::now().to_rfc3339();
        save_task_run(
            &cfg,
            None,
            &TaskRun {
                task_id: task_id.to_string(),
                workflow_id: workflow.workflow_id.clone(),
                agent_id: "specialized_builder.default".to_string(),
                session_id: "root-cleanup/builder".to_string(),
                parent_session_id: "root-cleanup".to_string(),
                status: TaskRunStatus::Succeeded,
                created_at: now.clone(),
                updated_at: now,
                source_agent_id: Some("planner.default".to_string()),
                result_summary: Some("ok".to_string()),
                join_group: None,
                message: Some("promote durable artifact".to_string()),
                metadata: Some(serde_json::json!({
                    "stage_kind": "promote",
                    "artifact_ref": "ar.promote"
                })),
                retry_count: 0,
                last_failure_class: None,
                retry_policy: None,
                side_effect_state: None,
                dedupe_key: Some(spec.dedupe_key.clone()),
            },
        )
        .unwrap();

        let removed = cleanup_stale_reservations(&cfg, None).unwrap();
        assert_eq!(removed, 1);
        let path = reservation_path(&cfg, &workflow.workflow_id, &spec.dedupe_key);
        assert!(!path.exists());
    }

    #[test]
    fn pending_approval_reservation_coalesces_duplicate_request() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let workflow = ensure_workflow_for_root_session(&cfg, None, "root-approval", None).unwrap();
        let gateway_dir = crate::execution::gateway_root_dir(&cfg);
        let store = GatewayStore::open(&gateway_dir).unwrap();

        let spec = build_spec(
            &workflow.workflow_id,
            "promote",
            "specialized_builder.default",
            Some("ar.promote.approval".to_string()),
            None,
        );
        match try_acquire_reservation(&cfg, Some(&store), &spec, None, None).unwrap() {
            AcquireOutcome::Acquired(_) => {}
            AcquireOutcome::Coalesced(_) => panic!("first reservation must acquire"),
        }

        let approval_request_id = "apr-coalesced";
        let mut approval = autonoetic_types::background::ApprovalRequest {
            request_id: approval_request_id.to_string(),
            agent_id: "planner.default".to_string(),
            session_id: "root-approval".to_string(),
            root_session_id: Some("root-approval".to_string()),
            workflow_id: Some(workflow.workflow_id.clone()),
            task_id: None,
            action: autonoetic_types::background::ScheduledAction::SessionContinue {
                session_id: "root-approval".to_string(),
                root_session_id: "root-approval".to_string(),
                requested_by_agent_id: "planner.default".to_string(),
                turn_counter: 1,
                max_turns: 8,
                payload: None,
            },
            created_at: Utc::now().to_rfc3339(),
            status: None,
            decided_at: None,
            decided_by: None,
            reason: Some("promote approval".to_string()),
            evidence_ref: None,
            decision_reason: None,
            approval_level: autonoetic_types::background::ApprovalLevel::Operator,
            min_dwell_ms: None,
            confirm_phrase: None,
            code_excerpts: None,
            risk_summary: None,

            expires_at: None,
        };
        store.create_approval(&mut approval).unwrap();
        attach_approval_request(
            &cfg,
            &workflow.workflow_id,
            &spec.dedupe_key,
            approval_request_id,
        )
        .unwrap();

        match try_acquire_reservation(&cfg, Some(&store), &spec, None, None).unwrap() {
            AcquireOutcome::Coalesced(existing) => {
                assert_eq!(
                    existing.approval_request_id.as_deref(),
                    Some(approval_request_id)
                );
                assert!(existing.existing_task_id.is_none());
            }
            AcquireOutcome::Acquired(_) => panic!("duplicate approval request should coalesce"),
        }
    }

    #[test]
    fn stale_reservation_is_replaced_without_hard_error() {
        let dir = tempdir().unwrap();
        let agents = dir.path().join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let cfg = test_config(&agents);
        let workflow = ensure_workflow_for_root_session(&cfg, None, "root-stale", None).unwrap();

        let spec = build_spec(
            &workflow.workflow_id,
            "install",
            "builder.default",
            Some("ar.stale".to_string()),
            None,
        );
        let path = reservation_path(&cfg, &workflow.workflow_id, &spec.dedupe_key);
        let stale = SingleFlightReservation {
            workflow_id: workflow.workflow_id.clone(),
            dedupe_key: spec.dedupe_key.clone(),
            stage_kind: "install".to_string(),
            agent_id: "builder.default".to_string(),
            artifact_ref: Some("ar.stale".to_string()),
            intent_digest: None,
            task_id: None,
            approval_request_id: None,
            created_at: (Utc::now() - Duration::minutes(10)).to_rfc3339(),
            updated_at: (Utc::now() - Duration::minutes(10)).to_rfc3339(),
        };
        write_json_file(&path, &stale).unwrap();

        match try_acquire_reservation(&cfg, None, &spec, Some("task-fresh"), None).unwrap() {
            AcquireOutcome::Acquired(reservation) => {
                assert_eq!(reservation.task_id.as_deref(), Some("task-fresh"));
            }
            AcquireOutcome::Coalesced(_) => panic!("stale reservation should be replaced"),
        }
    }
}
