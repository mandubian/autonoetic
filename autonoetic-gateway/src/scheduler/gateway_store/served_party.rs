//! Served-party attribution — *on whose behalf* a run executes (#822).
//!
//! `PrincipalKind::ServedUser` has existed since #359, but until now nothing
//! emitted it at session ingress: its only call sites were decider attribution
//! and a storage back-fill. So every session ever run is history that cannot be
//! re-attributed to a served party. That is the append-only argument
//! (`docs/concepts/philosophy.md` §4.7) — *history that wasn't attributed can
//! never be re-attributed* — applied to the §12 door.
//!
//! Today the operator and the served user are usually the same person, which is
//! precisely why this is cheap now and impossible later: the moment a hosted or
//! multi-tenant deployment makes them diverge, no amount of work recovers the
//! attribution for runs that happened before.
//!
//! ## What this is not
//!
//! Purely attributive. It enforces nothing. `U-1`/`U-2`/`U-3` are `MISSING` in
//! the active constitution — no mechanism honours a refusal, packages an
//! account, or exits with data — and a row here does not change that. Binding
//! events therefore carry **no** `enforced_rules`: tagging them `U-*` would
//! claim an enforcement that does not exist, which is the exact overclaim the
//! diagram and philosophy audits (#1261) kept removing.
//!
//! ## Write-once
//!
//! The binding is keyed by **root** session and never overwritten. A run whose
//! served party could change mid-flight would be worthless as evidence — the
//! question "who was this done for?" must have one answer per run. A second
//! attempt with a *different* principal is a caller bug and is reported, not
//! silently applied; a repeat of the *same* principal is idempotent.

use anyhow::Result;
use autonoetic_types::principal::{Principal, PrincipalKind};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// How a served party came to be recorded. A defaulted row is an *assumption*
/// (nobody said, so the operator stands in); a declared row is a caller's
/// statement. A future §12 mechanism must be able to tell them apart before it
/// honours anything on the served party's behalf.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServedPartySource {
    /// The caller named a served party at ingress.
    Declared,
    /// Nobody did; the operator is recorded as the party served.
    OperatorDefault,
}

impl ServedPartySource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::OperatorDefault => "operator_default",
        }
    }

    fn from_str(s: &str) -> Self {
        match s {
            "declared" => Self::Declared,
            // Unknown values fail *safe*: an unrecognized source is treated as
            // an assumption, never as a caller's declaration.
            _ => Self::OperatorDefault,
        }
    }
}

/// The recorded answer to "who was this run done for?".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServedParty {
    pub root_session_id: String,
    pub principal: Principal,
    pub source: ServedPartySource,
    pub recorded_at: String,
}

/// Outcome of a bind attempt, so callers can tell a first binding from a
/// no-op — the causal event is only worth emitting for the former.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindOutcome {
    /// First binding for this root session.
    Bound,
    /// Already bound to the same principal — idempotent replay.
    AlreadyBound,
    /// Already bound to a *different* principal. The stored value is kept.
    Conflict { existing: Principal },
}

/// What an ingress-supplied served-party token meant.
///
/// Three states, not two, because "the caller said nothing" and "the caller
/// said something I don't understand" must not collapse: both end up recording
/// the operator, but the second is a caller bug worth surfacing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServedPartyToken {
    /// No token supplied.
    Unspecified,
    /// A recognized principal.
    Party(Principal),
    /// A token was supplied in a form this gateway does not accept.
    Unrecognized,
}

