//! Declared bind-direction for **every** constitutional clause — the law side
//! of RFC #1283 (#1284 part 2).
//!
//! [`crate::enforcement_register`] answers *how this implementation enforces a
//! clause*: code sites, tests, config knobs. That is conformance data, and it
//! can only describe clauses that something enforces. This module answers a
//! different question — *what does the clause oblige, of whom, to whom* — which
//! is a property of the law itself and exists whether or not any code
//! implements it.
//!
//! The split is not tidiness. It is forced, twice over:
//!
//! - `U-1`–`U-3` are `MISSING`: nothing honours a served party's refusal,
//!   packages an account, or exits with their data. They cannot enter the
//!   enforcement register, which requires an enforcement entry per clause
//!   (`every_principle_and_right_has_at_least_one_entry`). But they are
//!   enacted law and they bind someone, so their direction must be
//!   recordable.
//! - The register holds 19 clauses; the constitution declares **221**. Bind
//!   direction that only exists for the enforced 9% is not a property of the
//!   constitution, it is a property of our test coverage.
//!
//! **One source of truth.** [`crate::enforcement_register::binds`] and its
//! siblings read this table. The register's clause structs deliberately do
//! *not* carry the fields: two places holding one relation is the defect RFC
//! §6.4 exists to prevent, and it already happened once — `self_describe`
//! sourced rights from the register while hardcoding their bind direction as
//! a literal, so the one place that could disagree with the register did.
//!
//! # `verified_by` — **under revision, do not build on it**
//!
//! RFC #1283 **rev 3 §2.4.1 supersedes this field's model.** Read that before
//! adding a `verified_by` value or reasoning about one; the account below is
//! kept because the values in this table were assigned under it, not because
//! it is the intended design.
//!
//! Two defects, both found by implementing it:
//!
//! - **The "floor" needs a total order this model denies.** `Construction` is
//!   not stronger than `Detection` for a universal negative over behaviour
//!   (`I-4`), so "at least X" is not well-formed. The `Ord` derives that
//!   asserted such an order are gone.
//! - **The field means two different things and nothing marks which.** For an
//!   ENFORCED clause the value describes *this implementation* — `Ri-0.3` is
//!   `Test` because `Tagged::permission_with_rules` does not forbid an empty
//!   rule list, a fact about our types rather than about what the clause
//!   demands. For an unmet clause it states a *requirement*: `I-3` declares
//!   `Construction` while its status is `PARTIAL`, naming what closing the gap
//!   needs. One column, two semantics.
//!
//! Replacement: `requires` (`preventive | detective`, constitutional — the
//! distinction that survives re-implementation) plus `achieved`
//! (modality + site, per implementation, register-side). Enforcement *status*
//! stays in the constitution's status cell and the register; neither field
//! duplicates it.
//!
//! The old text read "`verified_by` is a requirement, not a claim". That was
//! the intent and is not what the data does — which is exactly the
//! description-vs-requirement defect, committed in the doc comment that
//! warned against it.
//!
//! # Coverage
//!
//! Complete: `Ri-*` (18), `O-*` (4), `U-*` (3), `I-*` (14), and §5, §9, §15
//! of the numbered `P-*` — 73 clauses.
//!
//! **148 numbered `P-*` remain**, across §1, §2, §3, §4, §6, §7, §8, §10 and
//! §11. They are classified per section because "who must comply" is a
//! semantic judgement per clause, which is why #1284 sequences them as
//! separate reviewable tranches. They are not silently absent:
//! [`unclassified_clauses`] enumerates them and
//! `tests::the_unclassified_count_is_a_ratchet` pins the exact number, so a
//! new clause cannot arrive unclassified and each tranche has to lower the
//! constant.
//!
//! # Why §5, §9 and §15 came first
//!
//! Not size — **inference debt**. Before this tranche, `binds("P-5.2")`
//! answered from the *enforcement register's section grouping* `P-5`, reached
//! by parent lookup. That is precisely the inheritance this module warns
//! against below: it assumes every clause in a section binds what the section
//! binds. 84 clauses across §2, §5, §7, §9 and §15 were resolving that way, so
//! the coverage figure part 2 reported was mostly answered by a guess.
//!
//! Classifying a section converts its clauses from inherited to declared and
//! the guess stops being load-bearing for them. §2 and §7 are the remaining
//! 51; once they land, the register's section groupings can be deleted
//! outright.

use crate::enforcement_register::{Binds, OwedTo, VerifiedBy};
use autonoetic_types::principal::PrincipalKindTag;

/// A clause's declared relational fields, independent of whether anything
/// enforces it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relation {
    pub id: &'static str,
    /// Which power must comply.
    pub binds: Binds,
    /// Who has standing to invoke it.
    pub owed_to: OwedTo,
    /// Minimum modality that would establish compliance (see module docs —
    /// a requirement, not a claim about this implementation).
    pub verified_by: VerifiedBy,
}

/// Shorthand: owed to the agent under governance.
const TO_AGENT: OwedTo = OwedTo::Principal(PrincipalKindTag::AutonoeticAgent);
/// Shorthand: owed to the end user a session ultimately serves.
const TO_SERVED: OwedTo = OwedTo::Principal(PrincipalKindTag::ServedUser);

const fn right(id: &'static str, verified_by: VerifiedBy) -> Relation {
    Relation { id, binds: Binds::Enforcer, owed_to: TO_AGENT, verified_by }
}

const fn duty(id: &'static str, verified_by: VerifiedBy) -> Relation {
    Relation { id, binds: Binds::Decider, owed_to: TO_AGENT, verified_by }
}

/// An enforcer duty with no invocable beneficiary — an integrity property.
const fn property(id: &'static str, verified_by: VerifiedBy) -> Relation {
    Relation { id, binds: Binds::Enforcer, owed_to: OwedTo::NoOne, verified_by }
}

