//! Agent Runtime Submodule.
//!
//! Contains all logic for running an agent locally, including parsing its SKILL.md,
//! managing Tier 1 and Tier 2 memory, and enforcing the execution lifecycle.

pub mod active_execution_registry;
pub mod analysis;
pub mod approved_exec_cache;
pub mod artifact;
pub mod budget_tracker;
pub mod capability_inference;
pub mod checkpoint;
pub mod civic_evals;
pub mod plan_preflight;
pub mod code_excerpts;
pub mod compression;
pub mod compression_quality;
pub mod context_governor;
pub mod content_store;
pub mod context;

pub mod crypto;
pub mod curator_journal;
pub mod disclosure;
pub mod discretion_leak;
pub mod egress_labeler;
pub mod egress_path_matcher;
pub mod egress_proposal;
pub mod egress_stored;
pub mod error_fingerprint;
pub mod eval_stats;
pub mod failure_classification;
pub mod federation_carry_forward;
pub mod fuzzy_match;
pub mod guard;
pub mod guidance;
pub mod history_persist;
pub mod inference_profile;
pub mod human_gate;
pub mod install_contract;
pub mod lifecycle;
pub mod live_digest;
pub mod llm_preset_resolver;
pub mod mcp;
pub mod memory;
pub mod middleware;
pub mod model_router;
pub mod network_grant;
pub mod network_sinks;
pub mod network_host_contract;
pub mod network_policy;
pub mod local_model_context;
pub mod openrouter_catalog;
pub mod operator_activity;
pub mod parser;
pub mod post_session_digest;
pub mod promotion_governor;
pub mod promotion_evidence;
pub mod promotion_store;
pub mod session_timeline;
pub mod prompt_budget;
pub mod quality_signal;
pub mod reevaluation_state;
pub mod remote_access;
pub mod semantic_diff;
pub mod sealed_network;
pub mod sealed_network_proxy;
pub mod response_validation;
pub mod root_session_budget;
pub mod script_execute;
pub mod session_budget;
pub mod session_envelope;
pub mod operator_pending;
pub mod session_export;
pub mod session_resume;
pub mod session_context;
pub mod session_handoff;
pub mod session_outcome_writer;
pub mod session_overview;
pub mod host_probe_budget;
pub mod session_read_cache;
pub mod session_report;
pub mod session_tracer;
pub mod smoke_test_gate;
pub mod state_attestation;
pub mod store;
pub mod tool_call_processor;
pub mod tool_dispatch;
pub mod tool_tier_registry;
pub mod tools;
pub mod trajectory_health;
pub mod trajectory_monitor;
pub mod v4a;
pub mod workbench_return;

/// Returns true if the given filename matches common test-file patterns.
/// Used by `artifact_build` (metadata), `semantic_diff` (file role), and
/// formerly by `promotion_record` (removed — see #668).
///
/// All checks are case-insensitive.
pub fn is_test_file(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("/tests/")
        || lower.starts_with("tests/")
        || lower.contains("/__tests__/")
        || lower.starts_with("__tests__/")
        || lower.starts_with("test_")
        || lower.ends_with("_test.py")
        || lower.ends_with("_test.rs")
        || lower.ends_with("_test.go")
        || lower.ends_with(".test.ts")
        || lower.ends_with(".test.js")
        || lower.ends_with(".spec.ts")
        || lower.ends_with(".spec.js")
}

/// Extract the YAML frontmatter segment from a Markdown document (`---` … `---`).
///
/// The opening delimiter must be the first line of the document; the closing
/// delimiter must be a line consisting solely of `---` (or `...`). Splitting on
/// the raw substring `"---"` is wrong: frontmatter routinely contains longer
/// dash runs (`- "-----BEGIN"` in `prohibited_text_patterns`), and a substring
/// split truncates the segment mid-line — the parser then sees an unterminated
/// quoted scalar and the declaration silently degrades to "absent".
pub fn extract_yaml_frontmatter(doc: &str) -> Option<&str> {
    let rest = doc.strip_prefix("---")?;
    let rest = rest.trim_start_matches([' ', '\t']);
    let rest = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))?;
    let mut end = None;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end();
        if trimmed.eq("---") || trimmed.eq("...") {
            end = Some(offset);
            break;
        }
        offset += line.len();
    }
    Some(&rest[..end?])
}