/// Interpret an ingress-supplied served-party token.
///
/// **Exactly two forms are accepted**: `user:<id>` for a served user (the
/// convention `principal::decider_principal_kind` already recognizes, so one
/// spelling means one thing across the codebase), and the bare word `operator`.
///
/// A bare token is deliberately *not* read as a user id. Rule Zero — "rejection
/// is cheap; permission is explicit… never grant 'probably okay'" — applies to
/// parsing too: guessing would silently turn a typo (`operatorr`) into a served
/// user of that name, and this row is meant to be *evidence*. One character of
/// prefix buys the difference between a record and a guess.
pub fn parse_served_party_token(token: &str) -> ServedPartyToken {
    let s = token.trim();
    if s.is_empty() {
        return ServedPartyToken::Unspecified;
    }
    match s.strip_prefix("user:") {
        // `user:` with nothing after it names no one.
        Some(rest) if rest.trim().is_empty() => ServedPartyToken::Unrecognized,
        Some(rest) => ServedPartyToken::Party(Principal::served_user(rest.trim())),
        None if s == "operator" => ServedPartyToken::Party(Principal::human("operator")),
        None => ServedPartyToken::Unrecognized,
    }
}

/// The party a run serves when the caller named nobody: the operator.
pub fn operator_default() -> Principal {
    Principal::human("operator")
}

/// Bind the served party for a root session. Write-once (see module docs).
pub(super) fn bind_served_party(
    conn: &Connection,
    root_session_id: &str,
    principal: &Principal,
    source: ServedPartySource,
    now: &str,
) -> Result<BindOutcome> {
    if let Some(existing) = get_served_party(conn, root_session_id)? {
        return Ok(if existing.principal == *principal {
            BindOutcome::AlreadyBound
        } else {
            BindOutcome::Conflict {
                existing: existing.principal,
            }
        });
    }

    conn.execute(
        "INSERT OR IGNORE INTO session_served_party
             (root_session_id, principal_id, principal_kind, source, recorded_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            root_session_id,
            principal.id,
            principal.kind_to_storage(),
            source.as_str(),
            now,
        ],
    )?;
    Ok(BindOutcome::Bound)
}

