//! Constitution R+1 — Structured capability scopes mandatory for all capabilities.
//!
//! Bare-string capability declarations (e.g. `"ReadAccess"`) are rejected for
//! every capability type when routed through the lenient LLM normalization path
//! (`normalize_capability_from_llm`), which is the same code path used by
//! `agent_revision.create_from_intent` and frontmatter capability parsing.


use autonoetic_gateway::runtime::tools::agent_revision::normalize_capability_from_llm;

const ALL_CAPABILITY_NAMES: &[&str] = &[
    "SandboxFunctions",
    "ReadAccess",
    "WriteAccess",
    "NetworkAccess",
    "CodeExecution",
    "AgentMessage",
    "AgentRevision",
    "Evaluation",
    "ApprovalQueue",
    "SchedulerSignal",
    "SchedulerAccess",
    "CredentialAccess",
    "UserProfileAccess",
    "SkillInstall",
    "ConstitutionalProposal",
    "EmergencyStop",
    "AgentSpawn",
    "BackgroundReevaluation",
];

#[test]
fn lenient_path_rejects_all_bare_strings() {
    for name in ALL_CAPABILITY_NAMES {
        let result = normalize_capability_from_llm(serde_json::Value::String(name.to_string()));
        assert!(
            result.is_err(),
            "R+1: bare-string '{}' should be rejected by lenient path, but was accepted",
            name
        );
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains(name),
            "R+1: error for '{}' should name the capability, got: {}",
            name,
            msg
        );
    }
}

#[test]
fn lenient_path_accepts_all_tagged_objects() {
    let cases: Vec<(serde_json::Value, &str)> = vec![
        (
            serde_json::json!({"type":"SandboxFunctions","allowed":["content."]}),
            "SandboxFunctions",
        ),
        (
            serde_json::json!({"type":"ReadAccess","scopes":["self.*"]}),
            "ReadAccess",
        ),
        (
            serde_json::json!({"type":"WriteAccess","scopes":["self.*"]}),
            "WriteAccess",
        ),
        (
            serde_json::json!({"type":"NetworkAccess","hosts":["api.example.com"]}),
            "NetworkAccess",
        ),
        (
            serde_json::json!({"type":"CodeExecution","patterns":["python3 "]}),
            "CodeExecution",
        ),
        (
            serde_json::json!({"type":"AgentMessage","patterns":["*"]}),
            "AgentMessage",
        ),
        (
            serde_json::json!({"type":"AgentRevision","patterns":["*"]}),
            "AgentRevision",
        ),
        (
            serde_json::json!({"type":"Evaluation","patterns":["*"]}),
            "Evaluation",
        ),
        (
            serde_json::json!({"type":"ApprovalQueue","patterns":["*"]}),
            "ApprovalQueue",
        ),
        (
            serde_json::json!({"type":"SchedulerSignal","patterns":["*"]}),
            "SchedulerSignal",
        ),
        (
            serde_json::json!({"type":"SchedulerAccess","patterns":["scheduler.cron.*"]}),
            "SchedulerAccess",
        ),
        (
            serde_json::json!({"type":"CredentialAccess","services":["github"]}),
            "CredentialAccess",
        ),
        (
            serde_json::json!({"type":"UserProfileAccess","scopes":["basic"]}),
            "UserProfileAccess",
        ),
        (
            serde_json::json!({"type":"SkillInstall","allowed_sources":["agentskills.io"]}),
            "SkillInstall",
        ),
        (
            serde_json::json!({"type":"ConstitutionalProposal","patterns":["*"]}),
            "ConstitutionalProposal",
        ),
        (serde_json::json!({"type":"EmergencyStop"}), "EmergencyStop"),
        (
            serde_json::json!({"type":"AgentSpawn","max_children":5}),
            "AgentSpawn",
        ),
        (
            serde_json::json!({"type":"BackgroundReevaluation","min_interval_secs":60,"allow_reasoning":true}),
            "BackgroundReevaluation",
        ),
    ];

    for (value, name) in &cases {
        let result = normalize_capability_from_llm(value.clone());
        assert!(
            result.is_ok(),
            "R+1: tagged object for '{}' should be accepted, got error: {}",
            name,
            result.unwrap_err()
        );
    }
    assert_eq!(cases.len(), ALL_CAPABILITY_NAMES.len());
}

#[test]
fn lenient_path_rejects_mixed_bare_after_tagged() {
    let _tagged =
        normalize_capability_from_llm(serde_json::json!({"type":"NetworkAccess","hosts":["*"]}))
            .expect("tagged NetworkAccess should parse");
    let bare = normalize_capability_from_llm(serde_json::Value::String("ReadAccess".to_string()));
    assert!(
        bare.is_err(),
        "bare ReadAccess after tagged NetworkAccess should still be rejected"
    );
    let msg = bare.unwrap_err().to_string();
    assert!(
        msg.contains("ReadAccess") && msg.contains("scopes"),
        "error should name capability and required field, got: {}",
        msg
    );
}