#[cfg(test)]
mod tests {
    use super::{extract_yaml_frontmatter, is_test_file};

    #[test]
    fn is_test_file_detects_tests_directory_prefix() {
        assert!(is_test_file("tests/test_fib_agent.py"));
        assert!(is_test_file("tests/test_main.py"));
        assert!(is_test_file("tests/conftest.py")); // part of test infrastructure
        assert!(!is_test_file("src/main.py"));
    }

    #[test]
    fn is_test_file_detects_nested_tests_directory() {
        assert!(is_test_file("src/tests/test_foo.py"));
        assert!(is_test_file("packages/bar/tests/test_bar.py"));
    }

    #[test]
    fn is_test_file_detects_test_prefixes() {
        assert!(is_test_file("test_agent.py"));
        assert!(is_test_file("test_fib.py"));
    }

    #[test]
    fn is_test_file_detects_test_suffixes() {
        assert!(is_test_file("agent_test.py"));
        assert!(is_test_file("foo_test.rs"));
        assert!(is_test_file("component.test.ts"));
        assert!(is_test_file("widget.spec.js"));
    }

    #[test]
    fn is_test_file_case_insensitive() {
        assert!(is_test_file("Tests/test_fib.py"));
        assert!(is_test_file("TESTS/test_main.py"));
        assert!(is_test_file("Test_Agent.PY"));
    }

    #[test]
    fn is_test_file_rejects_non_test_files() {
        assert!(!is_test_file("fib_agent.py"));
        assert!(!is_test_file("main.py"));
        assert!(!is_test_file("requirements.txt"));
        assert!(!is_test_file("README.md"));
    }

    #[test]
    fn frontmatter_extracts_basic_segment() {
        let doc = "---\nname: x\n---\nbody";
        assert_eq!(extract_yaml_frontmatter(doc), Some("name: x\n"));
    }

    #[test]
    fn frontmatter_extracts_with_dots_terminator_and_crlf() {
        let doc = "---\r\nname: x\r\n...\r\nbody";
        assert_eq!(extract_yaml_frontmatter(doc), Some("name: x\r\n"));
    }

    #[test]
    fn frontmatter_requires_opening_delimiter_on_first_line() {
        assert_eq!(extract_yaml_frontmatter("body\n---\nx\n---\n"), None);
        assert_eq!(extract_yaml_frontmatter(""), None);
    }

    #[test]
    fn frontmatter_without_closing_delimiter_is_none() {
        assert_eq!(extract_yaml_frontmatter("---\nname: x\nbody"), None);
    }

    /// Regression: a `-----BEGIN` dash-run inside frontmatter must not
    /// terminate the segment. A substring split on `"---"` truncated the
    /// segment at the dash run, leaving an unterminated quoted scalar — the
    /// exact "unexpected end of stream while scanning a quoted scalar" that
    /// silenced the credential_onboarding.default remote_access declaration.
    #[test]
    fn frontmatter_survives_dash_run_in_quoted_scalar() {
        let doc = "---\nprohibited_text_patterns:\n  - \"BEGIN RSA PRIVATE KEY\"\n  - \"-----BEGIN\"\nloop_guard:\n  max_tool_failures: 8\n---\nbody";
        let fm = extract_yaml_frontmatter(doc).expect("frontmatter must extract");
        assert!(fm.contains("-----BEGIN"));
        assert!(fm.contains("max_tool_failures"));
        let value: serde_yaml::Value =
            serde_yaml::from_str(fm).expect("extracted frontmatter must parse as YAML");
        assert_eq!(
            value["prohibited_text_patterns"][1].as_str(),
            Some("-----BEGIN")
        );
    }

    /// Same shape without quotes: an unquoted `-----BEGIN` scalar is still
    /// frontmatter content, not a delimiter.
    #[test]
    fn frontmatter_survives_unquoted_dash_run() {
        let doc = "---\nblock: |-\n  -----BEGIN CERTIFICATE-----\nname: x\n---\n";
        let fm = extract_yaml_frontmatter(doc).expect("frontmatter must extract");
        assert!(fm.contains("BEGIN CERTIFICATE"));
    }
}