/// Read the served party for a root session, if one was ever bound.
pub(super) fn get_served_party(
    conn: &Connection,
    root_session_id: &str,
) -> Result<Option<ServedParty>> {
    let row = conn
        .query_row(
            "SELECT root_session_id, principal_id, principal_kind, source, recorded_at
               FROM session_served_party WHERE root_session_id = ?1",
            params![root_session_id],
            |row| {
                let id: String = row.get(1)?;
                let kind: String = row.get(2)?;
                let source: String = row.get(3)?;
                Ok(ServedParty {
                    root_session_id: row.get(0)?,
                    principal: Principal {
                        kind: Principal::kind_from_storage(&kind),
                        id,
                    },
                    source: ServedPartySource::from_str(&source),
                    recorded_at: row.get(4)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Record the served party at session ingress, and put the first binding on the
/// causal chain.
///
/// Lives here rather than inline in the router because it is testable in
/// isolation: a successful `event.ingest` dispatches a real agent turn, so the
/// router's own tests can only reach failure paths.
///
/// **Best-effort.** This is attribution, not enforcement, and must never be the
/// reason a turn fails to run — every failure is logged and swallowed. Returns
/// the outcome for tests and callers that care; `None` means the store refused
/// the write (already logged).
pub fn record_at_ingress(
    store: &super::GatewayStore,
    root_session_id: &str,
    agent_id: &str,
    token: Option<&str>,
    now: &str,
) -> Option<BindOutcome> {
    let (principal, source) = match token.map(parse_served_party_token) {
        Some(ServedPartyToken::Party(p)) => (p, ServedPartySource::Declared),
        // The caller tried to name someone and got the form wrong. Record the
        // operator like any unspecified run — attribution must not fail a turn —
        // but never silently: a dropped intent that looks like a successful
        // declaration is worse than no declaration at all.
        Some(ServedPartyToken::Unrecognized) => {
            tracing::warn!(
                target: "served_party",
                session_id = %root_session_id,
                "served_party token not in an accepted form (`user:<id>` or `operator`); \
                 recording the operator as the served party"
            );
            (operator_default(), ServedPartySource::OperatorDefault)
        }
        Some(ServedPartyToken::Unspecified) | None => {
            (operator_default(), ServedPartySource::OperatorDefault)
        }
    };

    let outcome = match store.bind_served_party(root_session_id, &principal, source, now) {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(
                target: "served_party",
                session_id = %root_session_id,
                error = %e,
                "failed to record served-party attribution"
            );
            return None;
        }
    };

    match &outcome {
        // Only a first binding is news; replays are silent.
        BindOutcome::Bound => {
            let event = autonoetic_types::causal_chain::CausalEventRecord {
                event_id: format!("served-{}", uuid::Uuid::new_v4()),
                agent_id: agent_id.to_string(),
                session_id: root_session_id.to_string(),
                turn_id: None,
                event_seq: 0,
                timestamp: now.to_string(),
                category: "session".to_string(),
                action: "served_party_bound".to_string(),
                status: "recorded".to_string(),
                // Deliberately empty: §12 is MISSING, so naming a `U-*` clause
                // here would claim an enforcement that does not exist.
                enforced_rules: Vec::new(),
                target: Some(principal.id.clone()),
                payload: Some(
                    serde_json::json!({
                        "principal_id": principal.id,
                        "principal_kind": principal.kind_to_storage(),
                        "source": source.as_str(),
                    })
                    .to_string(),
                ),
                payload_ref: None,
                evidence_ref: None,
                reason: Some(
                    "recorded on whose behalf this run executes (#822); \
                     attributive only — §12 is not enforced"
                        .to_string(),
                ),
            };
            if let Err(e) = store.create_causal_event(&event) {
                tracing::debug!(
                    target: "served_party",
                    session_id = %root_session_id,
                    error = %e,
                    "served-party binding recorded, causal event not written"
                );
            }
        }
        BindOutcome::AlreadyBound => {}
        // A run that changes whom it serves mid-flight is a caller bug. The
        // first binding stands; say so loudly rather than silently keeping or
        // silently replacing it.
        BindOutcome::Conflict { existing } => {
            tracing::warn!(
                target: "served_party",
                session_id = %root_session_id,
                existing = %existing.id,
                requested = %principal.id,
                "ingest named a different served party for a bound root session; \
                 keeping the original binding"
            );
        }
    }
    Some(outcome)
}

/// True when the recorded party is a served user genuinely distinct from the
/// operator — i.e. the case §12 exists for. Useful to a future U-* mechanism
/// deciding whether it has a real principal to serve or only the default.
pub fn is_distinct_served_user(p: &ServedParty) -> bool {
    p.source == ServedPartySource::Declared && matches!(p.principal.kind, PrincipalKind::ServedUser)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_parsing_accepts_only_the_two_declared_forms() {
        use ServedPartyToken::*;

        assert_eq!(
            parse_served_party_token("user:alice"),
            Party(Principal::served_user("alice"))
        );
        // A dotted user id must not be mistaken for an agent, matching
        // `decider_principal_kind`'s ordering.
        assert_eq!(
            parse_served_party_token("user:alice.smith"),
            Party(Principal::served_user("alice.smith"))
        );
        assert_eq!(
            parse_served_party_token("operator"),
            Party(Principal::human("operator"))
        );

        // Nothing said.
        assert_eq!(parse_served_party_token(""), Unspecified);
        assert_eq!(parse_served_party_token("   "), Unspecified);

        // Said badly. A bare token is *not* guessed at as a user id: a typo
        // must never become a served party of that name (Rule Zero — never
        // grant "probably okay"), and `user:` with nothing after it names no
        // one.
        assert_eq!(parse_served_party_token("acme-tenant-7"), Unrecognized);
        assert_eq!(parse_served_party_token("operatorr"), Unrecognized);
        assert_eq!(parse_served_party_token("alice"), Unrecognized);
        assert_eq!(parse_served_party_token("user:"), Unrecognized);
        assert_eq!(parse_served_party_token("user:   "), Unrecognized);
    }

    /// A malformed token must not masquerade as a successful declaration: it
    /// records the operator, and records it as a *default*, so nothing
    /// downstream reads it as "the caller named this party".
    #[test]
    fn unrecognized_token_records_the_operator_as_a_default() {
        let (_d, store) = open_store();
        record_at_ingress(&store, "root-bad", "planner.default", Some("alice"), "t0");

        let bound = store.get_served_party("root-bad").unwrap().unwrap();
        assert_eq!(bound.principal, operator_default());
        assert_eq!(bound.source, ServedPartySource::OperatorDefault);
        assert!(!is_distinct_served_user(&bound));
    }

    #[test]
    fn source_round_trips_and_unknown_fails_safe() {
        for s in [
            ServedPartySource::Declared,
            ServedPartySource::OperatorDefault,
        ] {
            assert_eq!(ServedPartySource::from_str(s.as_str()), s);
        }
        // An unrecognized column value must never be read back as a caller's
        // declaration.
        assert_eq!(
            ServedPartySource::from_str("something-else"),
            ServedPartySource::OperatorDefault
        );
    }

    fn open_store() -> (
        tempfile::TempDir,
        crate::scheduler::gateway_store::GatewayStore,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::scheduler::gateway_store::GatewayStore::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn binding_is_write_once_per_root_session() {
        let (_d, store) = open_store();
        let alice = Principal::served_user("alice");

        assert_eq!(
            store
                .bind_served_party("root-1", &alice, ServedPartySource::Declared, "t0")
                .unwrap(),
            BindOutcome::Bound
        );
        // Replaying the same principal is idempotent, not an error — pump
        // retries and reconnects must not be treated as conflicts.
        assert_eq!(
            store
                .bind_served_party("root-1", &alice, ServedPartySource::Declared, "t1")
                .unwrap(),
            BindOutcome::AlreadyBound
        );

        // A *different* principal is reported, and the original stands.
        let bob = Principal::served_user("bob");
        assert_eq!(
            store
                .bind_served_party("root-1", &bob, ServedPartySource::Declared, "t2")
                .unwrap(),
            BindOutcome::Conflict {
                existing: alice.clone()
            }
        );
        let stored = store.get_served_party("root-1").unwrap().unwrap();
        assert_eq!(
            stored.principal, alice,
            "first binding must survive a conflict"
        );
        assert_eq!(
            stored.recorded_at, "t0",
            "conflict must not restamp the row"
        );
    }

    #[test]
    fn served_party_round_trips_kind_and_source() {
        let (_d, store) = open_store();

        store
            .bind_served_party(
                "root-user",
                &Principal::served_user("acme.tenant"),
                ServedPartySource::Declared,
                "t0",
            )
            .unwrap();
        let user = store.get_served_party("root-user").unwrap().unwrap();
        assert_eq!(user.principal.kind, PrincipalKind::ServedUser);
        assert_eq!(user.principal.id, "acme.tenant");
        assert_eq!(user.source, ServedPartySource::Declared);
        assert!(is_distinct_served_user(&user));

        store
            .bind_served_party(
                "root-default",
                &operator_default(),
                ServedPartySource::OperatorDefault,
                "t0",
            )
            .unwrap();
        let defaulted = store.get_served_party("root-default").unwrap().unwrap();
        assert_eq!(defaulted.principal.kind, PrincipalKind::Human);
        assert_eq!(defaulted.source, ServedPartySource::OperatorDefault);
        assert!(
            !is_distinct_served_user(&defaulted),
            "the operator standing in is not a distinct served party"
        );
    }

    /// The ingress path an unspecified caller takes: the operator is recorded,
    /// and marked as a *default* so a future §12 mechanism can tell an
    /// assumption from a declaration.
    #[test]
    fn ingress_without_a_token_defaults_to_the_operator() {
        let (_d, store) = open_store();
        let outcome = record_at_ingress(&store, "root-a", "planner.default", None, "t0");
        assert_eq!(outcome, Some(BindOutcome::Bound));

        let bound = store.get_served_party("root-a").unwrap().unwrap();
        assert_eq!(bound.principal, operator_default());
        assert_eq!(bound.source, ServedPartySource::OperatorDefault);
        assert!(!is_distinct_served_user(&bound));
    }

    #[test]
    fn ingress_with_a_user_token_records_a_distinct_served_party() {
        let (_d, store) = open_store();
        record_at_ingress(
            &store,
            "root-b",
            "planner.default",
            Some("user:alice"),
            "t0",
        );

        let bound = store.get_served_party("root-b").unwrap().unwrap();
        assert_eq!(bound.principal, Principal::served_user("alice"));
        assert_eq!(bound.source, ServedPartySource::Declared);
        assert!(is_distinct_served_user(&bound));
    }

    /// The binding must be on the chain — an attribution nobody can read back
    /// is not evidence. And it must not claim a `U-*` clause: §12 is MISSING.
    #[test]
    fn first_binding_is_recorded_on_the_causal_chain_without_claiming_a_clause() {
        let (_d, store) = open_store();
        record_at_ingress(
            &store,
            "root-c",
            "planner.default",
            Some("user:alice"),
            "t0",
        );

        let events = store
            .search_causal_events(None, Some("planner.default"), 50)
            .unwrap();
        let bound = events
            .iter()
            .find(|e| e.category == "session" && e.action == "served_party_bound")
            .expect("served_party_bound event exists");

        assert_eq!(bound.session_id, "root-c");
        assert_eq!(bound.target.as_deref(), Some("alice"));
        assert!(
            bound.enforced_rules.is_empty(),
            "§12 is not enforced; naming a U-* clause here would be an overclaim"
        );
        let payload: serde_json::Value =
            serde_json::from_str(bound.payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["principal_kind"], "served_user");
        assert_eq!(payload["source"], "declared");
    }

    /// Replays must not multiply chain entries — the pump retries ingests.
    #[test]
    fn repeat_ingress_emits_exactly_one_chain_event() {
        let (_d, store) = open_store();
        for _ in 0..3 {
            record_at_ingress(
                &store,
                "root-d",
                "planner.default",
                Some("user:alice"),
                "t0",
            );
        }
        let events = store
            .search_causal_events(None, Some("planner.default"), 50)
            .unwrap();
        let n = events
            .iter()
            .filter(|e| e.category == "session" && e.action == "served_party_bound")
            .count();
        assert_eq!(n, 1, "only the first binding is news");
    }

    /// A later ingest naming someone else must not rewrite who the run served.
    #[test]
    fn ingress_cannot_rewrite_an_existing_binding() {
        let (_d, store) = open_store();
        record_at_ingress(
            &store,
            "root-e",
            "planner.default",
            Some("user:alice"),
            "t0",
        );
        let outcome = record_at_ingress(
            &store,
            "root-e",
            "planner.default",
            Some("user:mallory"),
            "t1",
        );

        assert_eq!(
            outcome,
            Some(BindOutcome::Conflict {
                existing: Principal::served_user("alice")
            })
        );
        assert_eq!(
            store.get_served_party("root-e").unwrap().unwrap().principal,
            Principal::served_user("alice")
        );
    }

    #[test]
    fn unbound_root_session_reads_as_none() {
        let (_d, store) = open_store();
        assert!(store.get_served_party("never-bound").unwrap().is_none());
    }

    #[test]
    fn distinct_served_user_requires_both_declared_and_served_kind() {
        let declared_user = ServedParty {
            root_session_id: "r".into(),
            principal: Principal::served_user("alice"),
            source: ServedPartySource::Declared,
            recorded_at: "t".into(),
        };
        assert!(is_distinct_served_user(&declared_user));

        // The operator standing in is not a distinct served party, however it
        // is spelled.
        let defaulted = ServedParty {
            source: ServedPartySource::OperatorDefault,
            principal: Principal::human("operator"),
            ..declared_user.clone()
        };
        assert!(!is_distinct_served_user(&defaulted));

        let declared_operator = ServedParty {
            source: ServedPartySource::Declared,
            principal: Principal::human("operator"),
            ..declared_user.clone()
        };
        assert!(!is_distinct_served_user(&declared_operator));
    }
}
