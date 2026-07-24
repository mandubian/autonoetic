//! Stack budget guard for `JsonRpcRouter::dispatch` (#884, after the #882
//! regression).
//!
//! `dispatch` is one `async fn` holding a 62-arm match. Locals that do not cross
//! an await live in its poll frame, and the frame is sized at entry for the
//! *widest* path — so it grows with every arm, whether or not that arm runs.
//! Measured on this tree, one `event.ingest` dispatch needs between 1 MiB and
//! 1.5 MiB of stack, against the 2 MiB that libtest gives a test thread. Adding
//! one ~200-line arm inline was enough to cross it (#882), and the failure mode
//! is a process abort with no test name attached:
//!
//! ```text
//! thread '<unknown>' has overflowed its stack
//! fatal runtime error: stack overflow, aborting
//! ```
//!
//! Note what does *not* work as a guard: the future's size is unchanged by
//! extracting arms (56,800 bytes before and after the #882 fix), because the
//! cost is in the poll frame, not the state machine. So this guard measures the
//! real quantity — it runs a dispatch on a thread with a fixed, deliberately
//! tight stack.
//!
//! An overflow aborts the process, which would take the whole test binary with
//! it, so the bounded run happens in a **child process** and the parent turns a
//! non-zero exit into a normal test failure with an actionable message.

/// Stack the dispatch path must fit inside. Deliberately below the 2 MiB libtest
/// default so this guard trips *before* unrelated router tests start aborting.
///
/// **If this fails, do not raise the budget** — extract the offending handler
/// into a method (see `handle_curation_run_for_session` for the shape) and file
/// it against #884. Raising the number buys a few weeks and then the aborts come
/// back somewhere with no name on them.
const STACK_BUDGET_BYTES: usize = 1_792 * 1024;

const CHILD_TEST: &str = "bounded_stack_child";
const CHILD_ENV: &str = "AUTONOETIC_STACK_BUDGET_CHILD";

#[test]
fn dispatch_fits_within_the_stack_budget() {
    // Guard against a child re-spawning itself.
    if std::env::var(CHILD_ENV).is_ok() {
        return;
    }
    let exe = std::env::current_exe().expect("test binary path");
    let output = std::process::Command::new(exe)
        .args(["--exact", CHILD_TEST, "--ignored", "--nocapture"])
        .env(CHILD_ENV, "1")
        .output()
        .expect("child test process should spawn");

    assert!(
        output.status.success(),
        "a JSON-RPC dispatch no longer fits in {} KiB of stack.\n\
         `dispatch` is one function whose frame is the sum of all 62 match arms, \
         and libtest threads get 2 MiB — so this is the early warning for the \
         unattributable `has overflowed its stack` aborts.\n\
         Fix by extracting the largest arms into methods (#884), not by raising \
         STACK_BUDGET_BYTES.\n\
         --- child stdout ---\n{}\n--- child stderr ---\n{}",
        STACK_BUDGET_BYTES / 1024,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

mod support;

/// The bounded run. `#[ignore]` because it is only meaningful when the parent
/// invokes it by name in its own process; the thread is spawned with an explicit
/// `stack_size` rather than relying on `RUST_MIN_STACK`, so the budget holds
/// regardless of how the harness was invoked (CI passes `--test-threads=1`,
/// which would otherwise run tests on the 8 MiB main thread and make this
/// vacuous).
#[test]
#[ignore]
fn bounded_stack_child() {
    use autonoetic_gateway::router::{JsonRpcRequest, JsonRpcRouter};
    use support::TestWorkspace;

    let handle = std::thread::Builder::new()
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(|| {
            let ws = TestWorkspace::new().expect("workspace");
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            rt.block_on(async move {
                let router = JsonRpcRouter::new(ws.gateway_config(), None);
                // event.ingest is the widest arm (307 lines) and the path the
                // #882 overflow surfaced on.
                let req = JsonRpcRequest {
                    jsonrpc: "2.0".to_string(),
                    id: "stack-budget".to_string(),
                    method: "event.ingest".to_string(),
                    params: serde_json::json!({
                        "event_type": "approval.resolved",
                        "message": "stack budget probe",
                        "request_id": "stack-budget-req",
                        "payload": { "approval_id": "apr-probe", "status": "approved" }
                    }),
                    auth_token: None,
                };
                let resp = router.dispatch(req).await;
                assert!(
                    resp.result.is_some() || resp.error.is_some(),
                    "dispatch should return a response"
                );
            });
        })
        .expect("bounded-stack thread should spawn");
    handle.join().expect("bounded-stack thread should not abort");
}
