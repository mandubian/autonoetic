//! Intent → proposed rule set (RFC §4.3 authoring aid, #978).
//!
//! Operators don't write `egress.rules` YAML by hand in the common case: at
//! session start or in the session room, *"emails stay local"* → the gateway
//! **proposes** the concrete rule set (from known tool catalogs, the MCP
//! server list, and path conventions) → the operator confirms with one
//! keystroke → the confirmed rules are the declared input.
//!
//! This module is the deterministic, prompt-free mapper. Two properties make
//! it safe as an authoring aid (RFC §4.3):
//!
//! 1. **It never fabricates.** A rule is only proposed when the subject
//!    actually matches a source the gateway knows (a registered tool family,
//!    a registered MCP server, or a path the operator named). An unmatched
//!    subject yields `rules: []` plus `known_sources` near-misses so the
//!    operator can pick — the natural language is only an authoring
//!    convenience; what gets enforced is the explicit, operator-confirmed
//!    rule.
//! 2. **It is deterministic.** Same intent + same catalog ⇒ same proposal.
//!    Lawful-Executor (§14) is preserved: enforcement remains a deterministic
//!    function of declared inputs. A proposal has *no* effect until the
//!    operator confirms it through `session.egress_policy.set`.
//!
//! The intent grammar is deliberately narrow — a handful of "stays local"
//! phrasings — so there is no free-text interpretation to drift.

use autonoetic_types::egress::{
    normalize_source_key, EgressRule, NamedEgressLabel,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// The subject of an intent phrase, after deterministic parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentSubject {
    /// A tool family or exact tool name ("emails", "sandbox", "web").
    Tool(String),
    /// An MCP server name ("gmail", "outlook").
    McpServer(String),
    /// A filesystem path ("~/mail", "/srv/notes").
    Path(String),
    /// Parsed but matched nothing recognizable.
    Unknown(String),
}

/// One proposed rule, carrying the *reason* it was proposed so the operator
/// can audit the mapping before confirming (RFC §4.3 "the proposed rule set
/// above" — the rationale is what makes a proposal reviewable).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedRule {
    #[serde(flatten)]
    pub rule: EgressRule,
    /// Why this rule was proposed — names the catalog entries that matched.
    pub rationale: String,
}

/// A concrete rule proposal for one intent phrase (#978).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressProposal {
    /// The normalized intent phrase the proposal was built from.
    pub intent: String,
    /// The parsed subject ("email", "~/mail", "gmail").
    pub subject: String,
    /// `tool` | `mcp_server` | `path` | `unknown`.
    pub kind: String,
    /// Rules to declare. **Empty when nothing matched** — the proposal then
    /// carries `known_sources` near-misses instead of inventing rules.
    #[serde(default)]
    pub rules: Vec<ProposedRule>,
    /// Registered MCP server names consulted for this proposal.
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    /// Near-miss source families/servers the subject might have meant —
    /// populated when `rules` is empty so the operator can pick deliberately.
    #[serde(default)]
    pub known_sources: Vec<String>,
    /// Human note about what was matched and what wasn't.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The catalog the mapper matches against: registered tool names plus
/// registered MCP server names. Pure data — the gateway builds it without
/// connecting to anything (RFC §4.3 "known tool catalogs, MCP server list").
#[derive(Debug, Clone, Default)]
pub struct SourceCatalog {
    pub tool_names: HashSet<String>,
    pub mcp_server_names: Vec<String>,
}

/// Path-taking source families used for path subjects (RFC §4.2 example uses
/// `fs.read:~/mail/**` and `sandbox.exec:~/mail/**`). The four families cover
/// every path-shaped tool class the gateway has.
const PATH_SOURCE_FAMILIES: &[&str] = &["fs.read", "content.read", "sandbox.exec", "artifact.exec"];