/// An enforcer duty owed to the **served party** — the end user a session
/// ultimately serves.
const fn served(id: &'static str, verified_by: VerifiedBy) -> Relation {
    Relation { id, binds: Binds::Enforcer, owed_to: TO_SERVED, verified_by }
}

/// §0 Bill of Rights. Uniformly enforcer duties owed to the agent — the
/// vertical application a bill of rights has: it binds the state, not the
/// citizen.
///
/// `Ri-0.15` is the one exception, and it was found by reading rather than
/// assumed: its `DecisionContext` is owed to *whoever decides the gate*,
/// human or agent, so its standing attaches to the **seat** rather than to a
/// principal kind. Its own text says so — "to every decider (human or
/// agent)… this is the gateway's mirror of O-1". A scheme that derived
/// `owed_to` from the `Ri-` prefix would have gotten it wrong, which is the
/// whole argument for declaring it.
fn rights() -> Vec<Relation> {
    vec![
        // Enforced by the per-turn signed state attestation block.
        right("Ri-0.1", VerifiedBy::Test),
        right("Ri-0.2", VerifiedBy::Test),
        // `Tagged::permission_with_rules` carries rule IDs, but nothing in
        // the type forbids an empty list, so an example test at the named
        // site is the honest floor. Making a ruleless rejection
        // unrepresentable would be a real strengthening.
        right("Ri-0.3", VerifiedBy::Test),
        right("Ri-0.4", VerifiedBy::Test),
        right("Ri-0.5", VerifiedBy::Test),
        right("Ri-0.6", VerifiedBy::Test),
        right("Ri-0.7", VerifiedBy::Test),
        right("Ri-0.8", VerifiedBy::Test),
        right("Ri-0.9", VerifiedBy::Test),
        right("Ri-0.10", VerifiedBy::Test),
        // Shares P-8.1's substrate: `compute_entry_hash` binds `actor_id`, so
        // reattribution is detectable by recomputation.
        right("Ri-0.11", VerifiedBy::Chokepoint),
        // `YieldReason` is a closed enum: an unlisted termination is a
        // compile error at every exhaustive match.
        right("Ri-0.12", VerifiedBy::Construction),
        // Policy decision signatures do not take reasoning as a parameter,
        // so no call site can consult it — including ones not yet written.
        right("Ri-0.13", VerifiedBy::Construction),
        right("Ri-0.14", VerifiedBy::Test),
        // Seat-standing, and `construction`: `DecisionContext` is a
        // *required* field on `human_gate.rs::GateRequest`, so a gate
        // without context cannot be built. (`GateService::check` rejecting
        // boilerplate is a chokepoint layered on top; the floor is the
        // structural guarantee underneath.)
        Relation {
            id: "Ri-0.15",
            binds: Binds::Enforcer,
            owed_to: OwedTo::Seat(Binds::Decider),
            verified_by: VerifiedBy::Construction,
        },
        // `is_advisory_only` is a runtime predicate, not a type, so the
        // "never raises a blocking gate" guarantee rests on tests.
        right("Ri-0.16", VerifiedBy::Test),
        right("Ri-0.17", VerifiedBy::Test),
        right("Ri-0.18", VerifiedBy::Test),
    ]
}

/// §O decider obligations. Bind the **seat**, so an agent holding
/// `GateDecider` (P-2.20) is bound identically to a human operator — no
/// special case, which is what made `Binds` range over powers rather than
/// occupants.
fn obligations() -> Vec<Relation> {
    vec![
        // BLOCKING at the `decide_request_with_options` chokepoint: the
        // decision does not commit until a non-empty reason is recorded.
        duty("O-1", VerifiedBy::Chokepoint),
        // "Cannot be reattributed" inherits Ri-0.11's hash binding.
        duty("O-2", VerifiedBy::Chokepoint),
        // `Detection` is the *correct* floor, not a weak one: nothing static
        // can prove a decider will act within a deadline, so the enforceable
        // form is recording and counting the breach.
        duty("O-6", VerifiedBy::Detection),
        duty("O-7", VerifiedBy::Detection),
    ]
}

/// §12 served-party rights. All three are `MISSING` — declared law that
/// nothing yet honours.
///
/// They bind the **enforcer**, not "the community". `philosophy.md` §3.3
/// reached this in prose before the model could say it: *"an entitlement in
/// §12 would be a claim, whereas an invariant on the enforcer is a
/// guarantee."* An aggregate like `community` is "gateway + agents", and a
/// clause that appears to bind it binds whichever party implements the
/// mechanism.
///
/// The floors are what closing each gap would require, and `U-1`'s is the
/// load-bearing one: refusing a delivered result is an *act*, acting needs a
/// surface, and a surface is a seat — so implementing `U-1` means giving the
/// served party a deciding seat for that act (#1274). That is a prediction
/// this table makes, not a description of anything built.
fn served_party() -> Vec<Relation> {
    vec![
        Relation { id: "U-1", binds: Binds::Enforcer, owed_to: TO_SERVED, verified_by: VerifiedBy::Chokepoint },
        Relation { id: "U-2", binds: Binds::Enforcer, owed_to: TO_SERVED, verified_by: VerifiedBy::Test },
        Relation { id: "U-3", binds: Binds::Enforcer, owed_to: TO_SERVED, verified_by: VerifiedBy::Test },
    ]
}

