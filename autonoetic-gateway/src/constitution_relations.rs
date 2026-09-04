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
//! # Coverage: complete
//!
//! **All 221 clauses** the active constitution declares carry a declared
//! `binds`, `owed_to` and `requires`. Nothing is inferred from an ID prefix,
//! nothing inherits a section grouping, and nothing is outstanding — the
//! ratchets that tracked the gap are pinned at zero, and `requires` is
//! mandatory on the record so its coverage is a type property.
//!
//! # What the completed classification says
//!
//! **One clause in 221 binds the reasoner.** 215 bind the enforcer, 5 bind
//! the decider, and `P-2.9` alone binds the agent — "they must attach
//! `execution_trace_id` from a completed run".
//!
//! That is worth sitting with, because the old `binds()` reported *every*
//! `P-*` as binding the agent. The document is almost entirely a constraint
//! on the party with power, not on the party under governance: agents are
//! told what will be prevented, not what they must do. `docs/concepts/philosophy.md`
//! §2 argues bind direction is the structural novelty here; the measured
//! ratio is 215:1 in the direction that claim predicts, which is a stronger
//! result than the prose ever asserted.
//!
//! **38 clauses are agent rights by relation** — enforcer duties owed to the
//! agent. Only **17** of them carry an `Ri-` prefix; the other **21** are
//! filed under rule IDs. (`Ri-0.15` is the eighteenth right and is *not* in
//! this set: its `DecisionContext` is owed to the deciding seat, not the
//! agent.) So more than half of what an agent can actually invoke does not
//! look like a right, and §0's rights/rules ratio — computed from prefixes —
//! understates rights by better than a factor of two.
//!
//! Five further clauses are owed to the agent but bind the **decider**
//! (`O-1`, `O-2`, `O-6`, `O-7`, `P-2.21`). They are duties owed to the agent
//! without being agent rights, which is the distinction `is_agent_right`
//! draws by requiring `binds == Enforcer` — a right is a claim against the
//! party that holds power over you, not against a peer occupying a seat.
//!
//! **167 clauses are owed to no one** — integrity properties, the largest
//! group by far. That is the category the prefix scheme had no home for
//! (RFC defect 1.4(3)), and it turns out to be most of the document.
//!
use crate::enforcement_register::{Binds, OwedTo, Requires, VerifiedBy};
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
    /// What the clause requires of **any** implementation — the
    /// constitutional half of RFC #1283 §2.4.1.
    ///
    /// Not `Option`: every clause in this table declares it, and the
    /// constructors are the only way in. Coverage is therefore a **type
    /// property**, not something a ratchet test has to chase — adding a clause
    /// without a requirement does not compile. That is the same
    /// make-the-gap-unrepresentable move the model recommends for clauses,
    /// applied to the model's own data.
    pub requires: Requires,
    /// Minimum modality that would establish compliance (see module docs —
    /// a requirement, not a claim about this implementation).
    pub verified_by: VerifiedBy,
}

/// Shorthand: owed to the agent under governance.
const TO_AGENT: OwedTo = OwedTo::Principal(PrincipalKindTag::AutonoeticAgent);
/// Shorthand: owed to the end user a session ultimately serves.
const TO_SERVED: OwedTo = OwedTo::Principal(PrincipalKindTag::ServedUser);


/// An enforcer duty owed to the agent — an agent **right** in the
/// substantive sense (RFC §2.5), whatever prefix the clause id carries.
///
/// The `_r` suffix is vestigial: it distinguished this from a `requires`-less
/// variant that no longer exists, since `requires` became mandatory once every
/// clause declared it. Kept because renaming four constructors across 221 call
/// sites buys nothing.
const fn right_r(
    id: &'static str,
    requires: Requires,
    verified_by: VerifiedBy,
) -> Relation {
    Relation {
        id,
        binds: Binds::Enforcer,
        owed_to: TO_AGENT,
        requires,
        verified_by,
    }
}

// `duty` (the `requires`-less constructor) is gone: every `O-*` declares its
// `requires`, so nothing used it. Removing it rather than keeping it dead
// means a new decider obligation cannot be added without declaring the field —
// the constructor set enforces coverage for completed families. The three
// remaining `requires`-less constructors survive only because the `P-*`
// section tranches predate the field; each should go as its family completes.
/// A duty binding whoever occupies the **deciding seat** — occupant-agnostic,
/// so a `GateDecider`-holding agent is bound identically to a human operator.
const fn duty_r(
    id: &'static str,
    requires: Requires,
    verified_by: VerifiedBy,
) -> Relation {
    Relation {
        id,
        binds: Binds::Decider,
        owed_to: TO_AGENT,
        requires,
        verified_by,
    }
}


/// An enforcer duty with no invocable beneficiary — an integrity property.
/// See [`OwedTo::NoOne`] for why that is a positive claim rather than a gap.
const fn property_r(
    id: &'static str,
    requires: Requires,
    verified_by: VerifiedBy,
) -> Relation {
    Relation {
        id,
        binds: Binds::Enforcer,
        owed_to: OwedTo::NoOne,
        requires,
        verified_by,
    }
}