/// Normalize an intent phrase for parsing: lowercase, collapse whitespace,
/// trim trailing punctuation.
fn normalize_intent(intent: &str) -> String {
    intent
        .trim()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['.', '!', '?', ';', ':'])
        .trim()
        .to_ascii_lowercase()
}

/// The "stays local" phrasings the mapper understands. Each form's subject is
/// the `*` placeholder; everything else in the phrase is fixed (deterministic
/// grammar, no free-text interpretation).
const LOCAL_PHRASES: &[&str] = &[
    "* must not leave this machine",
    "* must stay on this machine",
    "* must stay local",
    "* stays on this machine",
    "* stays off the network",
    "* stays local",
    "* stay on this machine",
    "* stay off the network",
    "* stay local",
    "keep * off the network",
    "keep * on this machine",
    "keep * local",
    "make * stay local",
    "make * stays local",
    "make * local",
    "local: *",
];

/// Parse an intent phrase into its subject (RFC §4.3). Returns `None` for
/// phrases that are not a recognizable "stays local" declaration — the mapper
/// refuses rather than guesses.
pub fn parse_intent(intent: &str) -> Option<IntentSubject> {
    let normalized = normalize_intent(intent);
    if normalized.is_empty() {
        return None;
    }
    let mut subject = None;
    for phrase in LOCAL_PHRASES {
        if let Some((prefix, suffix)) = phrase.split_once('*') {
            let prefix = prefix.trim_end();
            let suffix = suffix.trim_start();
            if normalized.starts_with(prefix)
                && normalized.ends_with(suffix)
                && normalized.len() >= prefix.len() + suffix.len()
            {
                let s = &normalized[prefix.len()..normalized.len() - suffix.len()];
                let s = s.trim().trim_matches(':').trim();
                // A bare-verb phrase ("* stays local") must carry a
                // single-token subject; anything more is a different
                // construction ("make emails stay local" is its own phrase
                // and is matched separately). Keeps the grammar predictable.
                if !s.is_empty() && ((prefix.is_empty() && !s.contains(' ')) || !prefix.is_empty())
                {
                    subject = Some(s);
                    break;
                }
            }
        }
    }
    // Bare form: a phrase that names a source without a verb ("email",
    // "~/mail") is still an intent — the proposal step decides whether it
    // matches a known source. Only non-empty, sane subjects pass.
    let subject = subject.or_else(|| {
        if !normalized.is_empty()
            && !normalized.contains(' ')
            && normalized.chars().all(|c| {
                c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/'
                    || c == '~'
            })
        {
            Some(normalized.as_str())
        } else {
            None
        }
    })?;

    let subject = subject.trim().to_string();
    if subject.is_empty() {
        return None;
    }

    // Path subjects: anything carrying a path separator or home token.
    if subject.contains('/') || subject.starts_with('~') {
        return Some(IntentSubject::Path(subject));
    }
    // MCP server subjects: no underscore but a dotted multi-part name
    // ("gmail.api" is still a server name). Default: treat as a tool subject;
    // the catalog match decides between tool family and MCP server.
    Some(IntentSubject::Tool(subject))
}

/// Search token for catalog matching: lowercased, with `-`/`.` folded to `_`
/// (same folding as [`normalize_source_key`] so tool names agree).
fn search_token(subject: &str) -> String {
    normalize_source_key(subject)
}

/// Strip a trailing plural "s" for matching when the bare token doesn't match
/// anything ("emails" → "email") — authoring convenience only; the *displayed*
/// subject stays as typed.
fn singular_candidate(token: &str) -> String {
    token.strip_suffix('s').map(str::to_string).unwrap_or_else(|| token.to_string())
}

/// Tool families: registered tool names grouped by their first segment
/// (`email_read`, `email_list` → family `email`). Sorted for determinism.
fn tool_families(tool_names: &HashSet<String>) -> Vec<(String, Vec<String>)> {
    let mut families: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for name in tool_names {
        let family = name.split('_').next().unwrap_or(name.as_str());
        families
            .entry(family.to_string())
            .or_default()
            .push(name.clone());
    }
    families
        .into_iter()
        .map(|(family, mut members)| {
            members.sort();
            (family, members)
        })
        .collect()
}