/// §13 cross-cutting invariants. Classified per clause — the `I-` prefix
/// conflates two axes (universality and bind direction), which is defect 1.4
/// of the RFC, so no uniform rule applies.
///
/// Most are integrity properties (`owed_to: NoOne`). Two are not: `I-8` and
/// `I-9` are the *mechanical restatements* of `Ri-0.13(a)` and `Ri-0.12` —
/// the same duty expressed as a universal rather than an existential — so
/// they carry the same standing as the rights they restate. `I-8`'s own text
/// says it: "this is the mechanical form of Ri-0.13(a)".
///
/// That is a finer distinction than it looks, and worth stating because the
/// neighbouring case goes the other way. `P-8.1` (hash-chain integrity) is
/// the *substrate* `Ri-0.11` depends on, not a restatement of it: chain
/// integrity also serves audit, forensics and operator trust, so it is owed
/// to no one in particular while `Ri-0.11` is owed to the agent. Restatement
/// inherits standing; substrate does not.
fn invariants() -> Vec<Relation> {
    vec![
        // N paths reduced to 1 (`tool_call_processor.rs`) plus a guard on
        // bypassing the 1.
        property("I-1", VerifiedBy::Chokepoint),
        // fsync-before-transition ordering, via P-8.16.
        property("I-2", VerifiedBy::Chokepoint),
        // Status is PARTIAL while the floor is `Construction`: the clause's
        // own text names what closing it requires — `RedactedPayload` at the
        // store write API, where the compiler covers paths that do not exist
        // yet. The floor states the requirement; it does not claim we meet
        // it.
        property("I-3", VerifiedBy::Construction),
        // A universal negative over behaviour. No static check succeeds, so
        // the enforceable form is counting each lapse as a durable
        // `discretion_leak` event.
        property("I-4", VerifiedBy::Detection),
        // Needs static analysis over the source with a documented allowlist,
        // in the shape of the existing docs guards — a set comparison.
        property("I-5", VerifiedBy::Registry),
        property("I-6", VerifiedBy::Detection),
        // A meta-rule about amendment. The mechanically enforceable residue
        // is that a conflict *escalates* rather than resolving silently,
        // which is observable only when it happens.
        property("I-7", VerifiedBy::Detection),
        // The mechanical form of Ri-0.13(a) — same duty, universal form, so
        // the agent's standing carries over.
        Relation { id: "I-8", binds: Binds::Enforcer, owed_to: TO_AGENT, verified_by: VerifiedBy::Construction },
        // The mechanical form of Ri-0.12.
        Relation { id: "I-9", binds: Binds::Enforcer, owed_to: TO_AGENT, verified_by: VerifiedBy::Construction },
        // Property-based over generated `(capabilities, tool-call, state)`
        // inputs: cannot prove determinism, can sample it.
        property("I-10", VerifiedBy::Sampling),
        // "Every invariant has a declared failure action" is a set
        // comparison against `fail_mode.rs`.
        property("I-11", VerifiedBy::Registry),
        // DESIGN DEBT: declared before any collective mechanism exists,
        // specifically so Sybil resistance cannot be an oversight in a first
        // design. The floor says what that design must provide — weight
        // collapse structural, not checked after the fact.
        property("I-12", VerifiedBy::Construction),
        // Documents a deliberate *absence* (no capability-attenuation check).
        // What verifies an absence is a test asserting it stays absent.
        property("I-13", VerifiedBy::Test),
        // The egress instance of I-8/I-10. An integrity property rather than
        // a served-party duty: P-15 is the duty owed to the served party,
        // I-14 is the plane-integrity substrate that makes it holdable —
        // the same substrate/restatement split as P-8.1 vs Ri-0.11.
        property("I-14", VerifiedBy::Chokepoint),
    ]
}

/// Numbered `P-*` clauses classified so far — the down payment on the
/// per-section tranches (#1284 part 2).
///
/// `P-8.1` is here because it is real law that was living register-side only:
/// the register carries it as an entrenched principle, and
/// `the_register_and_the_law_table_never_disagree` refuses to let a *numbered*
/// clause be register-only. The register's other five principles (`P-2`,
/// `P-5`, `P-7`, `P-9`, `P-15`) are section-level groupings, not clauses —
/// the constitution declares `P-7.5`, never a bare `P-7` — so they stay
/// register-side and are not law.
///
/// This is deliberately not seeded by inheriting each section grouping's
/// relation down onto its numbered children. `P-15` really does pass to every
/// `P-15.*`, but `P-2` (Approval Gates) binds the enforcer while individual
/// `P-2.*` clauses may well bind the reasoner — auto-inheriting would classify
/// 182 clauses with a guess and call it declared, which is the failure this
/// whole model exists to end.
fn principles() -> Vec<Relation> {
    let mut out = vec![
        // The causal chain is the substrate every correction-machinery clause
        // stands on. `owed_to: NoOne` deliberately: Ri-0.2 and Ri-0.11 are the
        // duties owed to the agent, and P-8.1 is the property that makes them
        // satisfiable. Recording it as owed to the agent would count one
        // relationship twice.
        property("P-8.1", VerifiedBy::Chokepoint),
    ];
    out.extend(section_5());
    out.extend(section_9());
    out.extend(section_15());
    out
}

