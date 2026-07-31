//! Stored-content egress filtering (RFC data-envelopes §6).
//!
//! Memories, execution traces, and other durable surfaces carry an
//! [`EgressLabel`]. At query time the gateway drops or substitutes indications
//! when the caller's target sink is excluded — the same rule as the LLM
//! chokepoint, applied to cross-session re-entry paths.

use autonoetic_types::egress::{
    EgressConfig, EgressLabel, Indication, IndicationVerbosity, NamedEgressLabel, Sink,
};

/// Resolve the effective label for a stored record (RFC §6.7).
///
/// - `Some(label)` → use it (Phase 3+ writes).
/// - `None` (legacy NULL column) → configured `legacy_unlabeled` default.
pub fn resolve_stored_label(
    stored: Option<&EgressLabel>,
    cfg: &EgressConfig,
) -> EgressLabel {
    stored
        .cloned()
        .unwrap_or_else(|| cfg.legacy_unlabeled.to_label())
}

/// Whether `label` permits shipping raw content to `sink`.
pub fn stored_allows_sink(label: &EgressLabel, sink: Sink) -> bool {
    label.allows(sink)
}

/// Outcome of filtering one stored content blob for a sink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilteredStoredContent {
    /// Content may reach the sink verbatim.
    Allowed(String),
    /// Content withheld — indication text only (never the original bytes).
    Withheld { indication: String },
}

/// Filter stored content for a target sink (RFC §6).
///
/// When the label excludes the sink, returns an indication built from
/// metadata only (`tool` / `kind` name + label display). Never embeds content.
pub fn filter_or_indicate_for_sink(
    content: &str,
    label: &EgressLabel,
    sink: Sink,
    kind: Option<&str>,
    verbosity: IndicationVerbosity,
) -> FilteredStoredContent {
    if label.allows(sink) {
        return FilteredStoredContent::Allowed(content.to_string());
    }
    let indication = Indication::generate(kind, 1, label, verbosity);
    let text = match verbosity {
        IndicationVerbosity::Terse => indication
            .terse
            .clone()
            .unwrap_or_else(|| indication.text.clone()),
        IndicationVerbosity::Descriptive => indication.text,
    };
    FilteredStoredContent::Withheld { indication: text }
}

/// Fail-closed query sink when the caller has not resolved a provider class:
/// treat as [`Sink::RemoteModel`] so unlabeled/tainted content is not leaked
/// into a remote-shaped request by accident.
pub fn query_sink_or_remote(sink: Option<Sink>) -> Sink {
    sink.unwrap_or(Sink::RemoteModel)
}

/// Parse a legacy-unlabeled named label from config (for tests / CLI).
pub fn legacy_label_from_named(named: NamedEgressLabel) -> EgressLabel {
    named.to_label()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(legacy: NamedEgressLabel) -> EgressConfig {
        EgressConfig {
            legacy_unlabeled: legacy,
            ..Default::default()
        }
    }

    #[test]
    fn resolve_none_uses_legacy_default() {
        let c = cfg(NamedEgressLabel::Unrestricted);
        assert!(resolve_stored_label(None, &c).is_unrestricted());
        let c = cfg(NamedEgressLabel::NoRemoteModel);
        assert_eq!(
            resolve_stored_label(None, &c),
            EgressLabel::no_remote_model()
        );
    }

    #[test]
    fn local_only_withheld_from_remote() {
        let out = filter_or_indicate_for_sink(
            "CANARY-SECRET",
            &EgressLabel::local_only(),
            Sink::RemoteModel,
            Some("knowledge_recall"),
            IndicationVerbosity::Descriptive,
        );
        match out {
            FilteredStoredContent::Withheld { indication } => {
                assert!(!indication.contains("CANARY"));
                assert!(indication.contains("withheld") || indication.contains("local_only"));
            }
            FilteredStoredContent::Allowed(_) => panic!("must withhold"),
        }
    }

    #[test]
    fn local_only_allowed_for_local_model() {
        let out = filter_or_indicate_for_sink(
            "CANARY-SECRET",
            &EgressLabel::local_only(),
            Sink::LocalModel,
            Some("knowledge_recall"),
            IndicationVerbosity::Descriptive,
        );
        assert_eq!(out, FilteredStoredContent::Allowed("CANARY-SECRET".into()));
    }
}
