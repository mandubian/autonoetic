//! Integration tests for the agent-initiated `capsule.export` /
//! `capsule.import` tools. Verifies the `CapsuleExport` capability gate.

use autonoetic_gateway::runtime::tools::capsule::{CapsuleExportTool, CapsuleImportTool};
use autonoetic_gateway::runtime::tools::NativeTool;
use autonoetic_types::agent::{AgentIdentity, AgentManifest};
use autonoetic_types::capability::Capability;
use crate::support::manifest_builder::TestManifest;

fn manifest_with(caps: Vec<Capability>) -> AgentManifest {
    AgentManifest {
        agent: AgentIdentity {
            id: "test".to_string(),
            name: "test".to_string(),
            description: "test".to_string(),
            singleton: false,
            resident_idle_ttl_secs: None,
        },
        capabilities: caps,
        ..TestManifest::new().build()
    }
}

#[test]
fn tool_unavailable_without_capability() {
    let no_cap = manifest_with(vec![Capability::CodeExecution {
        patterns: vec!["*".to_string()],
        commands: vec![],
    }]);
    assert!(!CapsuleExportTool.is_available(&no_cap));
    assert!(!CapsuleImportTool.is_available(&no_cap));
}

#[test]
fn tool_available_with_capsule_export_capability() {
    let with_cap = manifest_with(vec![Capability::CapsuleExport]);
    assert!(CapsuleExportTool.is_available(&with_cap));
    assert!(CapsuleImportTool.is_available(&with_cap));
}

#[test]
fn tool_definitions_advertise_required_fields() {
    let exp = CapsuleExportTool.definition();
    assert_eq!(exp.name, "capsule_export");
    let required = exp.input_schema["required"].as_array().unwrap();
    assert!(
        required.iter().any(|v| v == "agent_id"),
        "agent_id must be required"
    );

    let imp = CapsuleImportTool.definition();
    assert_eq!(imp.name, "capsule_import");
    let required = imp.input_schema["required"].as_array().unwrap();
    assert!(
        required.iter().any(|v| v == "archive"),
        "archive must be required"
    );
}
