//! `session.rate` / `session.outcome.get` / `session.export` service layer
//! (#1119 tranche 1) — the logic behind the JSON-RPC methods the CLI's
//! `autonoetic session rate|show|export` now calls, so the CLI stops reading
//! gateway.db directly.
//!
//! Note on coverage shape: these are exercised through
//! `GatewayExecutionService` directly rather than a second in-process
//! `JsonRpcRouter`. The session binary already hosts one shared router
//! (`timeline_jsonrpc`), and a second concurrent router initialization in
//! the same process flaked the timeline suite (global init races); the
//! router arms for these methods are thin param-decode + delegation
//! wrappers around exactly these service methods.

use autonoetic_gateway::execution::GatewayExecutionService;
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use std::sync::Arc;

use crate::support::TestWorkspace;

fn service() -> &'static GatewayExecutionService {
    static SERVICE: std::sync::OnceLock<GatewayExecutionService> = std::sync::OnceLock::new();
    SERVICE.get_or_init(|| {
        let ws = TestWorkspace::new().expect("workspace");
        let config = ws.gateway_config();
        let store = Arc::new(GatewayStore::open(ws.path()).expect("store open"));
        // The workspace leaks deliberately: the service (and its store) must
        // outlive the tests sharing this OnceLock.
        std::mem::forget(ws);
        GatewayExecutionService::new(config, Some(store))
    })
}

const ROOT: &str = "sess-outcome-rpc-root";

fn seed() -> &'static GatewayExecutionService {
    let svc = service();
    svc.upsert_session_outcome_metrics_for_test(ROOT);
    svc
}

/// Test-only seeding helper — mirrors what session close writes, without
/// pulling store internals into this test file.
trait SeedOutcome {
    fn upsert_session_outcome_metrics_for_test(&self, root: &str);
}

impl SeedOutcome for GatewayExecutionService {
    fn upsert_session_outcome_metrics_for_test(&self, root: &str) {
        let store = self.gateway_store().expect("store");
        store
            .upsert_session_outcome_metrics(
                root,
                root,
                "planner.default",
                Some("rate/show/export over RPC"),
                3,
                1234,
                0.001,
                5.0,
            )
            .expect("seed outcome");
    }
}

#[tokio::test]
async fn outcome_get_returns_the_seeded_row() {
    seed();
    let row = service()
        .get_session_outcome_row(ROOT)
        .expect("outcome row")
        .expect("seeded row present");
    assert_eq!(row["session_id"].as_str(), Some(ROOT));
    assert_eq!(row["turns"].as_u64(), Some(3));
}

#[tokio::test]
async fn outcome_get_missing_session_is_none() {
    let row = service()
        .get_session_outcome_row("no-such-session")
        .expect("query must not fail");
    assert!(row.is_none(), "unseeded session must return None: {row:?}");
}

#[tokio::test]
async fn rate_records_thumb_and_note_then_outcome_get_shows_it() {
    seed();
    service()
        .rate_session_outcome(
            ROOT,
            autonoetic_types::session_outcome::OperatorThumb::Down,
            Some("went sideways"),
        )
        .expect("rate");

    let row = service()
        .get_session_outcome_row(ROOT)
        .expect("outcome row")
        .expect("seeded row present");
    assert_eq!(
        row["operator_rating"]["thumb"].as_str(),
        Some("down"),
        "rating not reflected: {row}"
    );
}

#[tokio::test]
async fn export_roundtrips_the_full_payload() {
    seed();
    let value = service()
        .export_full_session(
            ROOT,
            &autonoetic_gateway::runtime::session_export::ExportOptions {
                format: autonoetic_gateway::runtime::session_export::ExportFormat::Json,
                ..Default::default()
            },
        )
        .expect("export");
    assert_eq!(value["root_session_id"].as_str(), Some(ROOT));
    // The payload must deserialize client-side — that is the CLI's contract.
    let export: autonoetic_gateway::runtime::session_export::SessionExport =
        serde_json::from_value(value).expect("SessionExport must roundtrip");
    assert_eq!(export.root_session_id, ROOT);
    assert_eq!(export.outcome.as_ref().expect("outcome").session_id, ROOT);
}