/// An enforcer duty owed to the **served party** — the end user a session
/// ultimately serves.
const fn served_r(
    id: &'static str,
    requires: Requires,
    verified_by: VerifiedBy,
) -> Relation {
    Relation {
        id,
        binds: Binds::Enforcer,
        owed_to: TO_SERVED,
        requires,
        verified_by,
    }
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
        right_r("Ri-0.1", Requires::Preventive, VerifiedBy::Test),
        right_r("Ri-0.2", Requires::Preventive, VerifiedBy::Test),
        // `Tagged::permission_with_rules` carries rule IDs, but nothing in
        // the type forbids an empty list, so an example test at the named
        // site is the honest floor. Making a ruleless rejection
        // unrepresentable would be a real strengthening.
        // requires: the RFC's worked mixed clause: an empty rule list is
        // excludable by construction; whether the named rule is the real basis
        // only review catches
        right_r("Ri-0.3", Requires::Both, VerifiedBy::Test),
        // requires: meters update structurally; "consumption is never silent" is a
        // behavioural claim about every path that spends
        right_r("Ri-0.4", Requires::Both, VerifiedBy::Test),
        // requires: the notice is injected *before* the next turn executes — a
        // turn that runs without it is the violation, and that is orderable
        right_r("Ri-0.5", Requires::Preventive, VerifiedBy::Test),
        // requires: narrowing requires recorded causal evidence (preventable);
        // "not silently" is the detective half
        right_r("Ri-0.6", Requires::Both, VerifiedBy::Test),
        // requires: "it may not refuse" — refusal is a representable state to exclude
        right_r("Ri-0.7", Requires::Preventive, VerifiedBy::Test),
        // requires: a durable ID is preventive; "cannot be silently dropped" is a
        // claim about the queue over time
        right_r("Ri-0.8", Requires::Both, VerifiedBy::Test),
        // requires: "where practical" makes compliance situational; only the
        // recorded flag and response show whether it was honoured
        right_r("Ri-0.9", Requires::Detective, VerifiedBy::Test),
        right_r("Ri-0.10", Requires::Preventive, VerifiedBy::Test),
        // Shares P-8.1's substrate: `compute_entry_hash` binds `actor_id`, so
        // reattribution is detectable by recomputation.
        // requires: the entry hash binds the actor (preventive); detecting an
        // attempted reattribution is the other half
        right_r("Ri-0.11", Requires::Both, VerifiedBy::Chokepoint),
        // `YieldReason` is a closed enum: an unlisted termination is a
        // compile error at every exhaustive match.
        // requires: a closed enum makes an unlisted termination a compile error
        right_r("Ri-0.12", Requires::Preventive, VerifiedBy::Construction),
        // Policy decision signatures do not take reasoning as a parameter,
        // so no call site can consult it — including ones not yet written.
        // requires: (a) is preventive at the signature; (c) capability-gated
        // disclosure writes an event — detective by design, since the point is
        // that the reviewed agent can see it happened
        right_r("Ri-0.13", Requires::Both, VerifiedBy::Construction),
        right_r("Ri-0.14", Requires::Preventive, VerifiedBy::Test),
        // Seat-standing, and `construction`: `DecisionContext` is a
        // *required* field on `human_gate.rs::GateRequest`, so a gate
        // without context cannot be built. (`GateService::check` rejecting
        // boilerplate is a chokepoint layered on top; the floor is the
        // structural guarantee underneath.)
        Relation {
            id: "Ri-0.15",
            binds: Binds::Enforcer,
            owed_to: OwedTo::Seat(Binds::Decider),
            requires: Requires::Preventive,
            verified_by: VerifiedBy::Construction,
        },
        // `is_advisory_only` is a runtime predicate, not a type, so the
        // "never raises a blocking gate" guarantee rests on tests.
        // requires: "never raises an execution-blocking gate" is an absolute the
        // sentinel's advisory-only path must make unreachable
        right_r("Ri-0.16", Requires::Preventive, VerifiedBy::Test),
        right_r("Ri-0.17", Requires::Preventive, VerifiedBy::Test),
        // requires: capability-free intake is preventive; "filing is never grounds
        // for sanction" is a claim about later conduct that only review reaches
        right_r("Ri-0.18", Requires::Both, VerifiedBy::Test),
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
        // requires: BLOCKING: the decision does not commit without a reason, so the
        // unmotivated decision is unrepresentable rather than merely logged
        duty_r("O-1", Requires::Preventive, VerifiedBy::Chokepoint),
        // "Cannot be reattributed" inherits Ri-0.11's hash binding.
        duty_r("O-2", Requires::Preventive, VerifiedBy::Chokepoint),
        // `Detection` is the *correct* floor, not a weak one: nothing static
        // can prove a decider will act within a deadline, so the enforceable
        // form is recording and counting the breach.
        // requires: nothing static can make a decider act within a window; the
        // recorded breach is the enforceable form
        duty_r("O-6", Requires::Detective, VerifiedBy::Detection),
        duty_r("O-7", Requires::Detective, VerifiedBy::Detection),
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
        // requires: a refusal the gateway can ignore is not a refusal — the
        // surface must make ignoring it impossible, not merely logged.
        served_r("U-1", Requires::Preventive, VerifiedBy::Chokepoint),
        served_r("U-2", Requires::Preventive, VerifiedBy::Test),
        // requires: export is preventive (it either produces the account or
        // fails); deletion is detective — you cannot prove absence, only
        // record and audit the shredding.
        served_r("U-3", Requires::Both, VerifiedBy::Test),
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
        property_r("I-1", Requires::Preventive, VerifiedBy::Chokepoint),
        // fsync-before-transition ordering, via P-8.16.
        property_r("I-2", Requires::Preventive, VerifiedBy::Chokepoint),
        // Status is PARTIAL while the floor is `Construction`: the clause's
        // own text names what closing it requires — `RedactedPayload` at the
        // store write API, where the compiler covers paths that do not exist
        // yet. The floor states the requirement; it does not claim we meet
        // it.
        // requires: the clause names its own requirement: `RedactedPayload` at the
        // write API, "where the compiler covers paths that do not exist yet"
        property_r("I-3", Requires::Preventive, VerifiedBy::Construction),
        // A universal negative over behaviour. No static check succeeds, so
        // the enforceable form is counting each lapse as a durable
        // `discretion_leak` event.
        // requires: a universal negative over behaviour. Prevention is unavailable,
        // so this is the *correct* requirement, not a concession — the case
        // that broke the floor model
        property_r("I-4", Requires::Detective, VerifiedBy::Detection),
        // Needs static analysis over the source with a documented allowlist,
        // in the shape of the existing docs guards — a set comparison.
        // requires: "hard-coded constants are discouraged and must be documented" —
        // a standing audit, not an exclusion
        property_r("I-5", Requires::Detective, VerifiedBy::Registry),
        // requires: "a decision without a rule reference is a gap by construction" —
        // the clause asks for construction in its own words
        property_r("I-6", Requires::Preventive, VerifiedBy::Detection),
        // A meta-rule about amendment. The mechanically enforceable residue
        // is that a conflict *escalates* rather than resolving silently,
        // which is observable only when it happens.
        // requires: a meta-rule about amendment; the mechanically enforceable residue
        // is that a conflict escalates rather than resolving silently, which is
        // observable only when it happens
        property_r("I-7", Requires::Detective, VerifiedBy::Detection),
        // The mechanical form of Ri-0.13(a) — same duty, universal form, so
        // the agent's standing carries over.
        Relation { id: "I-8", binds: Binds::Enforcer, owed_to: TO_AGENT, requires: Requires::Preventive, verified_by: VerifiedBy::Construction },
        // The mechanical form of Ri-0.12.
        Relation { id: "I-9", binds: Binds::Enforcer, owed_to: TO_AGENT, requires: Requires::Preventive, verified_by: VerifiedBy::Construction },
        // Property-based over generated `(capabilities, tool-call, state)`
        // inputs: cannot prove determinism, can sample it.
        // requires: determinism over declared inputs is a property of the decision
        // surfaces, not an event to count — sampling is how we *check* it, which
        // is `achieved`, not what the clause requires
        property_r("I-10", Requires::Preventive, VerifiedBy::Sampling),
        // "Every invariant has a declared failure action" is a set
        // comparison against `fail_mode.rs`.
        // requires: "no invariant silently disables" — a registry completeness check
        // makes the missing row impossible to ship
        property_r("I-11", Requires::Preventive, VerifiedBy::Registry),
        // DESIGN DEBT: declared before any collective mechanism exists,
        // specifically so Sybil resistance cannot be an oversight in a first
        // design. The floor says what that design must provide — weight
        // collapse structural, not checked after the fact.
        // requires: declared before any mechanism exists precisely so weight collapse
        // is structural in the first design rather than audited afterwards
        property_r("I-12", Requires::Preventive, VerifiedBy::Construction),
        // Documents a deliberate *absence* (no capability-attenuation check).
        // What verifies an absence is a test asserting it stays absent.
        // requires: documents a deliberate absence; what it requires is that no
        // attenuation check appear
        property_r("I-13", Requires::Preventive, VerifiedBy::Test),
        // The egress instance of I-8/I-10. An integrity property rather than
        // a served-party duty: P-15 is the duty owed to the served party,
        // I-14 is the plane-integrity substrate that makes it holdable —
        // the same substrate/restatement split as P-8.1 vs Ri-0.11.
        // requires: "no agent may set, strip, or read" — an absolute over the label
        // plane
        property_r("I-14", Requires::Preventive, VerifiedBy::Chokepoint),
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
        // requires: append-only linkage is preventive; tamper-*evidence* by
        // recomputation is detective, and the clause states both limbs
        property_r("P-8.1", Requires::Both, VerifiedBy::Chokepoint),
    ];
    out.extend(section_1());
    out.extend(section_2());
    out.extend(section_3());
    out.extend(section_4());
    out.extend(section_5());
    out.extend(section_6());
    out.extend(section_7());
    out.extend(section_8());
    out.extend(section_9());
    out.extend(section_10());
    out.extend(section_11());
    out.extend(section_15());
    out
}