/// §5 — I/O Schema Validation. Every clause binds the **enforcer**: §5 is
/// gateway validation machinery throughout, and not one clause obliges the
/// reasoner to do anything.
///
/// The discriminating axis here is `owed_to`, and it splits the section in a
/// way the prefix could never express. Most of §5 is well-formedness — nobody
/// can *claim* that a schema was checked. Three clauses are different: the
/// hint on a failed coercion, the uniform error envelope, and repair being
/// strictly opt-in are all things the agent can demand, because each protects
/// the agent from the gateway. They are the §5 instances of the parent
/// principle's own promise — "no gateway judgment about the agent's output is
/// silent or hidden".
fn section_5() -> Vec<Relation> {
    vec![
        property("P-5.1", VerifiedBy::Chokepoint),
        // `Construction`: the LLM-coercion fallback was *removed*, so
        // "coercion is deterministic only" holds because the non-deterministic
        // path is not in `SchemaEnforcementMode` to select.
        property("P-5.2", VerifiedBy::Construction),
        // Owed to the agent — a failure that does not say what to do next is
        // the Ri-0.3 defect wearing a schema error's clothes.
        right("P-5.3", VerifiedBy::Test),
        // Logging every pass/coerce/reject is a universal over decisions, so
        // recording is the enforceable form. The *duty to log* is an integrity
        // property; the agent's right to read what was logged is Ri-0.2, and
        // conflating them would count one relationship twice.
        property("P-5.4", VerifiedBy::Detection),
        property("P-5.5", VerifiedBy::Test),
        // "Authoritative runtime state, not LLM claims" is the §5 instance of
        // I-8: the verdict is a function of recorded state, never of model
        // output.
        property("P-5.6", VerifiedBy::Test),
        property("P-5.7", VerifiedBy::Test),
        // Owed to the agent, and the reason is the DISCRETION LEAK marker on
        // the clause itself: repair means the gateway rewriting the agent's
        // output. "Strictly opt-in, defaults false, attempts clamped" is a
        // limit on the gateway held *for* the agent.
        right("P-5.8", VerifiedBy::Chokepoint),
        property("P-5.9", VerifiedBy::Test),
        property("P-5.10", VerifiedBy::Test),
        // Owed to the agent: the agent is the consumer of these errors, and a
        // uniform envelope is what makes a failure machine-actionable rather
        // than a string to guess at.
        right("P-5.11", VerifiedBy::Test),
        property("P-5.12", VerifiedBy::Test),
        property("P-5.13", VerifiedBy::Chokepoint),
        // `Construction`: `FailureClass` is a closed enum and classification
        // is a pure function of gateway-observed state.
        property("P-5.14", VerifiedBy::Construction),
    ]
}

/// §9 — Agent Install & Provenance. Enforcer throughout, and almost entirely
/// integrity properties: an agent cannot demand that its own activation be
/// gated, and would generally prefer it were not.
///
/// `P-9.12` is the exception — a health report *returned in a response* is
/// owed to whoever called for it.
fn section_9() -> Vec<Relation> {
    vec![
        property("P-9.1", VerifiedBy::Chokepoint),
        // `Construction`: "not a runtime tool" is enforced by the tool being
        // absent from the registry, so there is no call site to gate.
        property("P-9.2", VerifiedBy::Construction),
        // Content-addressing *is* immutability: a changed revision is a
        // different address.
        property("P-9.3", VerifiedBy::Construction),
        property("P-9.4", VerifiedBy::Chokepoint),
        property("P-9.5", VerifiedBy::Test),
        property("P-9.6", VerifiedBy::Test),
        property("P-9.7", VerifiedBy::Chokepoint),
        property("P-9.8", VerifiedBy::Test),
        property("P-9.9", VerifiedBy::Chokepoint),
        property("P-9.10", VerifiedBy::Test),
        property("P-9.11", VerifiedBy::Chokepoint),
        right("P-9.12", VerifiedBy::Test),
        property("P-9.13", VerifiedBy::Test),
        // DESIGN DEBT — trust domains do not constrain cross-domain spawns
        // yet. The floor states what closing it requires, not what exists.
        property("P-9.14", VerifiedBy::Chokepoint),
        // The single door: N activation surfaces reduced to one gate matrix,
        // with the startup bootstrap exception made parameter-explicit
        // (`auto_promote: bool`) rather than implicit — a guarded bypass,
        // which is what separates a chokepoint from a convention.
        property("P-9.15", VerifiedBy::Chokepoint),
        property("P-9.16", VerifiedBy::Test),
    ]
}

/// §15 — Data Egress Localization. The one section whose duties run to the
/// **served party** rather than to the agent or to no one, which is what
/// `philosophy.md` §3.3 argued in prose: *"an entitlement in §12 would be a
/// claim, whereas an invariant on the enforcer is a guarantee."*
///
/// This is also the section the old `binds()` was most wrong about. It
/// reported `agent` for all three — a party `I-14` forbids from setting,
/// stripping or reading a label at all, so the agent could not comply even in
/// principle.
fn section_15() -> Vec<Relation> {
    vec![
        served("P-15.1", VerifiedBy::Chokepoint),
        served("P-15.2", VerifiedBy::Chokepoint),
        served("P-15.3", VerifiedBy::Chokepoint),
    ]
}

/// What a clause obliges — the three declared fields, **without an id**.
///
/// Deliberately id-less. Lookups can resolve by inheritance (`P-15.1` takes
/// `P-15`'s relation), so a returned value carrying an `id` would carry the
/// *parent's* id while the caller asked about the child. Every caller that
/// then assumed `result.id == queried_id` would be quietly wrong. Removing
/// the field removes the assumption; [`declared_at`] answers the provenance
/// question explicitly for the callers that actually have it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fields {
    pub binds: Binds,
    pub owed_to: OwedTo,
    pub verified_by: VerifiedBy,
}

impl Relation {
    fn fields(self) -> Fields {
        Fields { binds: self.binds, owed_to: self.owed_to, verified_by: self.verified_by }
    }
}

/// The classified table, built once.
fn table() -> &'static [Relation] {
    static TABLE: std::sync::OnceLock<Vec<Relation>> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut out = rights();
        out.extend(obligations());
        out.extend(served_party());
        out.extend(invariants());
        out.extend(principles());
        out
    })
}

/// Every classified clause, in constitutional order.
pub fn relations() -> &'static [Relation] {
    table()
}

/// The declared relation for `clause_id`, or `None` if it is not yet
/// classified.
///
/// A numbered sub-clause inherits its parent's relation when the parent is
/// classified and the child is not — `P-15.1` from `P-15`. Inheritance is
/// from the *declared parent*, never from the ID prefix: `Ri-0.15` does not
/// inherit from `Ri-0`, because `Ri-0` is a section, not a clause.
pub fn relation(clause_id: &str) -> Option<Fields> {
    resolve(clause_id).map(|(_, fields)| fields)
}

