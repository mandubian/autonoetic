//! Integration test for host-constant resolution through the approval chain.
//!
//! Reproduces the session-1a32cf14 failure shape: two sibling executor
//! sessions each run a *different* script (`gmail_read.py`, `gmail_latest.py`)
//! against the same IMAP host, with the hostname held in a module constant
//! (`HOST = "imap.gmail.com"`) and passed to `imaplib.IMAP4_SSL(HOST)`.
//!
//! Before host-constant resolution, `detected_hosts` came back empty, so:
//!   - no session grant was materialized on approval (approval.rs skips grant
//!     creation when `detected_hosts` is empty), and
//!   - coverage stayed `Unresolved`, keeping the exec cache out,
//! and the second sibling had to file a brand-new operator approval.
//!
//! This test pins the fixed chain: analyze → normalize_targets → Concrete
//! coverage → grant materialization → sibling session covered.

use autonoetic_gateway::runtime::approved_exec_cache::normalize_targets;
use autonoetic_gateway::runtime::remote_access::{
    classify_network_coverage, NetworkCoverage, RemoteAccessAnalyzer,
};
use autonoetic_gateway::scheduler::gateway_store::GatewayStore;
use autonoetic_types::background::{GrantScope, GrantTarget};

const GMAIL_READ_PY: &str = r#"#!/usr/bin/env python3
import imaplib
import email

HOST = "imap.gmail.com"

mail = imaplib.IMAP4_SSL(HOST)
mail.login("user@gmail.com", "app-password")
"#;

const GMAIL_LATEST_PY: &str = r#"import imaplib, email, os, json
from email.header import decode_header

secret = json.loads(os.environ["GMAIL_SECRET"])
IMAP_HOST = "imap.gmail.com"

conn = imaplib.IMAP4_SSL(IMAP_HOST)
conn.login(secret["username"], secret["app_password"])
"#;

fn make_gateway_dir(tmp: &tempfile::TempDir) -> std::path::PathBuf {
    let gw = tmp.path().join(".gateway");
    std::fs::create_dir_all(&gw).unwrap();
    gw
}

#[test]
#[serial_test::serial]
fn host_constant_flows_to_concrete_targets_and_sibling_grant_coverage() {
    // 1. Both scripts resolve the IMAP host as a concrete host_constant.
    for code in [GMAIL_READ_PY, GMAIL_LATEST_PY] {
        let analysis = RemoteAccessAnalyzer::analyze_code(code);
        assert!(
            analysis
                .detected_patterns
                .iter()
                .any(|p| p.category == "host_constant" && p.pattern == "imap.gmail.com"),
            "host_constant pattern missing for script: {code}"
        );

        // 2. detected_hosts on the approval = normalize_targets(patterns).
        let targets = normalize_targets(&analysis.detected_patterns);
        assert_eq!(targets, vec!["imap.gmail.com".to_string()]);

        // 3. Coverage is Concrete → exec-cache eligible (was Unresolved).
        assert_eq!(
            classify_network_coverage(&analysis.detected_patterns, targets),
            NetworkCoverage::Concrete {
                targets: vec!["imap.gmail.com".to_string()]
            }
        );
    }

    // 4. Approving the first sibling's exec materializes a root-scoped grant
    //    from the detected hosts (mirrors approval.rs::record_decision:
    //    GrantTarget::ExactHost per detected host, GrantScope::RootSession).
    let tmp = tempfile::tempdir().unwrap();
    let store = GatewayStore::open(&make_gateway_dir(&tmp)).unwrap();
    let first_targets = normalize_targets(
        &RemoteAccessAnalyzer::analyze_code(GMAIL_READ_PY).detected_patterns,
    );
    let grant_targets: Vec<GrantTarget> = first_targets
        .iter()
        .map(|h| GrantTarget::ExactHost(h.clone()))
        .collect();
    store
        .insert_session_grant(
            "root-1",
            "root-1/executor.default-469fdfa0",
            "executor.default",
            &GrantScope::RootSession,
            &grant_targets,
            "operator",
            &chrono::Utc::now().to_rfc3339(),
            Some("apr-b89fd9d8"),
            None,
        )
        .unwrap();

    // 5. The second sibling — different script, same host, SAME agent — is
    //    covered.
    let second_targets = normalize_targets(
        &RemoteAccessAnalyzer::analyze_code(GMAIL_LATEST_PY).detected_patterns,
    );
    assert!(
        store.grants_cover_targets(
            "root-1/executor.default-3e64a716",
            "root-1",
            "executor.default",
            &second_targets,
        ),
        "sibling session of the same agent running a different script against the same host must be grant-covered"
    );

    // 6. …but a DIFFERENT agent under the same root is NOT covered: approval
    //    is per-agent, so an unaudited candidate agent cannot inherit the
    //    operator's grant to executor.default (grant-strategy review, Gap 2).
    assert!(
        !store.grants_cover_targets(
            "root-1/gmail-a2435db5",
            "root-1",
            "gmail",
            &second_targets,
        ),
        "a different agent under the same root must not inherit executor.default's grant"
    );
}