/// A duty owed to whoever occupies the **deciding seat** — kind-agnostic, so
/// a `GateDecider`-holding agent has the same standing as a human operator.
const fn to_decider_r(
    id: &'static str,
    requires: Requires,
    verified_by: VerifiedBy,
) -> Relation {
    Relation {
        id,
        binds: Binds::Enforcer,
        owed_to: OwedTo::Seat(Binds::Decider),
        requires,
        verified_by,
    }
}

/// §1 — Capability & Rights.
///
/// Capability enforcement, and enforcer throughout: every clause is a gate
/// the gateway operates. `P-1.10` is the single agent-facing one — "never
/// advisory" means a denial arrives as a real error rather than a suggestion,
/// which is the Ri-0.3 family.
fn section_1() -> Vec<Relation> {
    vec![
        // requires: "no overrides" is an absolute on the gateway: `can_invoke_tool` is the
        // single gate and there is no bypass parameter
        property_r("P-1.1", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-1.2", Requires::Preventive, VerifiedBy::Test),
        property_r("P-1.3", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-1.4", Requires::Preventive, VerifiedBy::Test),
        // requires: "the gateway owns the detected-host contract" names its own bound
        // party — the agent supplies no hosts
        property_r("P-1.5", Requires::Preventive, VerifiedBy::Test),
        property_r("P-1.6", Requires::Preventive, VerifiedBy::Test),
        property_r("P-1.7", Requires::Preventive, VerifiedBy::Test),
        property_r("P-1.8", Requires::Preventive, VerifiedBy::Test),
        property_r("P-1.9", Requires::Preventive, VerifiedBy::Test),
        // requires: owed to the agent: "never advisory" means a denial arrives as a real
        // error rather than a suggestion the agent might reasonably ignore — the
        // Ri-0.3 family
        right_r("P-1.10", Requires::Preventive, VerifiedBy::Test),
        property_r("P-1.11", Requires::Preventive, VerifiedBy::Test),
    ]
}

/// §3 — Sandbox Isolation.
///
/// Integrity properties almost without exception, and the section that supplies
/// the model's canonical `owed_to: none` case: `P-3.1`'s `--unshare-all`
/// benefits the operator, but an agent cannot *claim* its own confinement and
/// would prefer not to have it.
///
/// `P-3.5` is the exception on both axes — owed to the agent (it learns the
/// call failed rather than hanging) and `detective`, since classifying a
/// driver's network errors is pattern-matching, so a novel shape is a miss.
fn section_3() -> Vec<Relation> {
    vec![
        // requires: the RFC's canonical `owed_to: none` example: an agent cannot demand
        // its own confinement and would prefer not to have it
        property_r("P-3.1", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-3.2", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-3.3", Requires::Preventive, VerifiedBy::Test),
        property_r("P-3.4", Requires::Preventive, VerifiedBy::Test),
        // requires: owed to the agent — it learns the call failed instead of hanging.
        // `Detective` because error classification is pattern-matching over a
        // driver's error surface: a novel error shape is a miss, not an
        // excluded state
        right_r("P-3.5", Requires::Detective, VerifiedBy::Detection),
        property_r("P-3.6", Requires::Preventive, VerifiedBy::Test),
        // requires: preventive on the gateway's own half — it refuses to start an exec
        // without declarations. The clause is candid that quota *enforcement* is
        // externalized to operator driver profiles (only wasm has a built-in
        // limiter), which is a conformance fact, not a weaker requirement
        property_r("P-3.7", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-3.8", Requires::Preventive, VerifiedBy::Test),
        property_r("P-3.9", Requires::Preventive, VerifiedBy::Test),
        // requires: "regardless of the candidate's declared NetworkAccess" — the gate
        // overrides a capability, so the deny cannot be reached around
        property_r("P-3.10", Requires::Preventive, VerifiedBy::Chokepoint),
    ]
}

/// §4 — Credential & Secret Protection.
///
/// Enforcer, integrity properties, overwhelmingly preventive: the section exists
/// to make secret exposure unrepresentable rather than auditable.
///
/// Two clauses are honest about the limit of that. `P-4.12` blocks
/// secret-shaped text by *pattern*, so coverage is audited; `P-4.15` pairs a
/// refusal to start with a causal event recording the probe.
fn section_4() -> Vec<Relation> {
    vec![
        // requires: "secrets never enter agent context" — the injection boundary is where
        // the value first exists, so the agent-visible path has nothing to leak
        property_r("P-4.1", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-4.2", Requires::Preventive, VerifiedBy::Test),
        property_r("P-4.3", Requires::Preventive, VerifiedBy::Test),
        // requires: a `cred_*` id *is* a reference; secret material is a different type
        property_r("P-4.4", Requires::Preventive, VerifiedBy::Construction),
        property_r("P-4.5", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-4.6", Requires::Preventive, VerifiedBy::Test),
        property_r("P-4.7", Requires::Preventive, VerifiedBy::Test),
        property_r("P-4.8", Requires::Preventive, VerifiedBy::Test),
        property_r("P-4.9", Requires::Preventive, VerifiedBy::Test),
        property_r("P-4.10", Requires::Preventive, VerifiedBy::Test),
        // requires: I-4's one named exception — a recovery decision the gateway does make.
        // Bounded to at most once per request, which is why it is a preventive
        // requirement and not an open licence
        property_r("P-4.11", Requires::Preventive, VerifiedBy::Test),
        // requires: blocking is preventive; `prohibited_text_patterns` is a pattern set, so
        // its coverage is audited rather than complete — a novel secret shape is
        // a miss
        property_r("P-4.12", Requires::Both, VerifiedBy::Test),
        property_r("P-4.13", Requires::Preventive, VerifiedBy::Chokepoint),
        // requires: the ordering invariant I-3 states universally
        property_r("P-4.14", Requires::Preventive, VerifiedBy::Chokepoint),
        // requires: refusing to start is preventive; the causal event recording the probe
        // result is the detective half
        property_r("P-4.15", Requires::Both, VerifiedBy::Test),
    ]
}

/// §6 — Session, Workflow & Budget.
///
/// Budget and lifecycle machinery. Four clauses are owed to the agent, and two of
/// them are right/mechanism pairs the RFC names (§1.4(2)): `P-6.21` ties the
/// tree-budget circuit breaker to Ri-0.12 reason (b) rather than an operator
/// stop, and `P-6.23`'s signed state block is what makes Ri-0.1's "inspect your
/// own state" true at every turn.
fn section_6() -> Vec<Relation> {
    vec![
        property_r("P-6.1", Requires::Preventive, VerifiedBy::Test),
        property_r("P-6.2", Requires::Preventive, VerifiedBy::Chokepoint),
        // requires: "all calls in a batch reserve together" — partial reservation is the
        // state being excluded
        property_r("P-6.3", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-6.4", Requires::Preventive, VerifiedBy::Test),
        // requires: "no silent-disable": an unavailable catalog with an active cap refuses
        // the completion rather than proceeding uncapped
        property_r("P-6.5", Requires::Preventive, VerifiedBy::Test),
        property_r("P-6.6", Requires::Preventive, VerifiedBy::Test),
        // requires: logging *is* the requirement
        property_r("P-6.7", Requires::Detective, VerifiedBy::Detection),
        property_r("P-6.8", Requires::Preventive, VerifiedBy::Test),
        property_r("P-6.9", Requires::Preventive, VerifiedBy::Test),
        property_r("P-6.10", Requires::Preventive, VerifiedBy::Test),
        property_r("P-6.11", Requires::Preventive, VerifiedBy::Test),
        property_r("P-6.12", Requires::Preventive, VerifiedBy::Test),
        // requires: "cover every yield reason" is a set comparison against the closed
        // `YieldReason` enum — the registry shape
        property_r("P-6.13", Requires::Preventive, VerifiedBy::Registry),
        property_r("P-6.14", Requires::Preventive, VerifiedBy::Test),
        // requires: owed to the agent: "atomically replays the pending tool call" is what
        // spares it a synthetic retry prompt in place of real results
        right_r("P-6.15", Requires::Preventive, VerifiedBy::Test),
        property_r("P-6.16", Requires::Preventive, VerifiedBy::Test),
        property_r("P-6.17", Requires::Preventive, VerifiedBy::Test),
        property_r("P-6.18", Requires::Preventive, VerifiedBy::Test),
        right_r("P-6.19", Requires::Preventive, VerifiedBy::Test),
        property_r("P-6.20", Requires::Preventive, VerifiedBy::Test),
        // requires: one of the six right/mechanism pairs (RFC §1.4(2)). The clause itself
        // ties the circuit breaker to Ri-0.12 reason (b) rather than an operator
        // stop, which is the guarantee the agent holds
        right_r("P-6.21", Requires::Preventive, VerifiedBy::Test),
        property_r("P-6.22", Requires::Preventive, VerifiedBy::Test),
        // requires: Ri-0.1's mechanism, and the second of the six pairs: the signed state
        // block is what makes "inspect your own state" true at every turn
        right_r("P-6.23", Requires::Preventive, VerifiedBy::Test),
        property_r("P-6.24", Requires::Preventive, VerifiedBy::Test),
        property_r("P-6.25", Requires::Preventive, VerifiedBy::Test),
        // requires: `side_effect_state` is a closed enum, so an unclassified side effect is
        // a compile error rather than an unhandled case
        property_r("P-6.26", Requires::Preventive, VerifiedBy::Construction),
    ]
}

/// §8 — Audit & Traceability.
///
/// The section where `detective` is most at home, and correctly so: "is logged",
/// "is detectable", "emits an event" are satisfied *by* recording — the log is
/// the compliance, not evidence of it.
///
/// Five clauses are `Both`, each pairing a structural guarantee with a
/// recording duty: a unique id plus universal logging (`P-8.2`), a refusal plus
/// a drift event (`P-8.12`), tracking plus cleanup audits (`P-8.15`), a
/// validation error for privileged classes plus verbatim persistence
/// (`P-8.18`). `P-8.19` is O-2's gateway-side mechanism.
fn section_8() -> Vec<Relation> {
    vec![
        // requires: a unique `event_id` is preventive; "every event is logged" is a
        // universal over emissions and is satisfied by recording
        property_r("P-8.2", Requires::Both, VerifiedBy::Detection),
        property_r("P-8.3", Requires::Preventive, VerifiedBy::Test),
        property_r("P-8.4", Requires::Preventive, VerifiedBy::Test),
        // requires: "untruncated" is the load-bearing word — truncation is the excluded
        // state
        property_r("P-8.5", Requires::Preventive, VerifiedBy::Test),
        property_r("P-8.6", Requires::Preventive, VerifiedBy::Test),
        property_r("P-8.7", Requires::Preventive, VerifiedBy::Test),
        property_r("P-8.8", Requires::Preventive, VerifiedBy::Test),
        property_r("P-8.9", Requires::Preventive, VerifiedBy::Test),
        // requires: "is detectable via promotion_history" — the clause asks for
        // discoverability, not exclusion
        property_r("P-8.10", Requires::Detective, VerifiedBy::Detection),
        property_r("P-8.11", Requires::Preventive, VerifiedBy::Test),
        // requires: refusing to start is preventive; the `runtime_lock_drift` event is the
        // detective half, and the operator override is why both are needed
        property_r("P-8.12", Requires::Both, VerifiedBy::Test),
        property_r("P-8.13", Requires::Detective, VerifiedBy::Detection),
        property_r("P-8.14", Requires::Preventive, VerifiedBy::Test),
        // requires: tracking by `(root_session_id, host)` is preventive; inclusion in
        // cleanup audits is the recording half
        property_r("P-8.15", Requires::Both, VerifiedBy::Test),
        // requires: the ordering I-2 states universally
        property_r("P-8.16", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-8.17", Requires::Detective, VerifiedBy::Detection),
        // requires: optional in general, but "for privileged tool classes, missing intent
        // is a validation error" is preventive, and "persists the intent verbatim"
        // is detective
        property_r("P-8.18", Requires::Both, VerifiedBy::Test),
        // requires: O-2's gateway-side mechanism: the agent under decision can always tell
        // who decided
        right_r("P-8.19", Requires::Preventive, VerifiedBy::Test),
    ]
}

/// §10 — Federation / Remote.
///
/// Enforcer, integrity, preventive throughout. The load-bearing word is in
/// `P-10.4` — remote agents "inherit *all* approval gates", so remoteness is
/// not a bypass — and `P-10.7`'s spawn-tree collapse is what `I-12` extends to
/// any future decision weight.
fn section_10() -> Vec<Relation> {
    vec![
        property_r("P-10.1", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-10.2", Requires::Preventive, VerifiedBy::Test),
        property_r("P-10.3", Requires::Preventive, VerifiedBy::Chokepoint),
        // requires: "inherit *all* approval gates" — remoteness is not a bypass
        property_r("P-10.4", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-10.5", Requires::Preventive, VerifiedBy::Test),
        property_r("P-10.6", Requires::Preventive, VerifiedBy::Test),
        // requires: the spawn-tree collapse I-12 extends to any future decision weight
        property_r("P-10.7", Requires::Preventive, VerifiedBy::Chokepoint),
        // requires: constant-time comparison excludes the timing channel rather than
        // detecting its use
        property_r("P-10.8", Requires::Preventive, VerifiedBy::Test),
        property_r("P-10.9", Requires::Preventive, VerifiedBy::Test),
    ]
}

/// §11 — Inter-Agent Messaging.
///
/// Half the section is owed to the agent, which is unusual and follows from what
/// messaging is: delivery, payload preservation and consent are guarantees to
/// the parties messaging. `P-11.5` is owed specifically to the *receiver* —
/// consent is receiver-declared because evaluators hold no adjudicating
/// capability.
fn section_11() -> Vec<Relation> {
    vec![
        property_r("P-11.1", Requires::Preventive, VerifiedBy::Chokepoint),
        right_r("P-11.2", Requires::Preventive, VerifiedBy::Test),
        property_r("P-11.3", Requires::Preventive, VerifiedBy::Test),
        right_r("P-11.4", Requires::Preventive, VerifiedBy::Test),
        // requires: owed to the agent, and specifically to the *receiver*: consent is
        // receiver-declared because evaluators hold no adjudicating capability
        right_r("P-11.5", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-11.6", Requires::Preventive, VerifiedBy::Test),
        property_r("P-11.7", Requires::Preventive, VerifiedBy::Test),
        right_r("P-11.8", Requires::Preventive, VerifiedBy::Test),
    ]
}

/// §2 — Approval Gates. The section that finally exercises the whole model:
/// it is the first to contain a `reasoner` clause, the first to put a
/// **decider** obligation under a `P-` prefix, and the first where duties run
/// to the **deciding seat** rather than to the agent or to no one.
///
/// Three findings, each a shape the prefix scheme could not express:
///
/// - **`P-2.9` binds the reasoner** — "they *must attach* `execution_trace_id`
///   from a completed run". The obligation falls on the recording agent; the
///   gateway's part is refusing to take its word (`pass` is derived from
///   `exit_code`, not set by the caller). Every clause classified before this
///   tranche bound the enforcer, which made `reasoner` look vestigial. It is
///   not — §2 is simply the first section where agents are told to do
///   something rather than prevented from doing it.
/// - **`P-2.21` binds the decider** — "it *must escalate* to a human operator
///   rather than reject". An agent-decider that cannot decide owes escalation.
///   That is an `O-*` obligation wearing a `P-` prefix, which is RFC defect
///   1.4(2) in the flesh, and it binds the *seat*: a human operator in the
///   same position owes the same thing.
/// - **Four clauses are owed to the `decider` seat** — `P-2.5`
///   (`detected_hosts` surfaced for operator visibility), `P-2.16` (the
///   capability delta names each added capability explicitly), `P-2.24`
///   (dwell time and typed confirmation on high-risk gates), `P-2.27` (the
///   envelope is locked by operator decision). Each is information or
///   protection the decider needs in order to decide, which is exactly
///   `Ri-0.15`'s relation — and before `OwedTo::Seat` existed there was
///   nowhere to record it.
fn section_2() -> Vec<Relation> {
    vec![
        // `GateService` is the unified gate chokepoint; "blocks pending
        // approval rather than hard-denying" is machinery, not a claim any
        // party can invoke.
        property_r("P-2.1", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-2.2", Requires::Preventive, VerifiedBy::Test),
        // "The `GateService` centralizes dedup … Tools do not implement their
        // own dedup logic" — N paths reduced to one, stated in the clause.
        property_r("P-2.3", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-2.4", Requires::Preventive, VerifiedBy::Test),
        // Surfaced *for operator visibility* — the decider cannot decide well
        // without it.
        to_decider_r("P-2.5", Requires::Preventive, VerifiedBy::Test),
        property_r("P-2.6", Requires::Preventive, VerifiedBy::Test),
        property_r("P-2.7", Requires::Preventive, VerifiedBy::Test),
        property_r("P-2.8", Requires::Preventive, VerifiedBy::Chokepoint),
        // **Reasoner.** "They must attach `execution_trace_id` from a
        // completed run" obliges the recording agent; the gateway's half is
        // declining to trust the claim.
        Relation {
            id: "P-2.9",
            binds: Binds::Reasoner,
            owed_to: OwedTo::NoOne,
            requires: Requires::Preventive,
            verified_by: VerifiedBy::Chokepoint,
        },
        // Owed to the agent: a gate-suspended turn resumes with real tool
        // results and an auto-injected `approval_ref` rather than a synthetic
        // retry prompt. The agent is the party that would otherwise be handed
        // a fabricated history.
        right_r("P-2.10", Requires::Preventive, VerifiedBy::Test),
        property_r("P-2.11", Requires::Preventive, VerifiedBy::Test),
        // The gateway-side mechanism of O-2: `decided_by` is persisted so the
        // agent under decision can tell who decided.
        right_r("P-2.12", Requires::Preventive, VerifiedBy::Test),
        property_r("P-2.13", Requires::Preventive, VerifiedBy::Test),
        property_r("P-2.14", Requires::Preventive, VerifiedBy::Test),
        // "Spawn payload is preserved verbatim" — owed to the agent whose
        // payload it is.
        right_r("P-2.15", Requires::Preventive, VerifiedBy::Test),
        // The delta approval "names each added capability explicitly" so the
        // operator sees what they are granting.
        to_decider_r("P-2.16", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-2.17", Requires::Preventive, VerifiedBy::Chokepoint),
        // "All execution suspension points … use the unified `GateService`."
        property_r("P-2.18", Requires::Preventive, VerifiedBy::Chokepoint),
        // requires: append-only is preventive; "recorded on the causal
        // chain" is the detective half
        property_r("P-2.19", Requires::Both, VerifiedBy::Test),
        property_r("P-2.20", Requires::Preventive, VerifiedBy::Chokepoint),
        // **Decider**, and owed to the agent. "It must escalate to a human
        // operator rather than reject" is a duty on whoever holds the gate,
        // owed to the agent whose gate is pending — an `O-*` obligation under
        // a `P-` prefix.
        Relation {
            id: "P-2.21",
            binds: Binds::Decider,
            owed_to: TO_AGENT,
            requires: Requires::Detective,
            verified_by: VerifiedBy::Test,
        },
        property_r("P-2.22", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-2.23", Requires::Preventive, VerifiedBy::Test),
        // Dwell time and typed confirmation protect the *decider* from their
        // own mis-click. Nobody else can invoke it.
        to_decider_r("P-2.24", Requires::Preventive, VerifiedBy::Test),
        // Fail-closed, "determined mechanically by the gateway … never
        // inferred from orchestrator-supplied claims" — the §2 instance of
        // I-8.
        property_r("P-2.25", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-2.26", Requires::Preventive, VerifiedBy::Chokepoint),
        // The envelope is "locked by operator decision" — its scope is the
        // decider's instrument.
        to_decider_r("P-2.27", Requires::Preventive, VerifiedBy::Test),
        property_r("P-2.28", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-2.29", Requires::Preventive, VerifiedBy::Chokepoint),
    ]
}

/// §7 — Abuse, hard stops, circuit breakers. Enforcer throughout and almost
/// entirely integrity properties: these are the limits that hold *against* the
/// agent, and an agent cannot demand its own halting.
///
/// `P-7.18` is the exception, and it is one of the six right/mechanism pairs
/// the RFC identifies (§1.4(2)): degraded mode "loses non-Core tools, network
/// access, and spawn capability **but retains reasoning**" — a guarantee to
/// the agent, and the mechanism `Ri-0.5` is the entitlement for.
///
/// `P-7.22` is the section's only `Detection` floor, and correctly so:
/// sandbox-escape attempts are *counted per session*, which is what you do
/// with a behaviour no static check can rule out.
fn section_7() -> Vec<Relation> {
    vec![
        property_r("P-7.1", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.2", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.3", Requires::Preventive, VerifiedBy::Test),
        // requires: recording *is* the requirement
        property_r("P-7.4", Requires::Detective, VerifiedBy::Test),
        property_r("P-7.5", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.6", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.7", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.8", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.9", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.10", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.11", Requires::Preventive, VerifiedBy::Test),
        // "No escape hatch; passes require real evaluator + auditor records."
        property_r("P-7.12", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-7.13", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-7.14", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.15", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.16", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.17", Requires::Preventive, VerifiedBy::Test),
        // Owed to the agent: degraded mode is bounded *for the agent's
        // benefit* — it retains reasoning rather than being stopped. Ri-0.5
        // is the entitlement; this is the mechanism.
        right_r("P-7.18", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.19", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.20", Requires::Preventive, VerifiedBy::Test),
        property_r("P-7.21", Requires::Preventive, VerifiedBy::Test),
        // Counted per session — the enforceable form for an attempt you
        // cannot statically preclude.
        // requires: counting attempts is detective; the threshold action
        // is preventive
        property_r("P-7.22", Requires::Both, VerifiedBy::Detection),
    ]
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
        property_r("P-5.1", Requires::Preventive, VerifiedBy::Chokepoint),
        // `Construction`: the LLM-coercion fallback was *removed*, so
        // "coercion is deterministic only" holds because the non-deterministic
        // path is not in `SchemaEnforcementMode` to select.
        property_r("P-5.2", Requires::Preventive, VerifiedBy::Construction),
        // Owed to the agent — a failure that does not say what to do next is
        // the Ri-0.3 defect wearing a schema error's clothes.
        // requires: a hint's *presence* is excludable by construction;
        // whether it is "actionable" only review reaches — the Ri-0.3
        // shape
        right_r("P-5.3", Requires::Both, VerifiedBy::Test),
        // Logging every pass/coerce/reject is a universal over decisions, so
        // recording is the enforceable form. The *duty to log* is an integrity
        // property; the agent's right to read what was logged is Ri-0.2, and
        // conflating them would count one relationship twice.
        // requires: satisfied *by* recording — the log is the compliance,
        // not evidence of it
        property_r("P-5.4", Requires::Detective, VerifiedBy::Detection),
        property_r("P-5.5", Requires::Preventive, VerifiedBy::Test),
        // "Authoritative runtime state, not LLM claims" is the §5 instance of
        // I-8: the verdict is a function of recorded state, never of model
        // output.
        property_r("P-5.6", Requires::Preventive, VerifiedBy::Test),
        property_r("P-5.7", Requires::Preventive, VerifiedBy::Test),
        // Owed to the agent, and the reason is the DISCRETION LEAK marker on
        // the clause itself: repair means the gateway rewriting the agent's
        // output. "Strictly opt-in, defaults false, attempts clamped" is a
        // limit on the gateway held *for* the agent.
        right_r("P-5.8", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-5.9", Requires::Preventive, VerifiedBy::Test),
        property_r("P-5.10", Requires::Preventive, VerifiedBy::Test),
        // Owed to the agent: the agent is the consumer of these errors, and a
        // uniform envelope is what makes a failure machine-actionable rather
        // than a string to guess at.
        right_r("P-5.11", Requires::Preventive, VerifiedBy::Test),
        property_r("P-5.12", Requires::Preventive, VerifiedBy::Test),
        property_r("P-5.13", Requires::Preventive, VerifiedBy::Chokepoint),
        // `Construction`: `FailureClass` is a closed enum and classification
        // is a pure function of gateway-observed state.
        property_r("P-5.14", Requires::Preventive, VerifiedBy::Construction),
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
        property_r("P-9.1", Requires::Preventive, VerifiedBy::Chokepoint),
        // `Construction`: "not a runtime tool" is enforced by the tool being
        // absent from the registry, so there is no call site to gate.
        property_r("P-9.2", Requires::Preventive, VerifiedBy::Construction),
        // Content-addressing *is* immutability: a changed revision is a
        // different address.
        property_r("P-9.3", Requires::Preventive, VerifiedBy::Construction),
        property_r("P-9.4", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-9.5", Requires::Preventive, VerifiedBy::Test),
        // requires: statuses bounding promotion is preventive; the
        // promotion-attempt ledger is the recording half
        property_r("P-9.6", Requires::Both, VerifiedBy::Test),
        property_r("P-9.7", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-9.8", Requires::Preventive, VerifiedBy::Test),
        property_r("P-9.9", Requires::Preventive, VerifiedBy::Chokepoint),
        // requires: a static scan over a language's import surface: a
        // missed import is a miss, not an excluded state, so completeness
        // is audited rather than guaranteed
        property_r("P-9.10", Requires::Detective, VerifiedBy::Test),
        property_r("P-9.11", Requires::Preventive, VerifiedBy::Chokepoint),
        right_r("P-9.12", Requires::Preventive, VerifiedBy::Test),
        property_r("P-9.13", Requires::Preventive, VerifiedBy::Test),
        // DESIGN DEBT — trust domains do not constrain cross-domain spawns
        // yet. The floor states what closing it requires, not what exists.
        property_r("P-9.14", Requires::Preventive, VerifiedBy::Chokepoint),
        // The single door: N activation surfaces reduced to one gate matrix,
        // with the startup bootstrap exception made parameter-explicit
        // (`auto_promote: bool`) rather than implicit — a guarded bypass,
        // which is what separates a chokepoint from a convention.
        property_r("P-9.15", Requires::Preventive, VerifiedBy::Chokepoint),
        property_r("P-9.16", Requires::Preventive, VerifiedBy::Test),
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
        served_r("P-15.1", Requires::Preventive, VerifiedBy::Chokepoint),
        // requires: gating on taint before send is preventive; the
        // `egress.boundary_refused` event is the recording half
        served_r("P-15.2", Requires::Both, VerifiedBy::Chokepoint),
        // requires: "widens *only* via" is preventive; "causal-logged" is
        // detective
        served_r("P-15.3", Requires::Both, VerifiedBy::Chokepoint),
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
    /// See [`Relation::requires`].
    pub requires: Requires,
    pub verified_by: VerifiedBy,
}

impl Relation {
    fn fields(self) -> Fields {
        Fields {
            binds: self.binds,
            owed_to: self.owed_to,
            requires: self.requires,
            verified_by: self.verified_by,
        }
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
/// What `clause_id` requires of any implementation, or `None` while the
/// clause awaits its `requires` tranche.
///
/// Distinct from [`relation`]`(..).verified_by`, which records what *this*
/// gateway achieves. RFC #1283 §2.4.1: the requirement is law, the mechanism
/// is conformance.
pub fn requires(clause_id: &str) -> Option<Requires> {
    relation(clause_id).map(|f| f.requires)
}

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

    out.push_str(
        "Two verification columns, and they answer different questions (RFC #1283 §2.4.1):\n\n\
         - **`requires`** is **law** — what *any* implementation must provide. `preventive` \
         means non-compliance has to be made impossible; `detective` means each occurrence \
         must be recorded. `detective` is not a weaker `preventive`: for a universal negative \
         over behaviour (`I-4`) prevention is unavailable, so recording is the *correct* \
         requirement.\n\
         - **`verified_by`** is **conformance** — the mechanism *this* gateway uses. It is the \
         field §2.4.1 renames to `achieved` and moves to a per-implementation register; until \
         that move it is still shown here, and for an enforced clause it describes our Rust \
         rather than stating a requirement. Read it as provisional.\n\n",
    );

    let total = clause_index.len();
    let classified = clause_index
        .iter()
        .filter(|(id, _)| relation(id).is_some())
        .count();
    out.push_str("## Coverage\n\n");
    if classified == total {
        out.push_str(&format!(
            "**All {total} clauses classified.** Every clause the active constitution declares \
             carries a declared `binds`, `owed_to` and `requires` — none is inferred from an ID \
             prefix, and none is outstanding.\n\n",
        ));
    } else {
        out.push_str(&format!(
            "**{classified} of {total}** clauses classified. The remainder are numbered `P-*` \
             awaiting their section tranche; they are counted, not hidden — a ratchet test pins \
             the exact number so a new clause cannot arrive unclassified.\n\n",
        ));
    }

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
    // Derived, not written: the interesting members of this list are whichever
    // clauses carry a non-`Ri-` prefix, and hardcoding examples went stale the
    // first time a tranche added one.
    let unprefixed: Vec<&str> = agent_rights
        .iter()
        .copied()
        .filter(|id| !id.starts_with("Ri-"))
        .collect();
    out.push_str(&format!(
        "**Agent rights by relation** ({}): `{}`\n\nA right is a *view*, not a family: an \
         enforcer duty owed to the agent is an agent right regardless of prefix. So this list \
         is not the `Ri-*` set — {} of its members carry another prefix (`{}`), and §0's \
         rights/rules ratio would be computed from this rather than from prefixes.\n\n",
        agent_rights.len(),
        agent_rights.join("`, `"),
        unprefixed.len(),
        unprefixed.join("`, `"),
    ));

    let with_requires: Vec<&(String, String)> = clause_index
        .iter()
        .filter(|(id, _)| requires(id).is_some())
        .collect();
    out.push_str("| `requires` | clauses | means |\n|---|---|---|\n");
    for req in crate::enforcement_register::Requires::ALL {
        let n = with_requires
            .iter()
            .filter(|(id, _)| requires(id) == Some(req))
            .count();
        let means = match req {
            crate::enforcement_register::Requires::Preventive => {
                "non-compliance must be made impossible"
            }
            crate::enforcement_register::Requires::Detective => {
                "each occurrence must be recorded — the correct requirement where prevention is unavailable, not a concession"
            }
            crate::enforcement_register::Requires::Both => {
                "both are demanded — usually two obligations under one id, and a **split candidate** at the clause's next amendment (§2.4.3)"
            }
        };
        out.push_str(&format!("| `{}` | {} | {} |\n", req.label(), n, means));
    }
    out.push_str(&format!(
        "\n`requires` is declared for **all {} classified clauses** — the field is mandatory \
         on the clause record, so coverage is a type property rather than something a test \
         has to chase.\n\n",
        with_requires.len(),
    ));

    out.push_str("## Clauses\n\n");
    out.push_str("| clause | binds | owed to | `requires` | `verified_by` | statement |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for (id, gloss) in clause_index {
        let statement = gloss.replace('|', "\\|");
        match relation(id) {
            Some(f) => out.push_str(&format!(
                "| `{}` | `{}` | {} | `{}` | `{}` | {} |\n",
                id,
                f.binds.label(),
                owed_cell(f.owed_to),
                f.requires.label(),
                f.verified_by.label(),
                statement,
            )),
            None => out.push_str(&format!(
                "| `{}` | — | — | — | — | *unclassified.* {} |\n",
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
        assert_eq!(seen.len(), 221, "every clause the active constitution declares");
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
        const UNCLASSIFIED: usize = 0;
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

        // Every declared clause is now classified, so the "unclassified"
        // case is only reachable with an id the constitution does not
        // declare — which is the boundary that still matters.
        assert!(relation("P-99.1").is_none());
        assert!(declared_at("P-99.1").is_none());

        // The two accessors never disagree about classification.
        for id in ["Ri-0.2", "U-1", "O-6", "I-14", "P-1.1", "P-99.1", "nonsense"] {
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
        // A numbered clause the constitution does not declare stays
        // unclassified rather than borrowing a neighbour's relation. `P-1.1`
        // used to serve here; every declared clause is classified now, so the
        // case needs an undeclared id.
        assert!(relation("P-1.99").is_none());
        assert!(relation("nonsense").is_none());
        // And a *declared* sub-clause resolves in its own right rather than
        // inheriting from a section — `P-1` is not a clause.
        assert_eq!(declared_at("P-1.1"), Some("P-1.1"));
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
        for sec in [
            "P-1.", "P-2.", "P-3.", "P-4.", "P-5.", "P-6.", "P-7.", "P-8.", "P-9.",
            "P-10.", "P-11.", "P-15.",
        ] {
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
        // No section awaits a tranche any more, so the anti-vacuity guard
        // moves to the other side: an id the constitution does not declare
        // must still resolve to nothing, or "classified completely" would be
        // satisfied by a table that answers for anything.
        assert!(relation("P-99.1").is_none(), "an undeclared id must not resolve");
        assert!(relation("Ri-9.9").is_none(), "an undeclared id must not resolve");
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
        // Nothing is outstanding any more, so the marker must be *absent* —
        // its presence would mean a clause slipped back to unclassified.
        assert!(
            !rendered.contains("*unclassified.*"),
            "the table claims complete coverage, so no row may be marked unclassified"
        );
        // Every declared clause has a row, including the ones that were the
        // last to be classified.
        for id in ["P-1.1", "P-6.1", "P-8.10", "P-11.5", "P-10.9"] {
            assert!(rendered.contains(&format!("| `{id}` |")), "{id} must be listed");
        }
    }

    /// `requires` coverage is enforced **by construction**, not by a ratchet.
    ///
    /// It began as a ratchet, because the `P-*` section tranches declared
    /// `binds`/`owed_to` before the field existed. Completing those 84 left
    /// every `requires`-less constructor unused, so they were removed and the
    /// field made mandatory: a clause cannot enter the table without a
    /// requirement, and a coverage test has nothing left to chase.
    ///
    /// This test remains as the guard against *undoing* that — reintroducing
    /// an `Option` or a defaulting constructor would silently restore the gap
    /// the ratchet used to measure.
    #[test]
    fn requires_coverage_is_a_type_property_not_a_ratchet() {
        // Trivially true given the field's type; asserted so that widening it
        // back to `Option` fails here rather than passing quietly.
        let all = relations();
        assert!(all.len() >= 221, "table shrank to {} clauses", all.len());
        for r in all {
            assert!(
                crate::enforcement_register::Requires::ALL.contains(&r.requires),
                "{} declares a requirement outside the closed set",
                r.id
            );
        }
        // Every classified clause answers, and every unclassified one does
        // not — the same boundary `relation()` draws.
        for id in ["Ri-0.3", "O-6", "U-1", "I-4", "P-8.1", "P-2.9", "P-7.22", "P-15.3"] {
            assert!(requires(id).is_some(), "{id} is classified");
        }
        for id in ["P-99.1", "Ri-9.9", "nonsense"] {
            assert!(requires(id).is_none(), "{id} is not a declared clause");
        }
    }

    /// `Detective` is the **correct** requirement where prevention is
    /// unavailable — the case that broke the floor model.
    ///
    /// Pinned by name because `Requires` reads as a two-value ladder with a
    /// "best of both" on top, which makes promoting these to `Preventive` the
    /// tempting cleanup. `I-4` is the proof it is not one: "the gateway does
    /// not make recovery decisions on the agent's behalf" is a universal
    /// negative over behaviour, and no type excludes it.
    #[test]
    fn detective_is_a_requirement_not_a_shortfall() {
        // The `P-*` members matter as much: each is a case where the
        // gateway cannot structurally exclude the failure. `P-2.21` is the
        // sharpest — the gateway cannot distinguish "could not determine"
        // from "decided to reject", so a wrong rejection is reviewable and
        // never preventable. `P-9.10` is a static import scan, where a missed
        // import is a miss rather than an excluded state.
        for id in [
            "I-4", "I-5", "I-7", "O-6", "O-7", "Ri-0.9",
            "P-2.21", "P-5.4", "P-7.4", "P-9.10",
        ] {
            let r = requires(id).expect("declared");
            assert!(
                r.detective(),
                "{id} requires recording; prevention is unavailable for it"
            );
            assert!(
                !r.preventive(),
                "{id} must not claim prevention — nothing static excludes the \
                 behaviour it forbids"
            );
        }
        // And the converse holds, so this is not vacuous: where the clause
        // names its own structural requirement, `preventive` is claimed.
        for id in ["I-3", "I-8", "I-9", "Ri-0.12", "Ri-0.15"] {
            assert!(requires(id).unwrap().preventive(), "{id} requires prevention");
        }
    }

    /// `Both` marks a clause carrying two obligations under one id — legal,
    /// but a **split candidate** (§2.4.3).
    ///
    /// `Ri-0.3` is the RFC's worked example and the reason `requires` is a
    /// set rather than a binary with a max-rule: an implementation that makes
    /// the empty rule list unrepresentable and builds no attribution review
    /// would, under a max-rule, read `preventive` and correctly claim full
    /// compliance — leaving the half where the interesting failures live
    /// unverified.
    #[test]
    fn mixed_clauses_demand_both_rather_than_rounding_to_one() {
        assert_eq!(requires("Ri-0.3"), Some(Requires::Both));
        let r = requires("Ri-0.3").unwrap();
        assert!(r.preventive() && r.detective(), "both limbs are demanded");

        // Pinned by name so a flip fails here rather than only in the
        // blessed-document diff, which a re-bless would carry straight past.
        // Each names its two limbs in the clause text: an absolute plus a
        // recording duty.
        for id in ["P-15.2", "P-15.3", "P-2.19", "P-5.3", "P-7.22", "P-9.6", "P-8.1", "U-3"] {
            assert_eq!(
                requires(id),
                Some(Requires::Both),
                "{id} demands both limbs; rounding to one leaves half unverified"
            );
        }

        // The set is meaningful only if it has members: a table where nothing
        // is `Both` has rounded its mixed clauses away.
        let both: Vec<&str> = relations()
            .iter()
            .filter(|r| r.requires == Requires::Both)
            .map(|r| r.id)
            .collect();
        assert!(
            both.len() >= 5,
            "only {} clause(s) marked Both — mixed clauses are common (a \
             representable core with a judgment-shaped penumbra), so a near-empty \
             set means they were rounded to whichever half was cheaper: {both:?}",
            both.len()
        );
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
        const INHERITED_FROM_A_SECTION_GROUPING: usize = 0;

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
        assert!(
            sections.is_empty(),
            "section-grouping inference must be **zero**: every numbered clause \
             in a section the register groups is now classified in its own \
             right. Still inferring: {sections:?}"
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

    /// §2 carries the three shapes the prefix scheme could not express, and
    /// each is pinned **by name**.
    ///
    /// Without this, flattening any of them failed only the law-table drift
    /// guard — which catches the change, but incidentally: because the
    /// rendered document differs, not because anything asserts the claim. A
    /// reviewer re-blessing the doc would sail straight past it. The same gap
    /// existed for §15 one tranche ago and is worth closing the same way.
    #[test]
    fn section_2_carries_the_shapes_the_prefix_scheme_could_not_express() {
        // **A reasoner clause.** "They *must attach* `execution_trace_id`" —
        // the obligation is the recording agent's. Everything classified
        // before §2 bound the enforcer, which made `reasoner` look vestigial;
        // it is not, and a table where the count returns to zero has lost
        // this.
        assert_eq!(relation("P-2.9").unwrap().binds, Binds::Reasoner);
        let reasoner_clauses = relations()
            .iter()
            .filter(|r| r.binds == Binds::Reasoner)
            .count();
        assert!(
            reasoner_clauses >= 1,
            "no clause binds the reasoner — the power the old binds() reported \
             for every clause now has no members, which cannot be right"
        );

        // **A decider obligation under a `P-` prefix.** "It must escalate to
        // a human operator rather than reject" binds whoever holds the gate
        // and is owed to the agent whose gate is pending. RFC defect 1.4(2).
        let p221 = relation("P-2.21").expect("classified");
        assert_eq!(p221.binds, Binds::Decider, "P-2.21 obliges the deciding seat");
        assert_eq!(p221.owed_to, TO_AGENT, "owed to the agent whose gate it is");

        // **Duties owed to the deciding seat.** Information or protection the
        // decider needs in order to decide — `Ri-0.15`'s relation, and
        // unrecordable before `OwedTo::Seat` existed.
        for id in ["P-2.5", "P-2.16", "P-2.24", "P-2.27"] {
            assert_eq!(
                relation(id).unwrap().owed_to,
                OwedTo::Seat(Binds::Decider),
                "{id} is owed to whoever decides, not to the agent and not to no one"
            );
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
