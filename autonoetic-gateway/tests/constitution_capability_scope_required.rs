//! Constitution R+1 — Structured capability scopes mandatory for all capabilities.
//!
//! Bare-string capability declarations (e.g. `"ReadAccess"`) are rejected for
//! every capability type. Agents must supply tagged objects with explicit scope
//! fields (e.g. `{"type":"ReadAccess","scopes":["self.*"]}`).

mod support;

use autonoetic_types::capability::Capability;

fn bare_string_caps_json(caps: &[&str]) -> String {
    let items: Vec<String> = caps.iter().map(|c| format!("\"{}\"", c)).collect();
    format!(r#"{{"capabilities":[{}]}}"#, items.join(","))
}

fn tagged_caps_json(caps: &[serde_json::Value]) -> String {
    format!(
        r#"{{"capabilities":[{}]}}"#,
        caps.iter()
            .map(|c| serde_json::to_string(c).unwrap())
            .collect::<Vec<_>>()
            .join(",")
    )
}

#[derive(Debug, serde::Deserialize)]
struct CapsOnly {
    capabilities: Vec<Capability>,
}

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
fn all_bare_string_capabilities_rejected() {
    for name in ALL_CAPABILITY_NAMES {
        let j = bare_string_caps_json(&[name]);
        let result = serde_json::from_str::<CapsOnly>(&j);
        assert!(
            result.is_err(),
            "R+1: bare-string '{}' should be rejected, but was accepted",
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
fn tagged_objects_with_explicit_scopes_accepted() {
    let cases: Vec<serde_json::Value> = vec![
        serde_json::json!({"type":"SandboxFunctions","allowed":["content."]}),
        serde_json::json!({"type":"ReadAccess","scopes":["self.*"]}),
        serde_json::json!({"type":"WriteAccess","scopes":["self.*"]}),
        serde_json::json!({"type":"NetworkAccess","hosts":["api.example.com"]}),
        serde_json::json!({"type":"CodeExecution","patterns":["python3 "]}),
        serde_json::json!({"type":"AgentMessage","patterns":["*"]}),
        serde_json::json!({"type":"AgentRevision","patterns":["*"]}),
        serde_json::json!({"type":"Evaluation","patterns":["*"]}),
        serde_json::json!({"type":"ApprovalQueue","patterns":["*"]}),
        serde_json::json!({"type":"SchedulerSignal","patterns":["*"]}),
        serde_json::json!({"type":"SchedulerAccess","patterns":["scheduler.cron.*"]}),
        serde_json::json!({"type":"CredentialAccess","services":["github"]}),
        serde_json::json!({"type":"UserProfileAccess","scopes":["basic"]}),
        serde_json::json!({"type":"SkillInstall","allowed_sources":["agentskills.io"]}),
        serde_json::json!({"type":"ConstitutionalProposal","patterns":["*"]}),
        serde_json::json!({"type":"EmergencyStop"}),
        serde_json::json!({"type":"AgentSpawn","max_children":5}),
        serde_json::json!({"type":"BackgroundReevaluation","min_interval_secs":60,"allow_reasoning":true}),
    ];

    let j = tagged_caps_json(&cases);
    let result: CapsOnly = serde_json::from_str(&j).unwrap();
    assert_eq!(
        result.capabilities.len(),
        cases.len(),
        "all {} tagged capabilities should parse",
        cases.len()
    );
}

#[test]
fn mixed_bare_and_tagged_rejected_at_first_bare() {
    let j = format!(
        r#"{{"capabilities":[{}, "ReadAccess"]}}"#,
        serde_json::to_string(&serde_json::json!({"type":"NetworkAccess","hosts":["*"]})).unwrap()
    );
    let result = serde_json::from_str::<CapsOnly>(&j);
    assert!(result.is_err(), "mixed bare+tagged should be rejected");
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("ReadAccess"),
        "error should name the offending capability, got: {}",
        msg
    );
}