/// Exact-tool and family matches for a tool token, plus any MCP server
/// matches. All deterministic and sorted.
fn match_tool_token(token: &str, catalog: &SourceCatalog) -> (Vec<ProposedRule>, Vec<String>) {
    let mut rules = Vec::new();
    let mut matched_any = false;
    let token_norm = search_token(token);

    // Exact registered tool name → bare rule, no glob ("web_search" alone).
    if catalog.tool_names.contains(&token_norm) {
        rules.push(ProposedRule {
            rule: EgressRule {
                source: token_norm.clone(),
                path: None,
                label: NamedEgressLabel::LocalOnly.to_label(),
            },
            rationale: "exact registered tool name".to_string(),
        });
        matched_any = true;
    }

    // Family glob: any registered tool whose first segment equals the token
    // ("email" + email_read/email_list → `email.*`).
    let families = tool_families(&catalog.tool_names);
    for (family, members) in &families {
        if family == &token_norm || singular_candidate(&token_norm) == *family {
            let source = format!("{family}.*");
            let rationale = format!(
                "matched {} registered tool(s): {}",
                members.len(),
                members.join(", ")
            );
            rules.push(ProposedRule {
                rule: EgressRule {
                    source,
                    path: None,
                    label: NamedEgressLabel::LocalOnly.to_label(),
                },
                rationale,
            });
            matched_any = true;
        }
    }

    // MCP server: a registered server name matching the token
    // ("gmail" + registered gmail server → `mcp.gmail.*`).
    let mut mcp_rules = Vec::new();
    for server in &catalog.mcp_server_names {
        let server_norm = search_token(server);
        if server_norm == token_norm || singular_candidate(&token_norm) == server_norm {
            let source = format!("mcp.{server}.*");
            mcp_rules.push(ProposedRule {
                rule: EgressRule {
                    source,
                    path: None,
                    label: NamedEgressLabel::LocalOnly.to_label(),
                },
                rationale: format!("matched registered MCP server '{server}'"),
            });
            matched_any = true;
        }
    }
    rules.extend(mcp_rules);
    rules.sort_by(|a, b| a.rule.source.cmp(&b.rule.source));

    // Known-source near-misses for the operator to pick from: families and
    // servers sharing a 3-gram with the token ("mailx" → "email", "gmail").
    // Suggestions only — never rules.
    let mut known = Vec::new();
    if !matched_any {
        for (family, _members) in &families {
            if shares_ngram(&token_norm, family, 3) {
                known.push(format!("{family}.*"));
            }
        }
        for server in &catalog.mcp_server_names {
            let server_norm = search_token(server);
            if shares_ngram(&token_norm, &server_norm, 3) {
                known.push(format!("mcp.{server}.*"));
            }
        }
        known.sort();
        known.dedup();
        known.truncate(8);
    }

    (rules, known)
}

/// Whether `a` and `b` share a common `n`-character substring (case already
/// folded). Short tokens fall back to exact equality. Char-boundary safe:
/// iterates `char`s rather than slicing `&str` at byte indices, so non-ASCII
/// source names (e.g. an MCP server name from the registry file) cannot panic.
fn shares_ngram(a: &str, b: &str, n: usize) -> bool {
    if a == b {
        return true;
    }
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    if a_chars.len() < n || b_chars.len() < n {
        return a.contains(b) || b.contains(a);
    }
    let grams: HashSet<String> = a_chars
        .windows(n)
        .map(|w| w.iter().collect())
        .collect();
    b_chars
        .windows(n)
        .any(|w| grams.contains(&w.iter().collect::<String>()))
}

