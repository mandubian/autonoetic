//! One-line glossary of constitutional clauses for human-readable surfaces
//! (the TUI policy pane, approval cards, the CLI "Governed by:" line).
//!
//! **Derived, not hand-maintained.** The map is generated directly from the
//! constitution markdown — each entry is the first sentence of a clause's
//! statement — so it cannot drift from the law (the failure mode that let an
//! orphan `P-2.22` gloss collide with a real rule). It is compiled into the
//! binary via the committed `constitution_glossary_generated.rs`, so it works
//! without filesystem access; a drift-guard test regenerates it from the
//! constitution and fails CI if the committed copy is stale.
//!
//! Regenerate after a constitution change:
//! `BLESS_GLOSSARY=1 cargo test -p autonoetic-gateway bless_constitution_glossary`

include!("constitution_glossary_generated.rs");

/// Longest gloss rendered inline on a "Governed by:" line before truncation.
const MAX_GLOSS_CHARS: usize = 100;

/// One-line explanation for a clause id (`P-x.y` / `Ri-x.y`), or `None` if the
/// id is not a constitutional clause.
pub fn rule_explanation(rule_id: &str) -> Option<&'static str> {
    GLOSSARY
        .iter()
        .find(|(id, _)| *id == rule_id)
        .map(|(_, gloss)| *gloss)
}

/// Format a list of enforced clause IDs as a human-readable "Governed by:"
/// line. Glosses longer than [`MAX_GLOSS_CHARS`] are truncated with an ellipsis
/// for compact display.
pub fn format_enforced_rules(rules: &[impl AsRef<str>]) -> String {
    if rules.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = rules
        .iter()
        .map(|r| {
            let id = r.as_ref();
            match rule_explanation(id) {
                Some(gloss) => format!("{} ({})", id, truncate_gloss(gloss)),
                None => id.to_string(),
            }
        })
        .collect();
    format!("Governed by: {}", parts.join("; "))
}

fn truncate_gloss(gloss: &str) -> String {
    if gloss.chars().count() <= MAX_GLOSS_CHARS {
        return gloss.to_string();
    }
    let cut: String = gloss.chars().take(MAX_GLOSS_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render the generated source file from a clause-glossary map.
    fn render_glossary_source(glossary: &std::collections::BTreeMap<String, String>) -> String {
        let mut out = String::new();
        out.push_str("// @generated — do not edit by hand.\n");
        out.push_str("// Regenerate with: BLESS_GLOSSARY=1 cargo test -p autonoetic-gateway bless_constitution_glossary\n");
        out.push_str("// Source of truth: the configured constitution markdown (see constitution_glossary.rs).\n");
        out.push_str("pub(crate) static GLOSSARY: &[(&str, &str)] = &[\n");
        for (id, gloss) in glossary {
            out.push_str(&format!("    ({:?}, {:?}),\n", id, gloss));
        }
        out.push_str("];\n");
        out
    }

    /// Read the default constitution markdown from the workspace.
    fn default_constitution_text() -> String {
        let rel = autonoetic_types::config::GatewayConfig::default()
            .constitution
            .source_path;
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join(&rel);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read constitution {}: {e}", path.display()))
    }

    fn generated_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/constitution_glossary_generated.rs")
    }

    #[test]
    fn bless_constitution_glossary() {
        if std::env::var("BLESS_GLOSSARY").is_err() {
            return;
        }
        let glossary = crate::constitution_digest::extract_rule_glossary(&default_constitution_text());
        std::fs::write(generated_path(), render_glossary_source(&glossary)).unwrap();
    }

    /// The committed generated map must equal a fresh extraction from the
    /// constitution — otherwise the gloss surface has drifted from the law.
    #[test]
    fn generated_glossary_matches_constitution() {
        let glossary = crate::constitution_digest::extract_rule_glossary(&default_constitution_text());
        let expected = render_glossary_source(&glossary);
        let committed = std::fs::read_to_string(generated_path()).unwrap();
        assert_eq!(
            committed, expected,
            "constitution_glossary_generated.rs is stale — run \
             `BLESS_GLOSSARY=1 cargo test -p autonoetic-gateway bless_constitution_glossary`"
        );
    }

    #[test]
    fn glossary_covers_every_clause_and_resolves() {
        // Non-empty, and a few known clauses resolve to their first sentence.
        assert!(GLOSSARY.len() > 100, "expected the full clause set");
        assert!(rule_explanation("P-2.1").is_some());
        assert!(rule_explanation("Ri-0.14").is_some());
        assert!(rule_explanation("P-999.999").is_none());
    }

    #[test]
    fn format_enforced_rules_empty_and_known() {
        let empty: Vec<&str> = vec![];
        assert_eq!(format_enforced_rules(&empty), "");
        let result = format_enforced_rules(&["P-2.1", "P-2.18"]);
        assert!(result.starts_with("Governed by: P-2.1 ("));
        assert!(result.contains("P-2.18"));
    }
}
