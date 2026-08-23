//! #1106 — shipped-manifest contract for `remote_access.targets: [any]`.
//!
//! `any` is only safe in two shapes, and CI must pin both:
//!
//! 1. `approval_mode: required` (executor.default's documented shape):
//!    declaration-wide, approval-per-host — the operator approval is the
//!    effective control, with the host named on the card and in the R++4
//!    phrase. A general-purpose execution role cannot enumerate its hosts;
//!    an enumeration would fail shut on the very first undeclared host
//!    (`undeclared_remote_target`), recreating the "full factory pipeline
//!    for one GET" failure the credential-egress RFC criticizes.
//! 2. `approval_mode: preapproved` with a wildcard NetworkAccess capability
//!    (`hosts: ["*"]`, genuine open-web roles): the capability is already
//!    the any-host authority, so the declaration adds nothing.
//!
//! Every other shape is the silent any-host auto-approval the
//! `remote_any_preapproval_requires_wildcard_capability` guard (#1106)
//! rejects at runtime — this test keeps the shipped roster clean of it.

use autonoetic_gateway::runtime::network_policy::load_manifest_remote_access_declaration;
use autonoetic_gateway::runtime::parser::SkillParser;
use autonoetic_types::agent::RemoteAccessApprovalMode;
use autonoetic_types::background::GrantTarget;
use autonoetic_types::capability::Capability;

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn skill_files() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![repo_root().join("agents")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().map(|n| n == "SKILL.md").unwrap_or(false) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[test]
fn shipped_agents_any_preapproval_requires_wildcard_capability() {
    let mut violations = Vec::new();
    let mut checked = 0;
    for path in skill_files() {
        let content = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let (manifest, _) = SkillParser::parse(&content)
            .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
        let Some(decl) = load_manifest_remote_access_declaration(
            path.parent().expect("skill dir"),
        ) else {
            continue;
        };
        let declares_any = decl.targets.iter().any(|t| matches!(t, GrantTarget::Any));
        if !declares_any {
            continue;
        }
        checked += 1;
        if matches!(decl.approval_mode, RemoteAccessApprovalMode::Required) {
            continue;
        }
        let wildcard = manifest.capabilities.iter().any(|c| {
            matches!(c, Capability::NetworkAccess { hosts } if hosts.iter().any(|h| h.trim() == "*"))
        });
        if !wildcard {
            violations.push(format!(
                "{}: targets:[any] + preapproved without wildcard NetworkAccess capability",
                path.display()
            ));
        }
    }
    assert!(checked > 0, "test must actually exercise the `any` roster");
    assert!(
        violations.is_empty(),
        "shipped manifests must never declare the silent any-host auto-approval shape:\n{}",
        violations.join("\n")
    );
}

#[test]
fn executor_default_keeps_approval_required_with_any_targets() {
    // executor.default's `targets: [any]` is load-bearing for its
    // general-purpose role (sandbox egress with curl/wget to whatever the
    // operator approves). The safety comes from approval_mode: required —
    // every networked exec goes through an operator approval with the host
    // named. Flipping to preapproved without a wildcard capability is the
    // silent any-host auto-approval; pin the required mode so the flip is
    // a deliberate, reviewed act.
    let path = repo_root().join("agents/specialists/executor.default/SKILL.md");
    let content = std::fs::read_to_string(&path).expect("executor SKILL.md");
    let _ = SkillParser::parse(&content).expect("parse executor");
    let decl = load_manifest_remote_access_declaration(path.parent().expect("executor dir"))
        .expect("executor declares remote_access");
    assert!(
        decl.targets.iter().any(|t| matches!(t, GrantTarget::Any)),
        "executor.default's role is general-purpose exec; if `any` is gone, \
         drop this test and re-derive the role's sandbox egress story"
    );
    assert!(
        matches!(decl.approval_mode, RemoteAccessApprovalMode::Required),
        "executor.default targets:[any] must pair with approval_mode: required \
         (the operator approval is the control) — see #1106"
    );
}
