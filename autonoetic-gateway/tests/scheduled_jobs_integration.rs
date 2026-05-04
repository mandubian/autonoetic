//! Integration tests for scheduled jobs (cron) lifecycle.
//!
//! Tests cover:
//! - Job creation, listing, pause, resume, cancel
//! - Due job claiming and next-run advancement
//! - Atomic claim-and-advance dedup semantics
//! - Ownership enforcement
//! - Enqueue failure backoff
//! - Min interval guardrail

use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::scheduled_job::{ScheduledJob, ScheduledJobStatus};
use chrono::{Duration, Utc};
use std::sync::Arc;

fn temp_gateway_store() -> (tempfile::TempDir, Arc<GatewayStore>) {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(GatewayStore::open(temp.path()).unwrap());
    (temp, store)
}

fn make_job(job_id: &str, owner: &str, next_run_at: &str) -> ScheduledJob {
    ScheduledJob {
        job_id: job_id.to_string(),
        owner_agent_id: owner.to_string(),
        root_session_id: "root-1".to_string(),
        target_agent_id: "coder.default".to_string(),
        target_revision_id:
            "rev_sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .to_string(),
        message: "Do the thing".to_string(),
        metadata_json: None,
        cron_expr: "*/5 * * * *".to_string(),
        timezone: "UTC".to_string(),
        next_run_at: next_run_at.to_string(),
        last_run_at: None,
        status: ScheduledJobStatus::Active,
        created_at: Utc::now().to_rfc3339(),
        updated_at: Utc::now().to_rfc3339(),
        last_error: None,
        generation: 0,
    }
}

#[test]
fn test_scheduled_job_crud() -> anyhow::Result<()> {
    let (_temp, store) = temp_gateway_store();

    let job = make_job("sj-test-001", "planner.default", &Utc::now().to_rfc3339());
    store.create_scheduled_job(&job)?;

    let fetched = store.get_scheduled_job("sj-test-001")?;
    assert!(fetched.is_some());
    let fetched = fetched.unwrap();
    assert_eq!(fetched.job_id, "sj-test-001");
    assert_eq!(fetched.owner_agent_id, "planner.default");
    assert_eq!(fetched.status, ScheduledJobStatus::Active);

    let jobs = store.list_scheduled_jobs_for_owner("planner.default", None, None)?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].job_id, "sj-test-001");

    Ok(())
}

#[test]
fn test_scheduled_job_pause_resume_cancel() -> anyhow::Result<()> {
    let (_temp, store) = temp_gateway_store();

    let job = make_job("sj-test-002", "planner.default", &Utc::now().to_rfc3339());
    store.create_scheduled_job(&job)?;

    let paused = store.pause_scheduled_job("sj-test-002")?;
    assert!(paused);

    let job = store.get_scheduled_job("sj-test-002")?;
    assert_eq!(job.unwrap().status, ScheduledJobStatus::Paused);

    let resumed = store.resume_scheduled_job("sj-test-002")?;
    assert!(resumed);

    let job = store.get_scheduled_job("sj-test-002")?;
    assert_eq!(job.unwrap().status, ScheduledJobStatus::Active);

    let cancelled = store.cancel_scheduled_job("sj-test-002")?;
    assert!(cancelled);

    let job = store.get_scheduled_job("sj-test-002")?;
    assert_eq!(job.unwrap().status, ScheduledJobStatus::Cancelled);

    let not_cancelled_again = store.cancel_scheduled_job("sj-test-002")?;
    assert!(!not_cancelled_again);

    Ok(())
}

#[test]
fn test_load_due_scheduled_jobs() -> anyhow::Result<()> {
    let (_temp, store) = temp_gateway_store();

    let now = Utc::now();
    let past = (now - Duration::minutes(5)).to_rfc3339();
    let future = (now + Duration::minutes(5)).to_rfc3339();

    let job1 = make_job("sj-due-001", "planner.default", &past);
    let job2 = make_job("sj-due-002", "planner.default", &past);
    let job3 = make_job("sj-due-003", "planner.default", &future);

    store.create_scheduled_job(&job1)?;
    store.create_scheduled_job(&job2)?;
    store.create_scheduled_job(&job3)?;

    let due = store.load_due_scheduled_jobs(&now.to_rfc3339(), 10)?;
    assert_eq!(due.len(), 2);

    let ids: Vec<&str> = due.iter().map(|j| j.job_id.as_str()).collect();
    assert!(ids.contains(&"sj-due-001"));
    assert!(ids.contains(&"sj-due-002"));
    assert!(!ids.contains(&"sj-due-003"));

    Ok(())
}

#[test]
fn test_claim_due_scheduled_job_dedup() -> anyhow::Result<()> {
    let (_temp, store) = temp_gateway_store();

    let now = Utc::now();
    let past = (now - Duration::minutes(5)).to_rfc3339();
    let future = (now + Duration::minutes(5)).to_rfc3339();

    let job = make_job("sj-claim-001", "planner.default", &past);
    store.create_scheduled_job(&job)?;

    let claimed1 = store.claim_due_scheduled_job("sj-claim-001", &now.to_rfc3339())?;
    assert!(claimed1.is_some());

    let claimed2 = store.claim_due_scheduled_job("sj-claim-001", &now.to_rfc3339())?;
    assert!(claimed2.is_some());

    store.advance_next_run("sj-claim-001", &future, Some(&now.to_rfc3339()), None)?;

    let claimed3 = store.claim_due_scheduled_job("sj-claim-001", &now.to_rfc3339())?;
    assert!(claimed3.is_none());

    Ok(())
}