/// Resolve the subject `kind` from the rules actually matched: when only MCP
/// server rules were produced, the subject names an MCP server, not a tool
/// ("gmail" + a registered gmail server → `kind: "mcp_server"`).
fn matched_kind(rules: &[ProposedRule]) -> String {
    if rules.iter().all(|r| r.rule.source.starts_with("mcp.")) {
        "mcp_server".to_string()
    } else {
        "tool".to_string()
    }
}

/// Path rules for a path subject: every path-taking source family gets a
/// `<family>:<path>/**` rule (RFC §4.2 path convention).
fn match_path(path: &str) -> Vec<ProposedRule> {
    let mut rules = Vec::new();
    let path = path.trim().trim_end_matches('/');
    let pattern = if path.is_empty() { "*".to_string() } else { format!("{path}/**") };
    for family in PATH_SOURCE_FAMILIES {
        rules.push(ProposedRule {
            rule: EgressRule {
                source: family.to_string(),
                path: Some(pattern.clone()),
                label: NamedEgressLabel::LocalOnly.to_label(),
            },
            rationale: format!(
                "path-taking source family '{family}' — path convention for '{path}'"
            ),
        });
    }
    rules
}

/// Build a concrete rule proposal from an intent phrase against a known
/// source catalog (RFC §4.3). Deterministic and non-fabricating: an
/// unmatched subject yields `rules: []` with `known_sources` near-misses.
pub fn build_egress_proposal(intent: &str, catalog: &SourceCatalog) -> EgressProposal {
    let normalized = normalize_intent(intent);
    let Some(subject) = parse_intent(intent) else {
        return EgressProposal {
            intent: normalized.clone(),
            subject: String::new(),
            kind: "unknown".to_string(),
            rules: Vec::new(),
            mcp_servers: catalog.mcp_server_names.clone(),
            known_sources: Vec::new(),
            note: Some(
                "not a recognizable 'stays local' declaration — say e.g. 'emails stay local' or '~/mail must not leave this machine'"
                    .to_string(),
            ),
        };
    };

    let (subject_text, kind, rules, known_sources, note) = match &subject {
        IntentSubject::Tool(token) => {
            let (rules, known) = match_tool_token(token, catalog);
            let kind = matched_kind(&rules);
            let note = if rules.is_empty() {
                Some(format!(
                    "'{token}' matches no registered tool family or MCP server — nothing proposed"
                ))
            } else {
                None
            };
            (token.clone(), kind, rules, known, note)
        }
        IntentSubject::McpServer(server) => {
            let (rules, known) = match_tool_token(server, catalog);
            let note = if rules.is_empty() {
                Some(format!(
                    "'{server}' matches no registered MCP server — nothing proposed"
                ))
            } else {
                None
            };
            (server.clone(), "mcp_server".to_string(), rules, known, note)
        }
        IntentSubject::Path(path) => {
            let rules = match_path(path);
            let note = if rules.is_empty() {
                Some(format!("'{}' produced no path rules", path))
            } else {
                None
            };
            (
                path.clone(),
                "path".to_string(),
                rules,
                Vec::new(),
                note,
            )
        }
        IntentSubject::Unknown(token) => (
            token.clone(),
            "unknown".to_string(),
            Vec::new(),
            Vec::new(),
            Some(format!("'{}' is not a source the gateway knows", token)),
        ),
    };

    EgressProposal {
        intent: normalized,
        subject: subject_text,
        kind,
        rules,
        mcp_servers: catalog.mcp_server_names.clone(),
        known_sources,
        note,
    }
}