/// Which declared clause supplied `clause_id`'s relation — itself when
/// classified directly, or the parent it inherited from. `None` when
/// unclassified.
///
/// Exists so "is this clause classified in its own right?" is answerable
/// without comparing ids to a value that may legitimately differ.
pub fn declared_at(clause_id: &str) -> Option<&'static str> {
    resolve(clause_id).map(|(id, _)| id)
}

fn resolve(clause_id: &str) -> Option<(&'static str, Fields)> {
    if let Some(exact) = table().iter().find(|r| r.id == clause_id) {
        return Some((exact.id, exact.fields()));
    }
    let parent = clause_id.split_once('.')?.0;
    table()
        .iter()
        .find(|r| r.id == parent)
        .map(|r| (r.id, r.fields()))
}

/// Clause IDs the active constitution declares that this table does not yet
/// classify, in constitutional order.
///
/// Visible and counted rather than absent — the discipline §12 already
/// applies to `U-1`–`U-3`. Each `P-*` section tranche shrinks this.
pub fn unclassified_clauses() -> Vec<String> {
    unclassified_among(&crate::constitution_digest::constitution_clause_ids())
}

/// [`unclassified_clauses`] over a caller-supplied clause list.
///
/// Split out because the loaded-constitution accessors panic when the
/// constitution runtime has not been initialized, which a pure classification
/// question has no business requiring — and which would make these checks
/// depend on global init order.
pub fn unclassified_among(clause_ids: &[String]) -> Vec<String> {
    clause_ids
        .iter()
        .filter(|id| relation(id).is_none())
        .cloned()
        .collect()
}

/// Render the **law table** — every clause the constitution declares, its
/// declared relation, and whether it is classified yet.
///
/// The reader-facing half of RFC #1283 §2.4.2. Before this, bind direction was
/// readable by code and invisible to a reader: the generated enforcement
/// register covered the 19 clauses with enforcement entries, so 54 of the 73
/// classified clauses — every `U-*`, every `I-*`, most `Ri-*` — appeared in no
/// document at all. Declaring a relation nobody can read defeats the point of
/// declaring it.
///
/// Deliberately a *separate* document from the enforcement register, because
/// they answer different questions and have different lifetimes. This one is
/// the law: what each clause obliges, of whom, to whom — the same for any
/// implementation. The register is conformance: which of *our* code sites and
/// tests hold it up. A re-implementation replaces the second and inherits the
/// first (§2.7).
///
/// Takes the clause index rather than reading the loaded constitution, so it
/// is pure and testable without booting a gateway.
pub fn render_law_table_markdown(clause_index: &[(String, String)]) -> String {
    let mut out = String::new();
    out.push_str("# Constitutional Law Table (generated)\n\n");
    out.push_str(
        "> **Generated** from `autonoetic-gateway/src/constitution_relations.rs`. Do not edit \
         by hand — run the generator (`BLESS_LAW_TABLE=1`). One row per clause the active \
         constitution declares, recording **which power it binds**, **who has standing to \
         invoke it**, and the verification field. Bind direction is declared data, never \
         derived from the ID prefix (#1284).\n\n\
         This is the **law** side: what a clause obliges, of whom, to whom — identical for any \
         implementation. Which code holds it up is conformance data and lives in \
         [`enforcement-register.md`](enforcement-register.md). See \
         `docs/proposals/constitution-bind-direction-model.md`.\n\n",
    );

    out.push_str("> ⚠️ **`verified_by` is under revision** (RFC #1283 §2.4.1). The column \
         records this implementation's mechanism for enforced clauses and a *requirement* for \
         unenforced ones — one column, two meanings. It is being replaced by `requires` \
         (constitutional) plus `achieved` (register). Read it as provisional.\n\n");

    let total = clause_index.len();
    let classified = clause_index
        .iter()
        .filter(|(id, _)| relation(id).is_some())
        .count();
    out.push_str("## Coverage\n\n");
    out.push_str(&format!(
        "**{classified} of {total}** clauses classified. The remainder are numbered `P-*` \
         awaiting their section tranche; they are counted, not hidden — a ratchet test pins \
         the exact number so a new clause cannot arrive unclassified.\n\n",
    ));

    out.push_str("| binds | clauses |\n|---|---|\n");
    for power in Binds::ALL {
        let n = clause_index
            .iter()
            .filter(|(id, _)| relation(id).map(|r| r.binds) == Some(power))
            .count();
        out.push_str(&format!("| `{}` | {} |\n", power.label(), n));
    }
    out.push('\n');

    out.push_str("| owed to | clauses | means |\n|---|---|---|\n");
    out.push_str(&format!(
        "| `autonoetic_agent` | {} | a duty the agent can invoke — an agent **right**, whatever \
         the ID prefix says |\n",
        count_owed(clause_index, |o| o == TO_AGENT)
    ));
    out.push_str(&format!(
        "| `served_user` | {} | owed to the end user a session serves |\n",
        count_owed(clause_index, |o| o == TO_SERVED)
    ));
    out.push_str(&format!(
        "| `decider` *(seat)* | {} | owed to whoever occupies the deciding seat, human or agent |\n",
        count_owed(clause_index, |o| matches!(o, OwedTo::Seat(_)))
    ));
    out.push_str(&format!(
        "| `none` | {} | an **integrity property**: no invocable beneficiary. Not a lesser \
         clause — nobody can *claim* their own sandbox confinement |\n",
        count_owed(clause_index, |o| o == OwedTo::NoOne)
    ));
    out.push('\n');

    let mut agent_rights: Vec<&str> = clause_index
        .iter()
        .filter(|(id, _)| {
            relation(id).is_some_and(|r| r.owed_to.is_agent_right(r.binds))
        })
        .map(|(id, _)| id.as_str())
        .collect();
    agent_rights.sort_unstable();
    out.push_str(&format!(
        "**Agent rights by relation** ({}): `{}`\n\nA right is a *view*, not a family: an \
         enforcer duty owed to the agent is an agent right regardless of prefix. This list is \
         therefore not the `Ri-*` set — `P-5.3`, `P-5.8`, `P-5.11` and `P-9.12` are here on \
         their relation, and §0's rights/rules ratio would be computed from this rather than \
         from prefixes.\n\n",
        agent_rights.len(),
        agent_rights.join("`, `"),
    ));

    out.push_str("## Clauses\n\n");
    out.push_str("| clause | binds | owed to | `verified_by` | statement |\n");
    out.push_str("|---|---|---|---|---|\n");
    for (id, gloss) in clause_index {
        let statement = gloss.replace('|', "\\|");
        match relation(id) {
            Some(f) => out.push_str(&format!(
                "| `{}` | `{}` | {} | `{}` | {} |\n",
                id,
                f.binds.label(),
                owed_cell(f.owed_to),
                f.verified_by.label(),
                statement,
            )),
            None => out.push_str(&format!(
                "| `{}` | — | — | — | *unclassified.* {} |\n",
                id, statement
            )),
        }
    }
    out
}