#[test]
fn test_advance_next_run() -> anyhow::Result<()> {
    let (_temp, store) = temp_gateway_store();

    let now = Utc::now();
    let next = (now + Duration::minutes(5)).to_rfc3339();

    let job = make_job("sj-advance-001", "planner.default", &now.to_rfc3339());
    store.create_scheduled_job(&job)?;

    store.advance_next_run("sj-advance-001", &next, Some(&now.to_rfc3339()), None)?;

    let job = store.get_scheduled_job("sj-advance-001")?;
    let job = job.unwrap();
    assert_eq!(job.next_run_at, next);
    assert_eq!(job.last_run_at, Some(now.to_rfc3339()));
    assert!(job.last_error.is_none());

    Ok(())
}

#[test]
fn test_list_scheduled_jobs_for_root() -> anyhow::Result<()> {
    let (_temp, store) = temp_gateway_store();

    let now = Utc::now();

    let job1 = make_job("sj-root-001", "planner.default", &now.to_rfc3339());
    let job2 = make_job("sj-root-002", "coder.default", &now.to_rfc3339());

    store.create_scheduled_job(&job1)?;
    store.create_scheduled_job(&job2)?;

    let root_jobs = store.list_scheduled_jobs_for_root("root-1")?;
    assert_eq!(root_jobs.len(), 2);

    let root_jobs_empty = store.list_scheduled_jobs_for_root("root-2")?;
    assert!(root_jobs_empty.is_empty());

    Ok(())
}

#[test]
fn test_delete_scheduled_job() -> anyhow::Result<()> {
    let (_temp, store) = temp_gateway_store();

    let job = make_job("sj-delete-001", "planner.default", &Utc::now().to_rfc3339());
    store.create_scheduled_job(&job)?;

    let deleted = store.delete_scheduled_job("sj-delete-001")?;
    assert!(deleted);

    let not_found = store.delete_scheduled_job("sj-delete-001")?;
    assert!(!not_found);

    Ok(())
}

#[test]
fn test_pagination() -> anyhow::Result<()> {
    let (_temp, store) = temp_gateway_store();

    let now = Utc::now();

    for i in 0..5 {
        let job = make_job(
            &format!("sj-page-{:03}", i),
            "planner.default",
            &now.to_rfc3339(),
        );
        store.create_scheduled_job(&job)?;
    }

    let all = store.list_scheduled_jobs_for_owner("planner.default", None, None)?;
    assert_eq!(all.len(), 5);

    let page1 = store.list_scheduled_jobs_for_owner("planner.default", Some(2), Some(0))?;
    assert_eq!(page1.len(), 2);

    let page2 = store.list_scheduled_jobs_for_owner("planner.default", Some(2), Some(2))?;
    assert_eq!(page2.len(), 2);

    let page3 = store.list_scheduled_jobs_for_owner("planner.default", Some(2), Some(4))?;
    assert_eq!(page3.len(), 1);

    Ok(())
}

#[test]
fn test_claim_and_advance_atomic() -> anyhow::Result<()> {
    let (_temp, store) = temp_gateway_store();

    let now = Utc::now();
    let past = (now - Duration::minutes(5)).to_rfc3339();
    let future = (now + Duration::minutes(5)).to_rfc3339();

    let job = make_job("sj-atomic-001", "planner.default", &past);
    store.create_scheduled_job(&job)?;

    let claimed = store.claim_and_advance_due_job("sj-atomic-001", &now.to_rfc3339(), &future)?;
    assert!(claimed.is_some());

    let job = store.get_scheduled_job("sj-atomic-001")?;
    let job = job.unwrap();
    assert_eq!(job.next_run_at, future);
    assert_eq!(job.last_run_at, Some(now.to_rfc3339()));

    let claimed2 = store.claim_and_advance_due_job("sj-atomic-001", &now.to_rfc3339(), &future)?;
    assert!(claimed2.is_none());

    Ok(())
}

#[test]
fn test_enqueue_failure_backoff() -> anyhow::Result<()> {
    let (_temp, store) = temp_gateway_store();

    let now = Utc::now();
    let past = (now - Duration::minutes(5)).to_rfc3339();

    let job = make_job("sj-backoff-001", "planner.default", &past);
    store.create_scheduled_job(&job)?;

    let backoff_secs = 60;
    let retry_at = (now + Duration::seconds(backoff_secs)).to_rfc3339();
    let error_msg = "Enqueue failed: simulated error";
    store.advance_next_run("sj-backoff-001", &retry_at, None, Some(error_msg))?;

    let job = store.get_scheduled_job("sj-backoff-001")?;
    let job = job.unwrap();
    assert_eq!(job.next_run_at, retry_at);
    assert_eq!(job.last_error, Some(error_msg.to_string()));

    let due_now = store.load_due_scheduled_jobs(&now.to_rfc3339(), 10)?;
    assert!(due_now.is_empty());

    let due_later = store.load_due_scheduled_jobs(&retry_at, 10)?;
    assert_eq!(due_later.len(), 1);

    Ok(())
}

#[test]
fn test_ownership_isolation() -> anyhow::Result<()> {
    let (_temp, store) = temp_gateway_store();

    let now = Utc::now();

    let job1 = make_job("sj-owner-001", "planner.default", &now.to_rfc3339());
    let job2 = make_job("sj-owner-002", "coder.default", &now.to_rfc3339());
    store.create_scheduled_job(&job1)?;
    store.create_scheduled_job(&job2)?;

    let planner_jobs = store.list_scheduled_jobs_for_owner("planner.default", None, None)?;
    assert_eq!(planner_jobs.len(), 1);
    assert_eq!(planner_jobs[0].job_id, "sj-owner-001");

    let coder_jobs = store.list_scheduled_jobs_for_owner("coder.default", None, None)?;
    assert_eq!(coder_jobs.len(), 1);
    assert_eq!(coder_jobs[0].job_id, "sj-owner-002");

    Ok(())
}
