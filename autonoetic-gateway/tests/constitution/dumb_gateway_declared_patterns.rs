//! Constitution Phase 4.3/4.4 pin: remote-access signals must be declaration-driven.

use autonoetic_gateway::runtime::remote_access::{
    undeclared_patterns_against_manifest, DetectedPattern,
};
use autonoetic_types::agent::RemoteAccessDeclaration;

#[test]
fn declared_patterns_cover_known_signals() {
    let patterns = vec![
        DetectedPattern {
            category: "import".to_string(),
            pattern: "import requests".to_string(),
            line_number: Some(1),
            reason: "import".to_string(),
        },
        DetectedPattern {
            category: "function_call".to_string(),
            pattern: "requests.get(".to_string(),
            line_number: Some(2),
            reason: "call".to_string(),
        },
        DetectedPattern {
            category: "network_command".to_string(),
            pattern: "curl".to_string(),
            line_number: Some(3),
            reason: "command".to_string(),
        },
        DetectedPattern {
            category: "network_command".to_string(),
            pattern: "pip install".to_string(),
            line_number: Some(4),
            reason: "package manager".to_string(),
        },
    ];
    let decl = RemoteAccessDeclaration {
        approval_mode: autonoetic_types::agent::RemoteAccessApprovalMode::Required,
        targets: vec![autonoetic_types::background::GrantTarget::Any],
        enabled_languages: vec![],
        python_imports: vec!["requests".to_string()],
        js_imports: vec![],
        rust_imports: vec![],
        go_imports: vec![],
        function_calls: vec!["requests.get".to_string()],
        shell_commands: vec!["curl".to_string()],
        package_manager_commands: vec!["pip install".to_string()],
    };

    let undeclared = undeclared_patterns_against_manifest(&patterns, Some(&decl));
    assert!(undeclared.is_empty());
}

#[test]
fn undeclared_patterns_fail_shut() {
    let patterns = vec![DetectedPattern {
        category: "network_command".to_string(),
        pattern: "wget".to_string(),
        line_number: Some(1),
        reason: "command".to_string(),
    }];
    let decl = RemoteAccessDeclaration {
        approval_mode: autonoetic_types::agent::RemoteAccessApprovalMode::Required,
        targets: vec![autonoetic_types::background::GrantTarget::Any],
        enabled_languages: vec![],
        python_imports: vec![],
        js_imports: vec![],
        rust_imports: vec![],
        go_imports: vec![],
        function_calls: vec![],
        shell_commands: vec!["curl".to_string()],
        package_manager_commands: vec![],
    };

    let undeclared = undeclared_patterns_against_manifest(&patterns, Some(&decl));
    assert_eq!(undeclared.len(), 1);
    assert_eq!(undeclared[0].pattern, "wget");
}

#[test]
fn undeclared_remote_target_fails_shut() {
    let patterns = vec![DetectedPattern {
        category: "url_literal".to_string(),
        pattern: "https://api.example.com/v1/data".to_string(),
        line_number: Some(1),
        reason: "url".to_string(),
    }];
    let decl = RemoteAccessDeclaration {
        approval_mode: autonoetic_types::agent::RemoteAccessApprovalMode::Required,
        targets: vec![autonoetic_types::background::GrantTarget::ExactHost(
            "api.other.com".to_string(),
        )],
        enabled_languages: vec![],
        python_imports: vec![],
        js_imports: vec![],
        rust_imports: vec![],
        go_imports: vec![],
        function_calls: vec![],
        shell_commands: vec!["curl".to_string()],
        package_manager_commands: vec![],
    };

    let undeclared = undeclared_patterns_against_manifest(&patterns, Some(&decl));
    assert_eq!(undeclared.len(), 1);
    assert_eq!(undeclared[0].category, "url_literal");
}