fn count_owed(index: &[(String, String)], pred: impl Fn(OwedTo) -> bool) -> usize {
    index
        .iter()
        .filter(|(id, _)| relation(id).is_some_and(|r| pred(r.owed_to)))
        .count()
}

fn owed_cell(owed: OwedTo) -> String {
    match owed {
        OwedTo::NoOne => "none *(integrity property)*".to_string(),
        OwedTo::Seat(power) => format!("`{}` *(seat)*", power.label()),
        OwedTo::Principal(kind) => format!("`{}`", kind.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn declared_ids() -> Vec<&'static str> {
        relations().iter().map(|r| r.id).collect()
    }

    /// Clause ids read straight from the active constitution *file*.
    ///
    /// Not `constitution_digest::constitution_clause_ids()`, which reads the
    /// loaded runtime and panics when the constitution has not been
    /// initialized — these checks are about the document, so they should not
    /// depend on whether some other test happened to boot a gateway first.
    fn active_constitution_clause_ids() -> Vec<String> {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace parent")
            .to_path_buf();
        let path = root.join(autonoetic_types::config::default_constitution_source_path());
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        crate::constitution_digest::clause_ids(&text)
    }

    /// Every clause this table names must exist in the active constitution.
    ///
    /// The orphan direction matters as much as the coverage direction: a
    /// relation for a clause that was never enacted, or was renumbered away,
    /// is a claim about law that does not exist. That is the `U-4` defect the
    /// diagram guard was written for, in a different artefact.
    #[test]
    fn every_declared_relation_names_a_real_clause() {
        let known: HashSet<String> =
            active_constitution_clause_ids().into_iter().collect();
        assert!(
            known.len() > 200,
            "clause extraction went blind: {} ids",
            known.len()
        );
        let orphans: Vec<&str> = declared_ids()
            .into_iter()
            .filter(|id| !known.contains(*id))
            .collect();
        assert!(
            orphans.is_empty(),
            "these clauses are classified but the active constitution ({}) does \
             not declare them: {orphans:?}",
            autonoetic_types::config::ACTIVE_CONSTITUTION_VERSION
        );
    }

    /// No clause is classified twice — a second entry would make `relation()`
    /// return whichever came first, silently.
    #[test]
    fn no_clause_is_classified_twice() {
        let ids = declared_ids();
        let mut seen = HashSet::new();
        for id in &ids {
            assert!(seen.insert(*id), "{id} is classified more than once");
        }
        assert_eq!(seen.len(), 73, "39 non-P clauses + P-8.1 + §5 (14) + §9 (16) + §15 (3)");
    }

    /// The four non-`P` families are **complete**. This is the coverage
    /// direction, scoped to what this tranche claims.
    #[test]
    fn the_non_p_families_are_fully_classified() {
        let missing: Vec<String> = active_constitution_clause_ids()
            .into_iter()
            .filter(|id| !id.starts_with("P-"))
            .filter(|id| relation(id).is_none())
            .collect();
        assert!(
            missing.is_empty(),
            "Ri-*/O-*/U-*/I-* are claimed complete, but these are unclassified: {missing:?}"
        );
    }

    /// The unclassified count is a **ratchet**, not a bound.
    ///
    /// Exact equality on purpose. `<=` would let a new clause arrive
    /// unclassified as long as another was classified in the same change,
    /// which is precisely the silent-drift this table exists to stop. An
    /// amendment that adds a clause must either classify it or lower nothing
    /// and update this number deliberately.
    #[test]
    fn the_unclassified_count_is_a_ratchet() {
        const UNCLASSIFIED: usize = 148;
        let outstanding = unclassified_among(&active_constitution_clause_ids());
        assert_eq!(
            outstanding.len(),
            UNCLASSIFIED,
            "unclassified clause count changed. Classifying a tranche? Lower the \
             constant. Adding a clause by amendment? Classify it, or raise the \
             constant deliberately and say why in the PR.\n  first 10: {:?}",
            outstanding.iter().take(10).collect::<Vec<_>>()
        );
        // Everything outstanding is a numbered `P-*`; the other families are
        // complete, and this keeps the two claims from drifting apart.
        assert!(
            outstanding.iter().all(|id| id.starts_with("P-")),
            "only numbered P-* should remain: {:?}",
            outstanding.iter().filter(|id| !id.starts_with("P-")).collect::<Vec<_>>()
        );
    }

    /// An inherited lookup says *what* the clause obliges without claiming to
    /// be the clause.
    ///
    /// `relation()` used to return the parent's whole `Relation`, id included,
    /// so `relation("P-15.1").id` was `"P-15"` — a value a caller could
    /// reasonably read as "the clause I asked about". The id cannot simply be
    /// rewritten to the queried one (`Relation.id` is `&'static str` and the
    /// query is not), so the field is gone from lookup results entirely and
    /// [`declared_at`] answers provenance for callers that need it.
    #[test]
    fn an_inherited_lookup_reports_its_source_separately() {
        // Nothing inherits today — no section grouping is law-side — so use a
        // classified clause to pin the exact case first.
        assert_eq!(declared_at("Ri-0.15"), Some("Ri-0.15"));
        assert_eq!(declared_at("I-8"), Some("I-8"));
        assert_eq!(declared_at("P-8.1"), Some("P-8.1"));

        // Unclassified stays unclassified in both accessors, rather than one
        // reporting a parent the other does not.
        assert!(relation("P-1.1").is_none());
        assert!(declared_at("P-1.1").is_none());

        // The two accessors never disagree about classification.
        for id in ["Ri-0.2", "U-1", "O-6", "I-14", "P-1.1", "nonsense"] {
            assert_eq!(
                relation(id).is_some(),
                declared_at(id).is_some(),
                "{id}: relation() and declared_at() disagree on classification"
            );
        }
    }

    /// Sub-clause inheritance resolves through the *declared parent*, and
    /// only there.
    #[test]
    fn sub_clauses_inherit_from_a_declared_parent_only() {
        // `Ri-0.15` is declared in its own right, and must not be shadowed by
        // a hypothetical `Ri-0` section entry.
        assert_eq!(relation("Ri-0.15").unwrap().owed_to, OwedTo::Seat(Binds::Decider));
        assert_eq!(relation("Ri-0.2").unwrap().owed_to, TO_AGENT);
        // An unclassified numbered clause with no classified parent stays
        // unclassified rather than borrowing a neighbour's relation.
        assert!(relation("P-1.1").is_none());
        assert!(relation("nonsense").is_none());
    }

    /// A classified section is classified **completely**, and its clauses no
    /// longer resolve by inheriting the enforcement register's section
    /// grouping.
    ///
    /// The second half is the point of the tranche. Before it,
    /// `binds("P-5.2")` answered from the register's `P-5` grouping via parent
    /// lookup — an inference that assumes every clause in a section binds what
    /// the section binds. A half-classified section is the worst state to be
    /// in: some clauses declared, the rest still quietly inheriting, with
    /// nothing marking which is which.
    #[test]
    fn a_classified_section_is_classified_completely_and_law_side() {
        let all = active_constitution_clause_ids();
        for sec in ["P-5.", "P-9.", "P-15."] {
            let clauses: Vec<&String> =
                all.iter().filter(|id| id.starts_with(sec)).collect();
            assert!(
                clauses.len() >= 3,
                "{sec}* extraction went blind: {} clauses",
                clauses.len()
            );
            for id in clauses {
                // Declared in its own right — `declared_at` returning the id
                // itself is what distinguishes a classification from an
                // inherited one.
                assert_eq!(
                    declared_at(id),
                    Some(id.as_str()),
                    "{id} is not classified in its own right; a section is \
                     classified completely or not at all"
                );
            }
        }
        // And the sections still awaiting a tranche are untouched, so this
        // test cannot pass by the table having swallowed everything.
        assert!(relation("P-2.20").is_none() || declared_at("P-2.20") != Some("P-2.20"));
    }

    fn active_constitution_clause_index() -> Vec<(String, String)> {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace parent")
            .to_path_buf();
        let path = root.join(autonoetic_types::config::default_constitution_source_path());
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        crate::constitution_digest::clause_index(&text)
    }

    /// Regenerate the committed law table.
    ///
    /// `BLESS_LAW_TABLE=1 cargo test -p autonoetic-gateway --lib bless_law_table`
    #[test]
    fn bless_law_table() {
        if std::env::var("BLESS_LAW_TABLE").is_err() {
            return;
        }
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../docs/constitution/law-table.md"
        );
        std::fs::write(path, render_law_table_markdown(&active_constitution_clause_index()))
            .expect("write law table");
    }

    /// The committed law table matches what the code would render.
    ///
    /// Same drift guard the enforcement register carries, for the same reason:
    /// a generated document that can silently disagree with its generator is
    /// worse than no document, because a reader trusts it.
    #[test]
    fn generated_law_table_matches_committed_doc() {
        let committed = include_str!("../../docs/constitution/law-table.md");
        assert_eq!(
            render_law_table_markdown(&active_constitution_clause_index()),
            committed,
            "generated law table differs from the committed doc; regenerate \
             docs/constitution/law-table.md (BLESS_LAW_TABLE=1)"
        );
    }

    /// The law table shows **every** clause — including the ones no
    /// implementation enforces.
    ///
    /// This is the gap §2.4.2 named. `U-1`–`U-3` are MISSING, so they can
    /// never appear in the enforcement register; before this document they
    /// appeared nowhere, which made the served party's charter invisible in
    /// exactly the corpus meant to make it legible.
    #[test]
    fn the_law_table_covers_clauses_no_code_enforces() {
        let rendered = render_law_table_markdown(&active_constitution_clause_index());
        for id in ["U-1", "U-2", "U-3", "I-8", "I-14", "Ri-0.15", "O-6", "P-5.3"] {
            assert!(
                rendered.contains(&format!("| `{id}` |")),
                "{id} is classified but does not appear in the law table"
            );
        }
        // And the unclassified are shown as such rather than omitted — a
        // reader must be able to tell "not yet decided" from "not a clause".
        assert!(
            rendered.contains("*unclassified.*"),
            "outstanding clauses must be listed and marked, not dropped"
        );
        assert!(rendered.contains("| `P-1.1` |"), "P-1.1 is unclassified but must be listed");
    }

    /// **Inference debt** — the number of `P-*` clauses whose bind direction
    /// is still answered by inheriting an enforcement-register *section*
    /// grouping rather than by a declaration. A ratchet, like the
    /// unclassified count.
    ///
    /// This is the metric the `P-*` tranches actually move, and it is not the
    /// same as coverage. A clause inheriting `P-5`'s relation already reports
    /// a bind direction, so classifying §5 does not raise the declared count
    /// by one — it converts an answer from *inferred* to *declared*. Counting
    /// only coverage would show this tranche as a no-op while it removes 33
    /// guesses.
    ///
    /// Reaches zero when §2 and §7 land, at which point the register's
    /// section groupings can be deleted and this test replaced by an assertion
    /// that no such inheritance exists at all.
    #[test]
    fn section_grouping_inference_is_a_ratchet() {
        const INHERITED_FROM_A_SECTION_GROUPING: usize = 51;

        let inherited: Vec<String> = active_constitution_clause_ids()
            .into_iter()
            .filter(|id| id.starts_with("P-"))
            // Not declared law-side …
            .filter(|id| declared_at(id).is_none())
            // … but the register resolves it, which can only be through a
            // section grouping, since every numbered register clause is
            // law-side (`the_register_and_the_law_table_never_disagree`).
            .filter(|id| crate::enforcement_register::binds(id).is_some())
            .collect();

        let mut sections: Vec<&str> = inherited
            .iter()
            .map(|id| id.split('.').next().unwrap_or(id))
            .collect();
        sections.sort_unstable();
        sections.dedup();

        assert_eq!(
            inherited.len(),
            INHERITED_FROM_A_SECTION_GROUPING,
            "section-grouping inference changed. Classified a section? Lower \
             the constant. It must never rise: a clause moving from declared \
             back to inherited is a regression.\n  still inferring: {sections:?}"
        );
        assert_eq!(
            sections,
            vec!["P-2", "P-7"],
            "only §2 and §7 should still infer; §5/§9/§15 are classified"
        );
    }

    /// `U-*` bind the enforcer and are owed to the served party — the fix for
    /// the §0/§12 contradiction the RFC records as defect 1.3, where §0 says
    /// a clause binds exactly one party while §12 spoke of "the community…
    /// collectively".
    #[test]
    fn served_party_rights_bind_the_enforcer_not_an_aggregate() {
        for id in ["U-1", "U-2", "U-3"] {
            let r = relation(id).expect("declared");
            assert_eq!(r.binds, Binds::Enforcer, "{id} must bind the enforcer");
            assert_eq!(r.owed_to, TO_SERVED, "{id} is owed to the served party");
        }
    }

    /// **§15 is owed to the served party**, not to the agent — the whole
    /// section, not just the `U-*` charter.
    ///
    /// Pinned by name because an earlier draft of the §15 tranche could be
    /// flipped to `owed_to: autonoetic_agent` without a single test failing.
    /// The claim is load-bearing twice over: it is the RFC's own migration
    /// entry for `P-15.*`, and it is the correction to what `binds()` used to
    /// report — `agent`, a party `I-14` forbids from setting, stripping or
    /// reading an egress label, so the agent could not comply even in
    /// principle.
    ///
    /// Data locality exists for the end user whose content carries the label.
    /// Recording it as owed to the agent would say the confinement is a
    /// guarantee *to the party being confined*.
    #[test]
    fn egress_localization_is_owed_to_the_served_party() {
        let clauses: Vec<String> = active_constitution_clause_ids()
            .into_iter()
            .filter(|id| id.starts_with("P-15."))
            .collect();
        assert_eq!(clauses.len(), 3, "expected the three §15 clauses");
        for id in &clauses {
            let r = relation(id).expect("§15 is classified");
            assert_eq!(r.binds, Binds::Enforcer, "{id} must bind the enforcer");
            assert_eq!(
                r.owed_to, TO_SERVED,
                "{id} is owed to the served party — the end user whose content \
                 carries the label, never the agent being confined by it"
            );
        }

        // I-14 is the plane-integrity substrate underneath, and is *not* a
        // served-party duty — the substrate/restatement distinction this
        // module draws for P-8.1 vs Ri-0.11, applied to egress.
        assert_eq!(relation("I-14").unwrap().owed_to, OwedTo::NoOne);
    }

    /// Invariants are classified per clause, not as a family — the `I-`
    /// prefix conflates universality with bind direction (RFC defect 1.4).
    #[test]
    fn invariants_are_not_uniform() {
        let inv: Vec<&Relation> = relations()
            .iter()
            .filter(|r| r.id.starts_with("I-"))
            .collect();
        let owed: HashSet<OwedTo> = inv.iter().map(|r| r.owed_to).collect();
        assert!(
            owed.len() > 1,
            "I-8/I-9 restate rights and carry the agent's standing, while the \
             rest are integrity properties — a uniform owed_to means that \
             distinction was flattened"
        );
        let floors: HashSet<VerifiedBy> = inv.iter().map(|r| r.verified_by).collect();
        assert!(
            floors.len() >= 4,
            "invariants span construction/chokepoint/registry/sampling/detection; \
             found only {floors:?}"
        );
    }

    /// A `Detection` floor on a behavioural universal is **correct**, and must
    /// not be "upgraded" — the variant order reads as a quality ladder, which
    /// makes raising these the tempting cleanup. For a duty to act within a
    /// deadline (`O-6`/`O-7`) or a universal negative over behaviour (`I-4`,
    /// `I-7`), no static check can succeed: raising the floor would demand the
    /// impossible and assert a proof nobody holds.
    #[test]
    fn behavioural_universals_keep_their_detection_floor() {
        for id in ["O-6", "O-7", "I-4", "I-6", "I-7"] {
            assert_eq!(
                relation(id).unwrap().verified_by,
                VerifiedBy::Detection,
                "{id} is a behavioural universal — recording each lapse is the \
                 enforceable form, and the strongest one available"
            );
        }
    }
}
