//! Stack budget guard for `GatewayServer::run`'s startup path (#916).
//!
//! Sibling of `guard/router_dispatch_stack_budget.rs` (#884), for a different
//! frame. `run` is one `async fn` that joins every listener and scheduler; its
//! poll frame is sized for the widest path whether or not that path runs.
//! Startup then performs the two deepest call chains in the process on what is
//! left — `serde_json` over the constitution lock (nested
//! `#[derive(Deserialize)]`, dozens of non-inlined frames), and an `ed25519`
//! verification that descends into curve25519-dalek's AVX2 backend (~40 KiB,
//! debug).
//!
//! Before #914 boxed the joined futures, this path had ~3 KiB of headroom
//! against a 2 MiB test thread, so an unrelated 3.2 KiB elsewhere aborted
//! startup as `fatal runtime error: stack overflow, aborting` — with no test
//! name attached, and invisible to CI, which runs on the 8 MiB main thread.
//! This pins the result.
//!
//! An overflow aborts the process, which would take the whole test binary with
//! it, so the bounded run happens in a **child process** and the parent turns
//! a non-zero exit into a normal test failure with an actionable message.

/// Stack the startup path must fit inside.
///
/// **If this fails, do not raise the budget.** Find what grew `run`'s frame
/// and move it off — `Box::pin` a joined future (#914's shape), or extract the
/// block into a method (#884's shape) — and file it against #916. Raising the
/// number buys a few weeks and then the aborts come back somewhere with no
/// name on them.
const STACK_BUDGET_BYTES: usize = 1_024 * 1024;

const CHILD_TEST: &str = "bounded_stack_child";
const CHILD_ENV: &str = "AUTONOETIC_BOOTSTRAP_STACK_BUDGET_CHILD";

mod support;

#[test]
fn gateway_bootstrap_fits_within_the_stack_budget() {
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
        "gateway startup no longer fits in {} KiB of stack.\n\
         `GatewayServer::run` is one frame sized for every path it joins, and \
         startup runs a deep serde parse + ed25519 verify inside it. This is the \
         early warning for the unattributable `has overflowed its stack` aborts \
         (#836, #882, #914).\n\
         Fix by moving work off that frame (Box::pin a joined future, extract a \
         block), not by raising STACK_BUDGET_BYTES.\n\
         --- child stdout ---\n{}\n--- child stderr ---\n{}",
        STACK_BUDGET_BYTES / 1024,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// The bounded run. `#[ignore]` because it is only meaningful when the parent
/// invokes it by name in its own process; the thread is spawned with an
/// explicit `stack_size` rather than relying on `RUST_MIN_STACK`, so the
/// budget holds regardless of how the harness was invoked (CI passes
/// `--test-threads=1`, which would otherwise run tests on the 8 MiB main
/// thread and make this vacuous).
///
/// Harness reused from `constitution/r_8_6_retention_policy_startup.rs`: drive
/// startup through the deep paths (retention pass, constitution snapshot +
/// signature verify) and force a fast, *expected* failure by pre-binding the
/// OFP port. Reaching `address already in use` proves the entire pre-bind
/// bootstrap fit the budget — anything short of it and we would be guarding
/// nothing.
#[test]
#[ignore]
fn bounded_stack_child() {
    use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
    use autonoetic_gateway::server::GatewayServer;
    use autonoetic_types::config::GatewayConfig;

    let workspace = crate::support::TestWorkspace::new().expect("test workspace");
    let mut config: GatewayConfig = workspace.gateway_config();
    config.retention.causal_events_days = 1;
    config.retention.execution_traces_days = 0;

    let gateway_dir = config.runtime_dir.clone();
    std::fs::create_dir_all(&gateway_dir).expect("gateway dir");

    // Same secret env real startup expects (constitution snapshot + vault init
    // live on `run`'s frame — they are the deep chains this guard exists for).
    // Set before spawning the bounded thread: process-wide either way, and the
    // deep consumers read them from within it.
    std::env::set_var("AUTONOETIC_SHARED_SECRET", "test-shared-secret");
    std::env::set_var(
        "AUTONOETIC_VAULT_KEY",
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    );
    std::env::set_var("AUTONOETIC_VAULT_KEY_PATH", "");

    // Pre-bind the OFP port so startup fails fast at bind time — AFTER the
    // retention pass and constitution work we want on the frame.
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("probe listener");
    let occupied_port = occupied.local_addr().expect("local addr").port();
    config.ofp_port = occupied_port;
    config.port = 0;

    let handle = std::thread::Builder::new()
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            let err = rt
                .block_on(async move { GatewayServer::new(config).run().await })
                .expect_err("server should fail on the pre-bound OFP port");
            let err_text = err.to_string();
            assert!(
                err_text.contains("Address already in use")
                    || err_text.contains("address already in use"),
                "expected the pre-bound OFP port to stop startup after bootstrap, got: {err_text}"
            );

            // Startup progressed past the retention pass — same assertion
            // shape as the retention startup test, proving we got through the
            // real bootstrap rather than bailing early.
            let store_after =
                GatewayStore::open(&std::path::PathBuf::from(gateway_dir)).expect("reopen store");
            let events = store_after
                .search_causal_events(None, None, 200)
                .expect("search causal events");
            assert!(
                !events.is_empty(),
                "startup must have progressed far enough to emit causal events"
            );
        })
        .expect("bounded-stack thread should spawn");

    // Hold the probe binding for the whole bounded run.
    handle
        .join()
        .expect("bootstrap must fit the stack budget (see the parent test's message)");
}