/// MCP server names from the gateway's MCP registry file, without connecting
/// to any server (RFC §4.3 "MCP server list"). Empty when the registry env
/// var or file is absent.
pub fn mcp_server_names_from_env() -> Vec<String> {
    let Ok(path) = std::env::var("AUTONOETIC_MCP_REGISTRY_PATH") else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(servers) = serde_json::from_str::<Vec<autonoetic_mcp::McpServer>>(&raw) else {
        return Vec::new();
    };
    let mut names: Vec<String> = servers.into_iter().map(|s| s.name).collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> SourceCatalog {
        SourceCatalog {
            tool_names: [
                "email_read".to_string(),
                "email_list".to_string(),
                "email_send".to_string(),
                "wiki_get".to_string(),
                "wiki_propose".to_string(),
                "sandbox_exec".to_string(),
                "sandbox_script".to_string(),
                "content_read".to_string(),
                "content_write".to_string(),
                "web_search".to_string(),
                "web_fetch".to_string(),
            ]
            .into_iter()
            .collect(),
            mcp_server_names: vec!["gmail".to_string(), "outlook".to_string()],
        }
    }

    #[test]
    fn parses_every_recognized_phrasing() {
        assert_eq!(
            parse_intent("emails stay local"),
            Some(IntentSubject::Tool("emails".to_string()))
        );
        assert_eq!(
            parse_intent("keep emails local"),
            Some(IntentSubject::Tool("emails".to_string()))
        );
        assert_eq!(
            parse_intent("Email stays on this machine."),
            Some(IntentSubject::Tool("email".to_string()))
        );
        assert_eq!(
            parse_intent("keep ~/mail off the network"),
            Some(IntentSubject::Path("~/mail".to_string()))
        );
        assert_eq!(
            parse_intent("~/mail must not leave this machine"),
            Some(IntentSubject::Path("~/mail".to_string()))
        );
        assert_eq!(
            parse_intent("make emails stay local"),
            Some(IntentSubject::Tool("emails".to_string()))
        );
        assert_eq!(
            parse_intent("local: email"),
            Some(IntentSubject::Tool("email".to_string()))
        );
        assert_eq!(parse_intent("email"), Some(IntentSubject::Tool("email".to_string())));
        assert_eq!(parse_intent("~/mail"), Some(IntentSubject::Path("~/mail".to_string())));
    }

    #[test]
    fn refuses_non_local_phrases() {
        assert_eq!(parse_intent(""), None);
        assert_eq!(parse_intent("   "), None);
        assert_eq!(parse_intent("emails should be deleted"), None);
        assert_eq!(parse_intent("send everything to the cloud"), None);
        assert_eq!(parse_intent("what is the weather?"), None);
    }

    #[test]
    fn maps_plural_tool_family_to_glob_rules() {
        let proposal = build_egress_proposal("emails stay local", &catalog());
        assert_eq!(proposal.kind, "tool");
        assert_eq!(proposal.subject, "emails");
        assert_eq!(proposal.rules.len(), 1);
        let r = &proposal.rules[0];
        assert_eq!(r.rule.source, "email.*");
        assert_eq!(r.rule.path, None);
        assert_eq!(r.rule.label, NamedEgressLabel::LocalOnly.to_label());
        assert!(r.rationale.contains("email_read"));
        assert!(r.rationale.contains("email_send"));
        assert!(proposal.note.is_none());
    }

    #[test]
    fn maps_mcp_server_subject_to_server_glob() {
        let proposal = build_egress_proposal("keep gmail local", &catalog());
        assert_eq!(proposal.kind, "mcp_server");
        assert_eq!(proposal.rules.len(), 1);
        assert_eq!(proposal.rules[0].rule.source, "mcp.gmail.*");
        assert!(proposal.rules[0].rationale.contains("gmail"));
    }

    #[test]
    fn maps_exact_tool_name_without_glob() {
        let proposal = build_egress_proposal("keep web_search local", &catalog());
        assert_eq!(proposal.kind, "tool");
        assert_eq!(proposal.rules.len(), 1);
        assert_eq!(proposal.rules[0].rule.source, "web_search");
    }

    #[test]
    fn unicode_near_miss_matching_does_not_panic() {
        // `shares_ngram` must stay char-boundary safe — an unregistered
        // Unicode server name (registry files are unvalidated) must not panic
        // while generating near-miss suggestions.
        let mut catalog = catalog();
        catalog.mcp_server_names = vec![
            "gmail".to_string(),
            "café".to_string(),
            "日本語ボックス".to_string(),
        ];
        let proposal = build_egress_proposal("keep café local", &catalog);
        assert_eq!(proposal.kind, "mcp_server");
        assert_eq!(proposal.rules.len(), 1);
        assert_eq!(proposal.rules[0].rule.source, "mcp.café.*");
        let near_miss = build_egress_proposal("keep 日本語ボックス local", &catalog);
        assert_eq!(near_miss.kind, "mcp_server");
        assert!(near_miss.rules.iter().any(|r| r.rule.source == "mcp.日本語ボックス.*"));
        // Near-miss path with a non-matching Unicode token: 3-gram matching
        // runs over chars, never slicing bytes.
        let miss = build_egress_proposal("keep 日本語ボx local", &catalog);
        assert!(miss.rules.is_empty());
        assert!(miss.known_sources.contains(&"mcp.日本語ボックス.*".to_string()));
    }

    #[test]
    fn maps_path_subject_to_path_convention_rules() {
        let proposal = build_egress_proposal("~/mail must not leave this machine", &catalog());
        assert_eq!(proposal.kind, "path");
        assert_eq!(proposal.rules.len(), PATH_SOURCE_FAMILIES.len());
        let sources: Vec<&str> = proposal.rules.iter().map(|r| r.rule.source.as_str()).collect();
        assert!(sources.contains(&"fs.read"));
        assert!(sources.contains(&"sandbox.exec"));
        for r in &proposal.rules {
            assert_eq!(r.rule.path.as_deref(), Some("~/mail/**"));
            assert_eq!(r.rule.label, NamedEgressLabel::LocalOnly.to_label());
        }
    }

    #[test]
    fn unknown_subject_proposes_nothing_and_lists_near_misses() {
        let proposal = build_egress_proposal("keep quantum local", &catalog());
        assert!(proposal.rules.is_empty());
        assert!(proposal.note.as_deref().unwrap().contains("quantum"));
        // "email" is a known family that shares no letters — expect empty-ish
        // known list, but the shape stays reviewable.
        assert!(!proposal.known_sources.contains(&"email.*".to_string()));
    }

    #[test]
    fn unknown_subject_suggests_substring_near_misses() {
        let proposal = build_egress_proposal("keep mailx local", &catalog());
        assert!(proposal.rules.is_empty());
        assert!(proposal.known_sources.contains(&"email.*".to_string()));
        assert!(proposal.known_sources.contains(&"mcp.gmail.*".to_string()));
    }

    #[test]
    fn identical_input_produces_identical_proposal() {
        let a = build_egress_proposal("emails stay local", &catalog());
        let b = build_egress_proposal("emails stay local", &catalog());
        assert_eq!(a, b);
    }

    #[test]
    fn empty_catalog_proposes_nothing() {
        let empty = SourceCatalog::default();
        let proposal = build_egress_proposal("emails stay local", &empty);
        assert!(proposal.rules.is_empty());
        assert!(proposal.note.is_some());
    }

    #[test]
    fn glob_rule_actually_matches_registered_tools() {
        let catalog = catalog();
        let proposal = build_egress_proposal("emails stay local", &catalog);
        for r in &proposal.rules {
            let matches: Vec<&String> = catalog
                .tool_names
                .iter()
                .filter(|t| autonoetic_types::egress::source_pattern_matches(&r.rule.source, t))
                .collect();
            assert!(!matches.is_empty(), "rule {} matches nothing", r.rule.source);
        }
    }

    #[test]
    fn path_rules_survive_round_trip_validation() {
        let proposal = build_egress_proposal("~/mail must not leave this machine", &catalog());
        let policy = autonoetic_types::egress::EgressSessionPolicy {
            rules: proposal.rules.iter().map(|p| p.rule.clone()).collect(),
            default_label: None,
            provider_constraint: None,
        };
        policy.validate().expect("proposed rules must validate");
    }
}
