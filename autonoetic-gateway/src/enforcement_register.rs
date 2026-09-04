//! Enforcement Register — the generated bridge between constitutional
//! **clauses** (principles and rights) and the concrete code that enforces
//! them (epic #297; design `docs/proposals/constitution-restructure.md`).
//!
//! The constitution is being restructured so the *signed* artifact holds a
//! small set of **principles** (rules — invariants binding the *agent*) and
//! **rights** (guarantees binding the *gateway*), while the large, churning
//! "how" lives here as a register mapping each clause to its mechanical
//! checks, code citations, tests, and config knobs. Splitting the two keeps
//! the signed law legible and lets enforcement detail scale with the code.
//!
//! **Bind direction is declared data, not a prefix convention** (#299, then
//! RFC #1283 / #1284). Every clause records three relational fields:
//!
//! | Field | Domain | Means |
//! |---|---|---|
//! | [`Binds`] | exactly one **power** (`reasoner` / `enforcer` / `decider`) | who must comply; non-compliance is *their* violation |
//! | [`OwedTo`] | one principal kind, one power (seat-standing), or `NoOne` | who has standing to invoke it |
//! | [`VerifiedBy`] | a modality **floor** | how compliance is established |
//!
//! [`binds`] reads the declared field. It previously *derived* the bound party
//! from the ID prefix — principle ⇒ agent, right ⇒ gateway — and that
//! derivation was false for every principle in this register: all six bind the
//! **enforcer**, not the reasoner. The clause statements said so all along
//! (`P-5` opens "The gateway normalizes…"), and the section comments in
//! [`enforcement_register`] had drifted into recording the contradiction
//! outright — `P-15` was annotated "binds agent+gateway", a two-power value
//! the model makes unrepresentable.
//!
//! Prefixes are therefore **stable identifiers with no semantics**. `P-8.1`
//! stays `P-8.1`; its meaning lives in its fields. That is deliberate:
//! renaming clause IDs to carry meaning is what produced the
//! `R+`/`R++`/`R+++` wreckage (#1277), where 32 IDs had to be recovered from
//! breadcrumbs months later. An ID should be a name, not a claim.
//!
//! A **right is a view, not a family**: an obligation with
//! `binds: Enforcer, owed_to: Principal(AutonoeticAgent)` *is* an agent right
//! ([`OwedTo::is_agent_right`]), whatever prefix its ID carries. This is also
//! the correct legal semantics — real bills of rights bind the state, not
//! citizens.
//!
//! Not every enforcer duty is a right, and that is the distinction the prefix
//! scheme could not draw: `P-3.1` is `binds: Enforcer, owed_to: NoOne` — an
//! integrity property nobody can claim.
//!
//! Coverage so far (grows as #303 migrates the remaining sections):
//! - Principle `P-7` "Bounded progress" (the loop-guard family — #298).
//! - Rights `Ri-0.13` (reasoning privacy) and `Ri-0.14` (child wake-up),
//!   seeded as the proof that rights are register-modelled the same way.
//!
//! Deliberately **not yet** here (tracked in #298/#299):
//! - a `#[enforces(...)]` proc-macro to derive entries from code annotations
//!   (entries are hand-authored for now);
//! - runtime verification that each cited `test` *exists and passes* —
//!   partially closed (#820): [`citation_check::every_parseable_citation_resolves`]
//!   parses each entry's `code`/`test` citation and asserts the referenced
//!   file exists and the referenced symbol appears in it (a rename or moved
//!   site fails loudly), and
//!   [`citation_check::prose_only_citations_are_the_known_set`] pins the
//!   exact set of citations still too free-text to parse mechanically, so
//!   growing that set is a deliberate, reviewable choice. What's still open:
//!   actually *running* the cited test and checking it passes, and pinning
//!   the resolved file/symbol identity (not just presence) so a citation
//!   pointing at an unrelated same-named symbol wouldn't be caught;
//! - signing the register digest into the constitution lock;
//! - restructuring `constitution.md` into a Bill of Rights (needs a signed
//!   version bump — bundled with the #303 migration).
//!
//! Clause vs rule: each entry's `clause_id` is the parent principle/right
//! (e.g. `P-7`, `Ri-0.14`); its `rule_id` is the specific numbered
//! sub-clause it enforces (e.g. `P-7.19`). The flat `R-x.y` table is gone —
//! rules were renumbered `R-x.y` → `P-x.y` in the #303 migration; no `R-`
//! alias is retained.

use autonoetic_types::principal::PrincipalKindTag;

/// Shorthand for the overwhelmingly common `owed_to` in this register: a duty
/// owed to the agent under governance.
const TO_AGENT: OwedTo = OwedTo::Principal(PrincipalKindTag::AutonoeticAgent);

/// Which **power** a clause binds (RFC #1283 §2.1) — the closed,
/// constitutional set from `docs/concepts/separation-of-powers.md`.
///
/// A power is a *function*, so only a power's occupant can be obliged to act.
/// The values name functions rather than implementations on purpose: a
/// re-implementer reads "the enforcer owes X" as a specification of what to
/// build, not a description of our Rust. Clauses bind **seats, never
/// occupants** — `O-*` binds `Decider` whoever holds it, human or agent, so
/// `P-2.20` (an agent in the decider seat) needs no special case.
///
/// Exactly one value per clause, mirroring §0's "exactly one party". That is
/// what makes an aggregate like `community` unrepresentable: it is
/// "gateway + agents", and a clause that appears to bind it binds the
/// [`Binds::Enforcer`], because the enforcer is what implements the mechanism.
///
/// **Not `Ord`.** The three powers are co-equal under the separation of
/// powers; an ordering would assert that one outranks another, which is the
/// opposite of what the separation means. Nothing used it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Binds {
    /// Proposes and acts, subject to gating. Occupied by agents, script-mode
    /// agents, federated foreign agents (`SessionRole::Planner`, `Specialist`,
    /// `Sentinel`, `Curator`, `Auditor`).
    Reasoner,
    /// Mechanical enforcement — the Lawful Executor. Occupied by the gateway
    /// runtime (`SessionRole::Runtime`).
    Enforcer,
    /// Resolving gates. Occupied by the human operator and by
    /// `GateDecider`-holding agents (`SessionRole::Operator`). `operator` is
    /// the *occupant name* for this seat, not a separate party (#359).
    Decider,
}

impl Binds {
    pub fn label(self) -> &'static str {
        match self {
            Binds::Reasoner => "reasoner",
            Binds::Enforcer => "enforcer",
            Binds::Decider => "decider",
        }
    }

    /// The closed power set, in constitutional order. Used by the
    /// one-power-per-clause test to pin the arity.
    pub const ALL: [Binds; 3] = [Binds::Reasoner, Binds::Enforcer, Binds::Decider];
}

/// Who has **standing to invoke** a clause (RFC #1283 §2.2) — a principal
/// kind, a power (seat-standing), or nobody.
///
/// `binds` and `owed_to` range over genuinely different domains, which is the
/// correction three earlier drafts of the model needed: obligations attach to
/// *seats*, standing attaches to *principals*. The domain is
/// [`PrincipalKindTag`] ∪ [`Binds`] ∪ `{NoOne}` rather than a bespoke party
/// list, because the principal census evolves (federation will plausibly add
/// duties owed to foreign peers) and the relational schema must not need
/// amending when it does.
///
/// Single-valued: two standings means two clauses, which is what keeps
/// [`tests::no_two_clauses_share_a_relation_and_statement`] well-formed.
///
/// **Not `Ord`.** Standing is not ranked: an agent's claim does not outrank a
/// served party's, and neither outranks `NoOne` — which is not a lesser
/// standing but the absence of one. Nothing used it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OwedTo {
    /// Standing by identity — e.g. `Ri-0.2` → `AutonoeticAgent`,
    /// `U-1` → `ServedUser`.
    Principal(PrincipalKindTag),
    /// Standing by *seat occupancy*, kind-agnostic — e.g. `Ri-0.15`, whose
    /// `DecisionContext` is owed to whoever decides the gate, human or agent.
    Seat(Binds),
    /// An **integrity property**: no invocable beneficiary.
    ///
    /// This variant is what earns the model its keep. A *duty* is owed to
    /// someone who can invoke it; a *property* is owed to no one. `P-3.1`
    /// (sandboxes default to `--unshare-all`) benefits the operator, but
    /// nobody can *claim* it — an agent cannot demand its own confinement and
    /// would prefer not to have it. Recording that as `NoOne` is more honest
    /// than inventing a pseudo-party to fill the slot.
    ///
    /// Named `NoOne` rather than `None` so a `match` arm cannot silently
    /// resolve to `Option::None`.
    NoOne,
}

impl OwedTo {
    pub fn label(self) -> &'static str {
        match self {
            OwedTo::Principal(kind) => kind.as_str(),
            OwedTo::Seat(power) => power.label(),
            OwedTo::NoOne => "none",
        }
    }

    /// True when this clause is an agent **right** in the substantive sense —
    /// an enforcer duty owed to the agent (RFC §2.5: "a right is a view, not a
    /// family"). Independent of whether the clause ID carries an `Ri-` prefix,
    /// which is the point.
    pub fn is_agent_right(self, binds: Binds) -> bool {
        binds == Binds::Enforcer
            && self == OwedTo::Principal(PrincipalKindTag::AutonoeticAgent)
    }
}

/// What a clause **requires** of any implementation: that non-compliance be
/// made impossible, that each occurrence be recorded, or both.
///
/// The constitutional half of RFC #1283 §2.4.1, replacing the `verified_by`
/// "floor". Binary-plus-both rather than a six-point ladder because this is
/// the distinction that is genuinely **normative and survives
/// re-implementation**: *"`P-3.1` must be preventive"* is law in any language,
/// while *"preventive via `--unshare-all` at one chokepoint"* is a fact about
/// this gateway. The latter is [`VerifiedBy`], which is conformance data.
///
/// The variants are exactly the three **non-empty subsets** of
/// {preventive, detective}, so "at least one is required" holds by
/// construction — there is no empty value to represent a clause that demands
/// nothing.
///
/// **Not `Ord`.** `Detective` is not a weaker `Preventive`. For a universal
/// negative over behaviour (`I-4`: the gateway does not improvise) prevention
/// is unavailable, so recording each lapse is the *correct* requirement, not
/// a concession. That asymmetry is why the floor model failed (§2.4.1(a)) and
/// an ordering here would reintroduce it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Requires {
    /// Non-compliance must be made impossible — a type, a signature, a closed
    /// enum, or a guarded chokepoint. Covers paths that do not exist yet.
    Preventive,
    /// Each occurrence must be recorded and countable. The enforceable form
    /// for a duty with a deadline, or a universal negative over behaviour.
    Detective,
    /// Both are required.
    ///
    /// Usually a sign the clause carries **two obligations under one id** —
    /// `Ri-0.3` has a representable core (a rejection with no rule id can be
    /// made unrepresentable) and a judgment-shaped penumbra (that the named
    /// rule is the *actual* basis, which only review catches). §2.4.3 keeps
    /// this legal but **marked**: a correct description now, and a split
    /// candidate at the clause's next amendment, by the same argument §2.2
    /// makes for two standings meaning two clauses.
    Both,
}

impl Requires {
    pub fn label(self) -> &'static str {
        match self {
            Requires::Preventive => "preventive",
            Requires::Detective => "detective",
            Requires::Both => "preventive+detective",
        }
    }

    /// True when prevention is among the requirements.
    pub fn preventive(self) -> bool {
        matches!(self, Requires::Preventive | Requires::Both)
    }

    /// True when recording is among the requirements.
    pub fn detective(self) -> bool {
        matches!(self, Requires::Detective | Requires::Both)
    }

    /// The three non-empty subsets, for completeness checks.
    pub const ALL: [Requires; 3] = [Requires::Preventive, Requires::Detective, Requires::Both];
}

/// How compliance is established. **The model here is superseded — read
/// RFC #1283 §2.4.1 first**
/// (`docs/proposals/constitution-bind-direction-model.md`).
///
/// The values in [`crate::constitution_relations`] were assigned under the
/// "floor" account below, which is why it is retained. It is not the intended
/// design: a floor presumes a total order, and there is none. `Construction`
/// is not stronger than [`VerifiedBy::Detection`] for a universal negative
/// over behaviour — for `I-4` or `O-6`, detection is the *only* modality that
/// applies, so "at least X" is not a well-formed requirement. The field also
/// means two different things depending on the clause: this implementation's
/// mechanism for an enforced one, a requirement for an unmet one.
///
/// Replacement: `requires` (`preventive | detective` — a non-empty set,
/// constitutional) plus `achieved` (modality + site, per implementation).
///
/// **Never "upgrade" a `Detection` value** on a behavioural universal. The
/// variant order reads as a quality ladder, which makes raising them the
/// tempting cleanup; it would demand the impossible and assert a proof nobody
/// holds. Under `requires` this stops being a temptation, because
/// `detective` is a positive statement about the clause's nature rather than a
/// low rung.
///
/// **Deliberately not `Ord`.** An earlier version derived it, asserting the
/// total order the paragraph above denies and inviting
/// `verified_by >= VerifiedBy::Chokepoint`, a comparison with no meaning.
/// Nothing ever used it.
///
/// The retained account still explains why modality cannot be pinned exactly:
/// Rust reaches `Construction` for `Ri-0.12` via a closed enum, while a Python
/// re-implementation cannot and would reach for [`VerifiedBy::Registry`] plus
/// [`VerifiedBy::Test`]. That observation is what `achieved` captures, and it
/// is the half of §2.4 that survived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerifiedBy {
    /// The bad state is unrepresentable — type, signature, or closed enum.
    /// Covers paths that do not exist yet.
    Construction,
    /// N paths reduced to 1, plus a guard on bypassing the 1.
    Chokepoint,
    /// "Every X has Y" as a set comparison over a registry.
    Registry,
    /// Property-based over generated inputs. Cannot prove; can sample.
    Sampling,
    /// An ordinary example-based test at a named site.
    Test,
    /// Recorded and counted in production rather than proven absent.
    Detection,
}

impl VerifiedBy {
    pub fn label(self) -> &'static str {
        match self {
            VerifiedBy::Construction => "construction",
            VerifiedBy::Chokepoint => "chokepoint",
            VerifiedBy::Registry => "registry",
            VerifiedBy::Sampling => "sampling",
            VerifiedBy::Test => "test",
            VerifiedBy::Detection => "detection",
        }
    }
}

/// A constitutional principle. The signed constitution carries these (`P-*`);
/// enforcement detail lives in [`enforcement_register`].
///
/// Historically described as "a rule-side invariant binding the agent". That
/// was the prefix convention talking: `P-*` binds whichever power its declared
/// [`Principle::binds`] field names, and every principle currently in this
/// register binds the [`Binds::Enforcer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Principle {
    pub id: &'static str,
    pub title: &'static str,
    pub statement: &'static str,
    /// Which power must comply. Declared, never inferred from `id`.
    pub binds: Binds,
    /// Who has standing to invoke this clause.
    pub owed_to: OwedTo,
    /// Minimum modality that establishes compliance.
    pub verified_by: VerifiedBy,
    /// See [`Right::entrenched`] — the same correction-core concept applied
    /// to a principle. P-8.1 (hash-chain integrity) is the principle-side
    /// member of the entrenched core: without a tamper-evident causal chain,
    /// every other correction relies on evidence that could be silently
    /// rewritten.
    pub entrenched: bool,
}

/// A constitutional right — a guarantee the gateway upholds for the agent.
/// First-class alongside principles (#299); also enforced by concrete code,
/// so it appears in the register the same way.
///
/// Under the declared-field model a right is not a distinct *kind* of clause:
/// it is the `binds: Enforcer, owed_to: Principal(AutonoeticAgent)` shape
/// (RFC #1283 §2.5). The struct survives because `Ri-*` IDs are load-bearing
/// in the signed text and in §0's rights/rules ratio, not because rights are
/// structurally special.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Right {
    pub id: &'static str,
    pub title: &'static str,
    pub statement: &'static str,
    /// Which power must comply. `Enforcer` for every right here — declared
    /// rather than assumed, so a right that in fact binds the decider (the
    /// `Ri-0.15` shape) can say so.
    pub binds: Binds,
    /// Who has standing to invoke this clause.
    pub owed_to: OwedTo,
    /// Minimum modality that establishes compliance.
    pub verified_by: VerifiedBy,
    /// Part of the entrenched correction core (`docs/concepts/philosophy.md` §3.1 /
    /// §4.1): a clause whose loss would remove the machinery other errors are
    /// corrected through. May be strengthened by ordinary amendment; a
    /// weakening or removal amendment additionally requires the explicit,
    /// dated justification recorded in that version's `RATIFY.md` (see the
    /// constitution's Amendment Process, "Entrenched clauses"). See
    /// [`entrenched_clauses`] for the structural backstop.
    pub entrenched: bool,
}

/// A single enforcement point: which clause it serves (principle *or*
/// right), the numbered rule it enforces, the mechanical check, the code that
/// implements it, the test that pins it, and the config knob(s) that tune it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnforcementEntry {
    /// Parent clause this enforces — a `P-*` principle id or an `Ri-*` right id.
    pub clause_id: &'static str,
    /// The specific numbered rule/right id (e.g. `P-7.19`, `Ri-0.14`).
    pub rule_id: &'static str,
    /// Stable machine identifier for the check (matches the `reason`/`event`
    /// code emitted on causal events where applicable).
    pub check_id: &'static str,
    pub code: &'static str,
    pub test: &'static str,
    pub config: Option<&'static str>,
}

/// Principles (rule-side, bind the agent). Seeded with the loop-guard family
/// as the proof; grows as #303 migrates the remaining sections. `P-8.1` is
/// migrated here (ahead of the rest of §8) because `docs/concepts/philosophy.md` §3.1
/// names it as a member of the entrenched correction core — migrating it
/// lets the entrenchment backstop cover it structurally.
pub fn principles() -> &'static [Principle] {
    &[
        // P-2 — the *bounding* is enforcer machinery (`promotion_governor`).
        // The reasoner is what gets bounded, but "who must comply" is not
        // "who is affected": if respawns are not bounded, the enforcer failed
        // to bound them. `NoOne` because nobody can claim it — an agent
        // cannot demand its own respawn ceiling and would prefer not to have
        // one; the operator benefits without having standing to invoke.
        Principle {
            id: "P-2",
            title: "Approval Gates",
            statement: "Promotion and gate actions are bounded so that repeated mechanical \
                        rejection cannot be respawned indefinitely across sessions without \
                        operator acknowledgement.",
            binds: Binds::Enforcer,
            owed_to: OwedTo::NoOne,
            verified_by: VerifiedBy::Test,
            entrenched: false,
        },
        // P-5 — the statement names its own bound party in its first three
        // words ("The gateway normalizes…"), which is how far the prefix
        // convention had drifted from the text. Owed to the agent: "no gateway
        // judgment about the agent's output is silent or hidden" is a
        // guarantee the agent can invoke, so under RFC §2.5 P-5 *is* an agent
        // right wearing a `P-` prefix. Floor is `Detection` and must stay
        // there: "every intervention is observable and counted as a named
        // discretion leak" is a universal over behaviour, which no static
        // check reaches.
        Principle {
            id: "P-5",
            title: "Deterministic coercion and response validation",
            statement: "The gateway normalizes model I/O only through deterministic, \
                        pre-committed tolerances; every such intervention is observable and \
                        counted as a named discretion leak (§14). No gateway judgment about \
                        the agent's output is silent or hidden.",
            binds: Binds::Enforcer,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Detection,
            entrenched: false,
        },
        // P-7 — "a session is halted" is done by the enforcer (`guard.rs`),
        // and "no condition relies on agent self-report" explicitly excludes
        // the reasoner from the mechanism. `NoOne`: the typed attributable
        // reason is owed to the agent, but that duty is Ri-0.3's; what P-7
        // itself guarantees is that non-progressing sessions stop, which no
        // party can claim.
        Principle {
            id: "P-7",
            title: "Bounded progress",
            statement: "A session is halted when it stops making progress, on a closed, \
                        configurable set of mechanically-detected non-progress conditions, \
                        each emitting a typed, attributable reason. No condition relies on \
                        agent self-report.",
            binds: Binds::Enforcer,
            owed_to: OwedTo::NoOne,
            verified_by: VerifiedBy::Test,
            entrenched: false,
        },
        // P-8.1 — the causal chain is append-only and tamper-evident. This is
        // the substrate every correction-machinery clause depends on (read
        // your history, attribute decisions, prove what happened): if the
        // chain can be silently rewritten, none of those rights hold.
        //
        // The enforcer writes the chain, so the enforcer complies. `NoOne`
        // deliberately, and the distinction is worth stating because it is
        // easy to get wrong: Ri-0.2 and Ri-0.11 are duties owed to the agent,
        // and P-8.1 is the *property that makes them satisfiable*. Recording
        // it as owed to the agent would double-count one relationship as two.
        // Floor `Chokepoint`: a single append path plus hash recomputation as
        // the bypass guard — a re-implementer must provide both, whatever its
        // type system.
        Principle {
            id: "P-8.1",
            title: "Hash-chain integrity",
            statement: "The causal chain is append-only JSONL with hash-chain integrity — \
                        each entry's `entry_hash` binds its fields and its `prev_hash` links \
                        it to the prior entry. Tampering with any recorded field (actor, \
                        action, outcome) leaves a stale hash detectable by recomputation.",
            binds: Binds::Enforcer,
            owed_to: OwedTo::NoOne,
            verified_by: VerifiedBy::Chokepoint,
            entrenched: true,
        },
        // P-9 — Agent Install & Provenance. Parent principle for the §9
        // sub-rules registered individually (P-9.15 single door, P-9.16
        // import provenance). The §9 prose frame is "Three-stage activation:
        // artifact_build → revision.create → revision.promote"; the
        // registered sub-rules pin the guarantees that make the door
        // single and the import attributable.
        //
        // "Gated so that every surface … passes the same promotion gates" is
        // the enforcer's duty, not the installing agent's. Floor `Chokepoint`
        // is earned rather than assumed: the single door is N paths reduced to
        // 1, and its one declared exception (startup bootstrap auto-promoting
        // the operator's own reference bundles) is parameter-explicit
        // (`auto_promote: bool`) — a guard on the bypass, which is exactly
        // what distinguishes a chokepoint from a convention.
        Principle {
            id: "P-9",
            title: "Agent Install & Provenance",
            statement: "Three-stage activation — artifact_build, revision.create, \
                        revision.promote — gated so that every surface that activates an \
                        agent passes the same promotion gates (single door), and every \
                        externally-installed agent carries durable import provenance.",
            binds: Binds::Enforcer,
            owed_to: OwedTo::NoOne,
            verified_by: VerifiedBy::Chokepoint,
            entrenched: false,
        },
        // P-15 — Data Egress Localization (constitution 2026.07.30 / #910).
        // Parent principle for the §15 sub-rules: the egress label plane keeps
        // operator-declared private content off every sink its label excludes,
        // with withholding (not poisoning) and operator-only widening.
        //
        // The one clause here whose `owed_to` is not the agent or nobody.
        // Data locality exists for the **served party** — the end user whose
        // content is labelled — and `philosophy.md` §3.3 already reached this
        // conclusion in prose: "an entitlement in §12 would be a claim,
        // whereas an invariant on the enforcer is a guarantee". The field now
        // says it. This is also where the old prefix scheme broke down
        // hardest: the section comment below used to read "binds
        // agent+gateway", a two-power value.
        Principle {
            id: "P-15",
            title: "Data Egress Localization",
            statement: "Content carrying an egress label never reaches a sink the label \
                        excludes — at the LLM chokepoint, at every off-machine boundary, \
                        and across sessions via stored content — and widens only via an \
                        explicit, operator-approved, causal-logged declassification grant.",
            binds: Binds::Enforcer,
            owed_to: OwedTo::Principal(PrincipalKindTag::ServedUser),
            verified_by: VerifiedBy::Chokepoint,
            entrenched: false,
        },
    ]
}

/// Rights (gateway-side, bind the gateway). Seeded with two real rights as
/// the proof that rights are register-modelled identically to principles,
/// plus the four rights `docs/concepts/philosophy.md` §3.1/§4.1 names as the
/// **entrenched correction core** — the machinery through which every other
/// constitutional error gets fixed (read your own history, know why you were
/// denied, be able to propose change, be non-repudiably attributed).
pub fn rights() -> &'static [Right] {
    &[
        Right {
            id: "Ri-0.2",
            title: "Own history is readable",
            statement: "Every agent may read its own causal chain and execution trace. The \
                        gateway does not hide actions taken on the agent's behalf. Audit is not \
                        a privilege of operators; it is a right of the subject.",
            binds: Binds::Enforcer,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Test,
            entrenched: true,
        },
        // `Tagged::permission_with_rules` carries the rule IDs, but nothing in the
        // type forbids an empty rule list, so the floor is an example test at the
        // named site rather than `Construction`. Closing that gap — making a
        // ruleless rejection unrepresentable — would be a real strengthening.
        Right {
            id: "Ri-0.3",
            title: "Named rejection",
            statement: "Every rejection names the rule ID that caused it. No agent is ever told \
                        \"denied\" without being told why. Rejection without explanation is \
                        indistinguishable from arbitrary authority.",
            binds: Binds::Enforcer,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Test,
            entrenched: true,
        },
        Right {
            id: "Ri-0.8",
            title: "Right to propose amendment",
            statement: "Any agent holding the ConstitutionalProposal capability may submit an \
                        amendment proposal through the declared channel. The proposal receives a \
                        durable ID and enters the review queue; it cannot be silently dropped.",
            binds: Binds::Enforcer,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Test,
            entrenched: true,
        },
        // Shares P-8.1's substrate: `compute_entry_hash` binds `actor_id`, so
        // reattribution is detectable by the same recomputation. Unlike P-8.1
        // this one *is* owed to the agent — "the agent can prove what it did" is
        // invocable, which is precisely the duty/property split.
        Right {
            id: "Ri-0.11",
            title: "Non-repudiation",
            statement: "Every action an agent performs is attributed to that agent on the causal \
                        chain and cannot be retroactively reattributed. The agent can prove what \
                        it did; no party can claim the agent performed an action it did not.",
            binds: Binds::Enforcer,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Chokepoint,
            entrenched: true,
        },
        // `Construction`, and the strongest kind: `runtime/checkpoint.rs::YieldReason`
        // is a closed Rust enum, so an unlisted termination is a compile error at
        // every exhaustive match and deserialization rejects unknown variants.
        // This is the mechanism the 2026.09.02 amendment cites for I-9. A Python
        // re-implementation could not reach `Construction` here and would declare
        // `Registry` + `Test` — which is why the field is a floor, not an exact
        // modality.
        Right {
            id: "Ri-0.12",
            title: "Closed list of termination reasons",
            statement: "A session terminates only for a reason in the declared, closed list \
                        (agent exit, budget exhaustion, operator emergency stop, parent-orphan \
                        reap, unrecoverable fatal error naming a rule ID, scheduled timeout). \
                        Turn-budget exhaustion — the `max_session_turns_hard` ceiling that \
                        continuation approvals cannot lift — terminates as budget exhaustion; \
                        any termination outside the list is a rights violation and a gateway bug.",
            binds: Binds::Enforcer,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Construction,
            entrenched: false,
        },
        // `Construction` at the signature: `policy.rs::can_invoke_tool` and its 22
        // sibling decision surfaces do not take reasoning as a parameter, so the
        // gateway cannot consult it — including at call sites that do not exist
        // yet. The mechanism the amendment cites for I-8, and the strongest form
        // present anywhere in the document.
        Right {
            id: "Ri-0.13",
            title: "Reasoning privacy",
            statement: "An agent's internal reasoning is private-under-law: not used by the \
                        gateway as a basis for policy decisions, recorded to the agent's own \
                        causal chain for forensic review, and disclosed to other parties only \
                        through capability-gated audit.",
            binds: Binds::Enforcer,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Construction,
            entrenched: false,
        },
        Right {
            id: "Ri-0.14",
            title: "Wake-up over polling",
            statement: "When a child task reaches a terminal state or resolves a gate, the \
                        gateway wakes the parent with typed child state. Parents are not \
                        required to poll to discover child-state transitions.",
            binds: Binds::Enforcer,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Test,
            entrenched: false,
        },
        Right {
            id: "Ri-0.17",
            title: "Self capsule export (emigration)",
            statement: "An agent may request export of its own cognitive capsule for \
                        migration to another gateway. Scoped to the caller's own identity.",
            binds: Binds::Enforcer,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Test,
            entrenched: false,
        },
        Right {
            id: "Ri-0.18",
            title: "Right to report",
            statement: "Any agent may file an anomaly report without holding any \
                        capability; every flag is durably recorded, non-repudiably \
                        attributed, cannot be silently dropped, and filing is never \
                        itself grounds for sanction.",
            binds: Binds::Enforcer,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Test,
            entrenched: false,
        },
    ]
}

/// Correction-core clause IDs — principles, rights, and obligations marked
/// [`Principle::entrenched`] / [`Right::entrenched`] / [`Obligation::entrenched`].
/// Pure lookup; see [`tests::entrenched_clauses_all_exist_in_register`] for
/// the structural backstop that keeps this list honest as the register
/// evolves.
pub fn entrenched_clauses() -> Vec<&'static str> {
    principles()
        .iter()
        .filter(|p| p.entrenched)
        .map(|p| p.id)
        .chain(rights().iter().filter(|r| r.entrenched).map(|r| r.id))
        .chain(obligations().iter().filter(|o| o.entrenched).map(|o| o.id))
        .collect()
}

/// A §O decider obligation — a duty binding whoever *decides* a gate. The
/// symmetric counterpart to a [`Principle`] (agent) and a [`Right`] (gateway);
/// modelled in the register the same way (#359 / #399).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Obligation {
    pub id: &'static str,
    pub title: &'static str,
    pub statement: &'static str,
    /// Which power must comply. `Decider` for every `O-*` — the seat, not its
    /// occupant, so an agent-decider (`P-2.20`) is bound identically.
    pub binds: Binds,
    /// Who has standing to invoke this clause.
    pub owed_to: OwedTo,
    /// Minimum modality that establishes compliance.
    pub verified_by: VerifiedBy,
    /// See [`Right::entrenched`] — the same correction-core concept applied
    /// to a decider obligation.
    pub entrenched: bool,
}

/// Decider obligations (§O, bind the decider). Seeded with the two enacted
/// clauses (O-1 motivation, O-2 attribution); O-3/O-4/O-5 enter by amendment as
/// each becomes mechanically enforced (#399). O-6 (proposal adjudication duty)
/// and O-7 (anomaly adjudication duty) were enacted law from 2026.07.08 / 2026.07.19
/// respectively and join the register here so contract-health attributes their
/// `decider_obligation` / `sla_breached` events rather than bucketing `unattributed`.
pub fn obligations() -> &'static [Obligation] {
    &[
        // `Chokepoint`, and the code says so: `enforce_decider_motivation` sits at
        // the `decide_request_with_options` chokepoint and a BLOCKING decision does
        // not commit until a non-empty reason is recorded. Prevention, not
        // after-the-fact detection.
        Obligation {
            id: "O-1",
            title: "Motivated decision",
            statement: "A decision owes a motivation, graduated by stakes. A rejection/abort, or \
                        an approval of an elevated-authority or external/irreversible action, is \
                        BLOCKING: it does not commit until a non-empty reason is recorded. Silent \
                        rejection by a decider is as illegitimate as a gateway denial with no rule \
                        ID (Ri-0.3).",
            binds: Binds::Decider,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Chokepoint,
            entrenched: true,
        },
        // Attribution rests on the same hash binding as Ri-0.11 — `decided_by` +
        // `decided_by_kind` on the approval, actor bound into the entry hash — so
        // "cannot be reattributed" inherits that chokepoint's strength.
        Obligation {
            id: "O-2",
            title: "Attributed decision",
            statement: "Every decision is attributed to the deciding principal (id + kind) on the \
                        causal chain and cannot be reattributed. The agent under decision can \
                        always tell who decided and what kind of principal they are.",
            binds: Binds::Decider,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Chokepoint,
            entrenched: false,
        },
        // `Detection` is the *correct* floor, not a weak one. "A proposal left
        // un-adjudicated past the window is a recorded breach" is a duty to act
        // within a deadline: nothing static can prove a human will decide on
        // time, so the enforceable form is recording and counting the breach —
        // which is built (`flag_proposal_sla_breaches`). Upgrading this floor
        // would demand the impossible.
        Obligation {
            id: "O-6",
            title: "Duty to adjudicate proposals, on time",
            statement: "A proposal review authority owes every Ri-0.8 proposal a recorded, \
                        motivated decision within a bounded adjudication window; a proposal left \
                        un-adjudicated past the window is a recorded breach attributed to the \
                        adjudicating seat (the decision is still owed). Window duration is config.",
            binds: Binds::Decider,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Detection,
            entrenched: false,
        },
        // Same shape as O-6, and deliberately a separate clause rather than one
        // clause with two standings: the adjudicated object differs (Ri-0.18 flags
        // vs Ri-0.8 proposals) and single-valued `owed_to` is what keeps the
        // non-duplication test well-formed.
        Obligation {
            id: "O-7",
            title: "Duty to adjudicate reports, on time",
            statement: "An anomaly review authority owes every Ri-0.18 flag a recorded, motivated \
                        decision (confirmed/dismissed/deferred, with under_review as the \
                        non-terminal holding state) within a bounded adjudication window; a flag \
                        left un-adjudicated past the window is a recorded breach attributed to the \
                        adjudicating seat (the decision is still owed). Window duration is config.",
            binds: Binds::Decider,
            owed_to: TO_AGENT,
            verified_by: VerifiedBy::Detection,
            entrenched: false,
        },
    ]
}

/// The enforcement register. P-7's four checks (P-7.5/7.7/7.19/7.20), one
/// check per seeded right, plus §O decider obligations (O-1/O-2).
pub fn enforcement_register() -> &'static [EnforcementEntry] {
    &[
        // ── P-5 (deterministic coercion / response validation; enforcer, owed to agent) ──
        // Both entries are marked in the constitution as "DISCRETION LEAK"
        // — the gateway is doing its job, but any place it substitutes its
        // own judgment for the agent's is a named debt, not an invisible
        // convenience. The register entry makes it attributable in
        // contract-health.
        EnforcementEntry {
            clause_id: "P-5",
            rule_id: "P-5.2",
            check_id: "input_normalization_leak",
            code: "runtime/discretion_leak.rs::record_discretion_leak (tokio::task_local scope) \
                   + runtime/tool_call_processor.rs::note_llm_normalization \
                   + runtime/response_validation.rs::parse_reply_json (autonoetic_types::reply_json ladder)",
            test: "runtime::discretion_leak::tests",
            config: None,
        },
        EnforcementEntry {
            clause_id: "P-5",
            rule_id: "P-5.8",
            check_id: "gateway_authored_repair_leak",
            code: "runtime/response_validation.rs::validate_and_maybe_repair (gateway-authored repair prompt) \
                   + runtime/discretion_leak.rs::record_discretion_leak",
            test: "runtime::discretion_leak::tests",
            config: Some("response_validation.repair_enabled, response_validation.max_validation_loops, max_validation_duration_ms"),
        },
        // ── P-7 (enforcer, owed to no one) ──
        EnforcementEntry {
            clause_id: "P-7",
            rule_id: "P-7.5",
            check_id: "tool_failure_budget",
            code: "guard.rs::register_failure + check_loop",
            test: "runtime::guard::tests::test_loop_guard_trips_on_tool_failure_budget",
            config: Some("loop_guard.max_tool_failures"),
        },
        EnforcementEntry {
            clause_id: "P-7",
            rule_id: "P-7.7",
            check_id: "no_meaningful_progress",
            code: "guard.rs::check_loop",
            test: "runtime::guard::tests::test_loop_guard_trips_on_max_loops",
            config: Some("loop_guard.max_loops_without_progress"),
        },
        EnforcementEntry {
            clause_id: "P-7",
            rule_id: "P-7.19",
            check_id: "rotating_polling_pattern",
            code: "guard.rs::register_progress_inner (window + trip) + check_loop",
            test: "runtime::guard::tests::rotating_polling_pattern_with_five_tools_trips",
            config: Some("loop_guard.rotation_window_size, loop_guard.rotation_distinct_floor"),
        },
        EnforcementEntry {
            clause_id: "P-7",
            rule_id: "P-7.20",
            check_id: "child_failure_budget",
            code: "guard.rs::register_child_failure + check_loop",
            test: "runtime::guard::tests::test_loop_guard_trips_on_child_failures",
            config: Some("loop_guard.max_child_failures"),
        },
        // ── P-2.20 (issue #1192) ──
        // The `agent_decider.{kind}_gate` event carries `enforced_rules:
        // ["P-2.20"]`, but the rule had no register entry — so every
        // agent-decider ruling bucketed as `unattributed` in contract health.
        // Registered here so the seat's use is countable, which is the whole
        // point of putting an agent in it.
        EnforcementEntry {
            clause_id: "P-2",
            rule_id: "P-2.20",
            check_id: "agent_decider_capability",
            code: "scheduler/approval.rs::decide_request_with_options + runtime/human_gate.rs::verify_agent_decider",
            test: "constitution/gate_decider.rs",
            config: None,
        },
        // ── P-2.29 (issue #720) ──
        EnforcementEntry {
            clause_id: "P-2",
            rule_id: "P-2.29",
            check_id: "promotion_attempts_exhausted",
            code: "runtime/promotion_governor.rs::check_attempt_exhaustion + runtime/tools/agent_revision.rs::record_attempt",
            test: "promotion/attempt_exhaustion.rs",
            config: Some("promotion_governor.max_promotion_attempts_per_revision"),
        },
        // ── P-8.1 (enforcer, owed to no one; entrenched — correction core) ──
        EnforcementEntry {
            clause_id: "P-8.1",
            rule_id: "P-8.1",
            check_id: "hash_chain_integrity",
            code: "causal_chain.rs::compute_entry_hash (SHA-256 over actor_id + prev_hash + fields) + append-only linkage",
            test: "constitution/rights_early_bucket.rs::ri_0_11_tampered_actor_id_leaves_stale_hash",
            config: None,
        },
        // ── Ri-0.2 (enforcer, owed to agent; entrenched — correction core) ──
        EnforcementEntry {
            clause_id: "Ri-0.2",
            rule_id: "Ri-0.2",
            check_id: "own_history_readable",
            code: "observability.* tools gated by ReadAccess capability",
            test: "constitution/rights_early_bucket.rs::ri_0_2_agent_with_read_access_can_search_own_traces",
            config: None,
        },
        // ── Ri-0.3 (enforcer, owed to agent; entrenched — correction core) ──
        EnforcementEntry {
            clause_id: "Ri-0.3",
            rule_id: "Ri-0.3",
            check_id: "named_rejection",
            code: "Tagged::permission_with_rules + PolicyDecision.enforced_rules",
            test: "constitution/rights_late_bucket.rs::ri_0_3_capability_rejection_carries_rule_ids",
            config: None,
        },
        // ── Ri-0.8 (enforcer, owed to agent; entrenched — correction core) ──
        EnforcementEntry {
            clause_id: "Ri-0.8",
            rule_id: "Ri-0.8",
            check_id: "amendment_proposal_intake",
            code: "runtime/tools/constitution.rs::constitution_propose_amendment \
                   + scheduler/gateway_store/constitutional_proposals.rs",
            test: "constitution/rights_amendment_proposal.rs",
            config: None,
        },
        // ── Ri-0.11 (enforcer, owed to agent; entrenched — correction core) ──
        EnforcementEntry {
            clause_id: "Ri-0.11",
            rule_id: "Ri-0.11",
            check_id: "non_repudiation",
            code: "causal chain hash integrity + agent_id on every event; compute_entry_hash binds actor_id",
            test: "constitution/rights_early_bucket.rs::ri_0_11_hash_chain_integrity",
            config: None,
        },
        // ── Ri-0.12 (enforcer, owed to agent — closed list of termination reasons) ──
        // The `max_session_turns_hard` ceiling (issue #854) terminates a
        // session via reason (b) budget exhaustion: turn-budget exhausted. It
        // is the *absolute* cap that continuation approvals cannot lift, so a
        // delegated child cannot extend its window past it however many times
        // the soft gate clears.
        EnforcementEntry {
            clause_id: "Ri-0.12",
            rule_id: "Ri-0.12",
            check_id: "session_turn_hard_cap",
            code: "runtime/lifecycle.rs::execute_with_history + emit_session_turn_hard_cap_event \
                   + runtime/tool_dispatch.rs::effective_max_session_turns_hard",
            test: "runtime::lifecycle::tests::test_max_session_turns_hard_cap_terminates_without_approval",
            config: Some("max_session_turns_hard, max_session_turns, loop_guard.max_session_turns_hard"),
        },
        // ── Ri-0.13 (enforcer, owed to agent) ──
        EnforcementEntry {
            clause_id: "Ri-0.13",
            rule_id: "Ri-0.13",
            check_id: "reasoning_disclosure_capability_gated",
            code: "runtime/tools/observability.rs (reasoning audit) + disclosure gating",
            test: "constitution/private_reasoning_c.rs::ri_0_13c_execute_reads_and_discloses",
            config: None,
        },
        // ── Ri-0.14 (enforcer, owed to agent) ──
        EnforcementEntry {
            clause_id: "Ri-0.14",
            rule_id: "Ri-0.14",
            check_id: "child_state_wakeup",
            code: "scheduler/workflow_store.rs::update_task_run_status (send_child_state_notification) \
                   + scheduler/signal.rs + scheduler/task_notify.rs",
            test: "constitution/right_ri_0_14.rs::child_waiting_transition_emits_typed_parent_wakeup_event",
            config: Some("default_workflow_wait_secs"),
        },
        // ── Ri-0.17 (enforcer, owed to agent) ──
        EnforcementEntry {
            clause_id: "Ri-0.17",
            rule_id: "Ri-0.17",
            check_id: "self_capsule_export",
            code: "runtime/tools/capsule.rs::CapsuleExportTool (two-tier gate) \
                   + policy.rs::can_use_capsule_self",
            test: "capsule_self_export_scoping_integration.rs::self_export_denied_for_other_agent_id",
            config: None,
        },
        // ── O-1 (decider, owed to agent) ──
        EnforcementEntry {
            clause_id: "O-1",
            rule_id: "O-1",
            check_id: "decider_obligation_motivation",
            code: "scheduler/approval.rs::enforce_decider_motivation (classifier decision_is_blocking) \
                   at the decide_request_with_options chokepoint; emits decider_obligation.refused/.satisfied",
            test: "constitution/o_1_decider_motivation.rs + scheduler::approval::tests::decider_obligation_emits_tagged_o1_event",
            config: Some("decider_obligations.enabled"),
        },
        // ── O-2 (decider, owed to agent) ──
        EnforcementEntry {
            clause_id: "O-2",
            rule_id: "O-2",
            check_id: "decider_attribution",
            code: "decided_by + decided_by_kind on the approval (principal::decider_principal_kind, #361) \
                   + actor bound into the causal-chain entry hash (causal_chain.rs)",
            test: "constitution/o_1_decider_motivation.rs",
            config: None,
        },
        // ── Ri-0.18 (enforcer, owed to agent — capability-free intake + loud flood cap) ──
        // The tool's `is_available` is unconditionally true (Core tier); intake
        // is gated only by the per-reporter triage bound, which rejects past the
        // cap *loudly* (`anomaly_flag_flood`), never silently. The two halves
        // are pinned by separate tests; collapse them into one entry mirroring
        // the O-1/O-2 one-entry-per-clause convention.
        EnforcementEntry {
            clause_id: "Ri-0.18",
            rule_id: "Ri-0.18",
            check_id: "anomaly_flag_capability_free_intake",
            code: "runtime/tools/anomaly_flag.rs::AnomalyFlagTool \
                   + scheduler/gateway_store/anomaly_flags.rs::insert_anomaly_flag \
                   + scheduler/gateway_store/anomaly_flags.rs::emit_anomaly_flag_flood_alert",
            test: "anomaly_flag_integration.rs::tool_available_with_zero_capabilities \
                   + anomaly_flag_integration.rs::filing_emits_causal_event_tagged_ri_0_18 \
                   + anomaly_flags.rs::flood_cap_rejects_at_limit_and_keeps_existing \
                   + anomaly_flag_integration.rs::flood_cap_rejects_filing_loudly",
            config: Some("max_pending_anomaly_flags_per_reporter"),
        },
        // ── O-6 (decider, owed to agent — proposal adjudication duty + SLA breach) ──
        // Enacted law since 2026.07.08 (was missing from the register); now
        // registered alongside the SLA breach path so contract-health attributes
        // both the recorded decision and any `sla_breached` event against O-6.
        EnforcementEntry {
            clause_id: "O-6",
            rule_id: "O-6",
            check_id: "proposal_adjudication_recorded_within_sla",
            code: "scheduler/gateway_store/constitutional_proposals.rs::decide_constitutional_proposal \
                   + scheduler/gateway_store/constitutional_proposals.rs::flag_proposal_sla_breaches \
                   + scheduler.rs::check_adjudication_sla_breaches",
            test: "router.rs::test_dispatch_constitution_resolve_proposal \
                   + scheduler.rs::breaches_are_recorded_without_changing_status",
            config: Some("decider_obligations.enabled, decider_obligations.adjudication_sla_secs"),
        },
        // ── O-7 (decider, owed to agent — anomaly adjudication duty + SLA breach) ──
        // Both adjudication surfaces route through `decide_anomaly_flag`: the
        // operator seat (`anomaly.resolve`) and the ombudsman office
        // (`anomaly_adjudicate`, RFC Part F #774). The shared SLA test covers
        // both O-6 and O-7 (it inserts a sample proposal and a sample flag in
        // one body); citing it for both obligations is correct, not redundant.
        EnforcementEntry {
            clause_id: "O-7",
            rule_id: "O-7",
            check_id: "anomaly_adjudication_recorded_within_sla",
            code: "runtime/tools/anomaly_adjudicate.rs::AnomalyAdjudicateTool \
                   + scheduler/gateway_store/anomaly_flags.rs::decide_anomaly_flag \
                   + scheduler/gateway_store/anomaly_flags.rs::flag_anomaly_flag_sla_breaches \
                   + scheduler.rs::check_adjudication_sla_breaches",
            test: "router.rs::test_dispatch_anomaly_resolve_terminal_decision_without_reason_rejected \
                   + anomaly_adjudicate_tool_integration.rs::terminal_decision_requires_reason \
                   + scheduler.rs::breaches_are_recorded_without_changing_status",
            config: Some("decider_obligations.enabled, decider_obligations.adjudication_sla_secs"),
        },
        // ── P-9.15 (enforcer, owed to no one — single door for agent activation) ──
        // skill_install must install Candidate only; activation must route
        // through the AgentRevisionPromoteTool gate matrix. Startup bootstrap's
        // auto-promote of the operator's own reference bundles is the sole
        // declared exception, and it is parameter-explicit (`auto_promote: bool`).
        EnforcementEntry {
            clause_id: "P-9",
            rule_id: "P-9.15",
            check_id: "single_door_activation",
            code: "runtime/tools/skill.rs::SkillInstallTool \
                   + bootstrap.rs::bootstrap_single_agent_candidate_only \
                   + bootstrap.rs::bootstrap_agents \
                   + runtime/tools/agent_revision.rs::AgentRevisionPromoteTool \
                   + runtime/tools/agent_revision.rs::check_capability_delta",
            test: "skill_install_one_door_provenance.rs::one_door_generous_install_stays_candidate_and_unpromoted",
            config: None,
        },
        // ── P-9.16 (enforcer, owed to no one — import provenance on installed agents) ──
        // source_kind/source_ref recorded on the revision and an
        // agent_install/skill_imported causal event emitted, both durably.
        EnforcementEntry {
            clause_id: "P-9",
            rule_id: "P-9.16",
            check_id: "import_provenance_recorded",
            code: "runtime/tools/skill.rs::SkillInstallTool \
                   + bootstrap.rs::bootstrap_single_agent_candidate_only",
            test: "skill_install_one_door_provenance.rs::provenance_recorded_on_revision_and_causal_event",
            config: None,
        },
        // ── P-15 (data egress localization; enforcer, owed to served user; #910) ──
        // The label plane is gateway-managed (I-14); these entries pin the
        // three §15 rules to their enforcement points.
        EnforcementEntry {
            clause_id: "P-15",
            rule_id: "P-15.1",
            check_id: "egress_chokepoint_withhold",
            code: "llm/egress_chokepoint.rs::filter_request \
                   + runtime/egress_labeler.rs::plan_taint_following_route",
            test: "egress/chokepoint_canary.rs + egress/routing.rs",
            config: Some("egress.rules, egress.default_label, llm_presets.*.egress_class"),
        },
        EnforcementEntry {
            clause_id: "P-15",
            rule_id: "P-15.2",
            check_id: "egress_boundary_gate",
            code: "runtime/egress_labeler.rs::network_egress_boundary_refusal_json \
                   + runtime/egress_labeler.rs::mcp_remote_egress_refusal_json \
                   + runtime/egress_labeler.rs::ofp_federated_egress_refusal \
                   + runtime/egress_labeler.rs::emit_surface_boundary_refused",
            test: "egress/phase4_boundaries.rs + egress/phase4_web_hooks.rs + egress/phase4_sandbox.rs",
            config: None,
        },
        EnforcementEntry {
            clause_id: "P-15",
            rule_id: "P-15.3",
            check_id: "egress_declassification_only",
            code: "scheduler/approval.rs::apply_decision \
                   + scheduler/gateway_store/egress_declassification.rs::declassification_allows \
                   + runtime/egress_labeler.rs::emit_declassified",
            test: "egress/phase4_declassification.rs + egress/compartment.rs",
            config: Some("default_grant_ttl_secs"),
        },
    ]
}

/// Look up a principle by ID, including the numbered `P-x.y` sub-rules
/// that are codified in the signed constitution as children of a principle.
pub fn principle(id: &str) -> Option<&'static Principle> {
    principles().iter().find(|p| p.id == id).or_else(|| {
        // Numbered rules like P-2.29 inherit the parent principle statement.
        if let Some(parent) = id.split('.').next() {
            principles().iter().find(|p| p.id == parent)
        } else {
            None
        }
    })
}

/// Look up a right by ID.
pub fn right(id: &str) -> Option<&'static Right> {
    rights().iter().find(|r| r.id == id)
}

/// Look up a decider obligation by ID.
pub fn obligation(id: &str) -> Option<&'static Obligation> {
    obligations().iter().find(|o| o.id == id)
}

/// True when `clause_id` resolves to a known principle, right, or obligation.
pub fn clause_exists(clause_id: &str) -> bool {
    principle(clause_id).is_some()
        || right(clause_id).is_some()
        || obligation(clause_id).is_some()
}

/// Human-readable title for a clause (principle, right, *or* obligation).
/// `None` if the clause is unknown.
pub fn clause_title(clause_id: &str) -> Option<&'static str> {
    principle(clause_id)
        .map(|p| p.title)
        .or_else(|| right(clause_id).map(|r| r.title))
        .or_else(|| obligation(clause_id).map(|o| o.title))
}

/// Bind direction for a clause — **read from the clause's declared field**.
/// `None` if the clause is unknown.
///
/// This function used to infer: principle ⇒ agent, right ⇒ gateway,
/// obligation ⇒ decider. The inference was structurally unable to be right,
/// because it consulted the ID prefix rather than the clause, and it was
/// wrong for every principle in this register — all six bind the *enforcer*.
/// Reading the field is the whole point of #1284: bind direction is a property
/// of the obligation, so it has to be recorded on the obligation.
pub fn binds(clause_id: &str) -> Option<Binds> {
    crate::constitution_relations::relation(clause_id)
        .map(|r| r.binds)
        .or_else(|| principle(clause_id).map(|p| p.binds))
        .or_else(|| right(clause_id).map(|r| r.binds))
        .or_else(|| obligation(clause_id).map(|o| o.binds))
}

/// Standing to invoke a clause, read from its declared field. `None` if the
/// clause is unknown — distinct from `Some(OwedTo::NoOne)`, which is a
/// positive claim that the clause is an integrity property.
pub fn owed_to(clause_id: &str) -> Option<OwedTo> {
    crate::constitution_relations::relation(clause_id)
        .map(|r| r.owed_to)
        .or_else(|| principle(clause_id).map(|p| p.owed_to))
        .or_else(|| right(clause_id).map(|r| r.owed_to))
        .or_else(|| obligation(clause_id).map(|o| o.owed_to))
}

/// Verification-modality floor for a clause, read from its declared field.
pub fn verified_by(clause_id: &str) -> Option<VerifiedBy> {
    crate::constitution_relations::relation(clause_id)
        .map(|r| r.verified_by)
        .or_else(|| principle(clause_id).map(|p| p.verified_by))
        .or_else(|| right(clause_id).map(|r| r.verified_by))
        .or_else(|| obligation(clause_id).map(|o| o.verified_by))
}

/// One registered clause, flattened across the three ID families.
///
/// Exists because `Principle`, `Right` and `Obligation` are separate lists,
/// so every traversal has to remember all three — and a traversal that
/// forgets one fails silently. `the_power_set_is_closed_at_three` originally
/// chained principles and rights and skipped obligations, leaving `O-*` free
/// to declare an out-of-set power with nothing to catch it. Anything asking a
/// relational question should go through [`clause_relations`] rather than
/// re-chaining by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClauseRelation {
    pub id: &'static str,
    pub binds: Binds,
    pub owed_to: OwedTo,
    pub verified_by: VerifiedBy,
    pub statement: &'static str,
    pub entrenched: bool,
}

/// Every registered clause with its declared relational fields, principles
/// then rights then obligations.
pub fn clause_relations() -> Vec<ClauseRelation> {
    let principles = principles().iter().map(|p| ClauseRelation {
        id: p.id,
        binds: p.binds,
        owed_to: p.owed_to,
        verified_by: p.verified_by,
        statement: p.statement,
        entrenched: p.entrenched,
    });
    let rights = rights().iter().map(|r| ClauseRelation {
        id: r.id,
        binds: r.binds,
        owed_to: r.owed_to,
        verified_by: r.verified_by,
        statement: r.statement,
        entrenched: r.entrenched,
    });
    let obligations = obligations().iter().map(|o| ClauseRelation {
        id: o.id,
        binds: o.binds,
        owed_to: o.owed_to,
        verified_by: o.verified_by,
        statement: o.statement,
        entrenched: o.entrenched,
    });
    principles.chain(rights).chain(obligations).collect()
}

/// Clauses that are agent **rights** in the substantive sense — enforcer
/// duties owed to the agent — regardless of ID prefix (RFC §2.5).
///
/// Returns `P-5` alongside the `Ri-*` set: `P-5` guarantees the agent that no
/// gateway judgment about its output is silent, which is a right by relation
/// even though its ID says principle. §0's rights/rules ratio is computed from
/// prefixes today; this is what it would be computed from instead.
pub fn agent_rights() -> Vec<&'static str> {
    clause_relations()
        .into_iter()
        .filter(|c| c.owed_to.is_agent_right(c.binds))
        .map(|c| c.id)
        .collect()
}

/// Entries serving a given clause (principle or right).
pub fn entries_for(clause_id: &str) -> impl Iterator<Item = &'static EnforcementEntry> + '_ {
    enforcement_register()
        .iter()
        .filter(move |e| e.clause_id == clause_id)
}

/// Reverse-lookup: which parent clause does a numbered `P-x.y` / `Ri-x.y` rule
/// belong to? Lets enforcement events that carry rule IDs (e.g.
/// `loop_guard.tripped`'s `enforced_rules`) be attributed to their
/// principle/right for detection-loop correlation (#302). `None` if the rule
/// ID is not (yet) in the register.
pub fn clause_of_rule(rule_id: &str) -> Option<&'static str> {
    enforcement_register()
        .iter()
        .find(|e| e.rule_id == rule_id)
        .map(|e| e.clause_id)
}

/// A per-clause tally of enforcement occurrences — the raw signal behind a
/// "contract health" view (#302): which principles/rights are tripping, and
/// how often. Pure aggregation over occurrences the caller has already
/// gathered (e.g. from `causal_events`), so it is trivially testable and
/// carries no I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractHealth {
    /// `(clause_id, count)` sorted by descending count, then clause_id.
    pub by_clause: Vec<(String, u64)>,
    /// Occurrences whose rule ID did not resolve to any register clause
    /// (e.g. a not-yet-migrated rule) — surfaced rather than dropped so
    /// coverage gaps stay visible.
    pub unattributed: u64,
}

/// Tally enforcement occurrences (each identified by its numbered rule/right
/// ID) into a [`ContractHealth`], attributing each to its clause.
///
/// Builds the rule-ID → clause map once up front (rather than rescanning
/// the register per occurrence via [`clause_of_rule`]), keeping the tally
/// `O(register_entries + occurrences)` even as the register grows during
/// migration (#303).
pub fn contract_health<I, S>(rule_ids: I) -> ContractHealth
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    use std::collections::BTreeMap;
    let rule_to_clause: std::collections::HashMap<&'static str, &'static str> =
        enforcement_register()
            .iter()
            .map(|e| (e.rule_id, e.clause_id))
            .collect();
    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut unattributed = 0u64;
    for id in rule_ids {
        match rule_to_clause.get(id.as_ref()) {
            Some(clause) => *counts.entry(clause).or_insert(0) += 1,
            None => unattributed += 1,
        }
    }
    let mut by_clause: Vec<(String, u64)> =
        counts.into_iter().map(|(c, n)| (c.to_string(), n)).collect();
    // Descending count, then clause_id for stable ordering.
    by_clause.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ContractHealth {
        by_clause,
        unattributed,
    }
}

/// Clauses declared in this register (principle, right, or obligation IDs)
/// that recorded **zero** enforcement occurrences in `health` — either
/// perfectly deterrent (never needed to fire) or dead letter (never wired).
/// Scoped to clauses migrated into this structured register (#303 is
/// ongoing) — a clause not yet registered here is invisible to this check,
/// not confirmed live. Sorted for stable output.
pub fn dead_clauses(health: &ContractHealth) -> Vec<&'static str> {
    let seen: std::collections::HashSet<&str> =
        health.by_clause.iter().map(|(c, _)| c.as_str()).collect();
    let mut dead: Vec<&'static str> = principles()
        .iter()
        .map(|p| p.id)
        .chain(rights().iter().map(|r| r.id))
        .chain(obligations().iter().map(|o| o.id))
        .filter(|id| !seen.contains(id))
        .collect();
    dead.sort_unstable();
    dead
}

/// Render the register as a stable markdown document. This is the generated
/// artifact committed at `docs/constitution/enforcement-register.md`; the
/// [`tests::generated_register_matches_committed_doc`] test guards against
/// drift between code and the committed file.
pub fn render_register_markdown() -> String {
    let mut out = String::new();
    out.push_str("# Enforcement Register (generated)\n\n");
    out.push_str(
        "> **Generated** from `autonoetic-gateway/src/enforcement_register.rs`. Do not edit by \
         hand — run the register generator. Maps each constitutional **clause** to the \
         mechanical checks, code, tests, and config that enforce it, and records the three \
         relational fields (#1284): which power it **binds**, who it is **owed to**, and the \
         **verification floor** that establishes compliance. Legacy `R-x.y` / `Ri-x.y` IDs are \
         preserved as stable reference keys. See `docs/proposals/constitution-restructure.md` \
         and `docs/proposals/constitution-bind-direction-model.md`.\n\n",
    );

    out.push_str("## Bind-direction summary\n\n");
    out.push_str(
        "Bind direction is **declared per clause**, not derived from the ID prefix. The \
         headings below group by ID family because that is how the signed text is organised; \
         the `binds` column is the authority. Counts are partial while migration (#303) is in \
         progress — not the design ratio.\n\n",
    );
    out.push_str("| binds | clauses |\n|---|---|\n");
    for power in Binds::ALL {
        let mut ids: Vec<&str> = clause_relations()
            .into_iter()
            .filter(|c| c.binds == power)
            .map(|c| c.id)
            .collect();
        ids.sort_unstable();
        let cell = if ids.is_empty() {
            "— *none registered*".to_string()
        } else {
            format!("{} — `{}`", ids.len(), ids.join("`, `"))
        };
        out.push_str(&format!("| `{}` | {} |\n", power.label(), cell));
    }

    let mut substantive = agent_rights();
    substantive.sort_unstable();
    out.push_str(&format!(
        "\n**Agent rights by relation** ({}): `{}`. A right is a *view*, not a family — an \
         enforcer duty owed to the agent is an agent right whatever prefix its ID carries, \
         which is why this list is not the same as the `Ri-*` set.\n\n",
        substantive.len(),
        substantive.join("`, `"),
    ));

    out.push_str("## Principles (`P-*`)\n\n");
    for p in principles() {
        out.push_str(&format!("### {} — {}{ent}\n\n", p.id, p.title, ent = entrenched_tag(p.entrenched)));
        out.push_str(&render_relation(p.binds, p.owed_to, p.verified_by));
        out.push_str(&format!("{}\n\n", p.statement));
        out.push_str(&render_entries_table(p.id));
    }

    out.push_str("## Rights (`Ri-*`)\n\n");
    for r in rights() {
        out.push_str(&format!("### {} — {}{ent}\n\n", r.id, r.title, ent = entrenched_tag(r.entrenched)));
        out.push_str(&render_relation(r.binds, r.owed_to, r.verified_by));
        out.push_str(&format!("{}\n\n", r.statement));
        out.push_str(&render_entries_table(r.id));
    }

    out.push_str("## Obligations (`O-*`)\n\n");
    for o in obligations() {
        out.push_str(&format!("### {} — {}{ent}\n\n", o.id, o.title, ent = entrenched_tag(o.entrenched)));
        out.push_str(&render_relation(o.binds, o.owed_to, o.verified_by));
        out.push_str(&format!("{}\n\n", o.statement));
        out.push_str(&render_entries_table(o.id));
    }
    out
}

/// The one-line relational header rendered under each clause heading.
fn render_relation(binds: Binds, owed_to: OwedTo, verified_by: VerifiedBy) -> String {
    let owed = match owed_to {
        OwedTo::NoOne => "none *(integrity property)*".to_string(),
        OwedTo::Seat(power) => format!("`{}` *(seat-standing)*", power.label()),
        OwedTo::Principal(kind) => format!("`{}`", kind.as_str()),
    };
    format!(
        "**binds** `{}` · **owed to** {} · **floor** `{}`\n\n",
        binds.label(),
        owed,
        verified_by.label(),
    )
}

/// `""` for an ordinary clause, `" *(entrenched)*"` for one in the
/// correction core (`docs/concepts/philosophy.md` §3.1/§4.1). Surfaced in the rendered
/// register so the entrenchment a weakening amendment must overcome is visible
/// at the clause, not buried in the struct.
fn entrenched_tag(entrenched: bool) -> &'static str {
    if entrenched {
        " *(entrenched)*"
    } else {
        ""
    }
}

fn render_entries_table(clause_id: &str) -> String {
    let mut t = String::new();
    t.push_str("| rule id | check | code | test | config |\n");
    t.push_str("|---|---|---|---|---|\n");
    for e in entries_for(clause_id) {
        t.push_str(&format!(
            "| `{}` | `{}` | `{}` | `{}` | {} |\n",
            e.rule_id,
            e.check_id,
            e.code,
            e.test,
            e.config.map(|c| format!("`{c}`")).unwrap_or_else(|| "—".to_string()),
        ));
    }
    t.push('\n');
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    // ── Referential integrity (the achievable totality checks now) ──────

    #[test]
    fn every_entry_references_an_existing_clause() {
        for e in enforcement_register() {
            assert!(
                clause_exists(e.clause_id),
                "entry {} references unknown clause {}",
                e.rule_id,
                e.clause_id
            );
        }
    }

    #[test]
    fn every_principle_and_right_has_at_least_one_entry() {
        for p in principles() {
            assert!(
                entries_for(p.id).next().is_some(),
                "principle {} has no enforcement entries",
                p.id
            );
        }
        for r in rights() {
            assert!(
                entries_for(r.id).next().is_some(),
                "right {} has no enforcement entries",
                r.id
            );
        }
        for o in obligations() {
            assert!(
                entries_for(o.id).next().is_some(),
                "obligation {} has no enforcement entries",
                o.id
            );
        }
    }

    #[test]
    fn agent_decider_rulings_attribute_to_p_2() {
        // #1192: `agent_decider.{kind}_gate` events carry enforced_rules
        // ["P-2.20"]. Without a register entry they bucketed as
        // `unattributed`, so the seat's use was invisible to contract health.
        let health = contract_health(["P-2.20", "P-2.20"]);
        assert_eq!(health.unattributed, 0);
        assert!(health.by_clause.contains(&("P-2".to_string(), 2)));
    }

    #[test]
    fn decider_obligations_are_registered_and_attributable() {
        // §O clauses resolve like principles/rights, and bind the decider.
        assert!(clause_exists("O-1"));
        assert!(clause_exists("O-2"));
        assert_eq!(binds("O-1"), Some(Binds::Decider));
        assert_eq!(clause_title("O-1"), Some("Motivated decision"));
        // The O-1 motivation event (enforced_rules: ["O-1"]) attributes to its
        // clause via contract-health — not dropped as `unattributed`.
        let health = contract_health(["O-1", "O-1", "O-2"]);
        assert_eq!(health.unattributed, 0);
        assert!(health.by_clause.contains(&("O-1".to_string(), 2)));
        assert!(health.by_clause.contains(&("O-2".to_string(), 1)));
        assert_eq!(clause_of_rule("O-1"), Some("O-1"));
    }

    #[test]
    fn rule_ids_are_unique() {
        let mut seen = HashSet::new();
        for e in enforcement_register() {
            assert!(seen.insert(e.rule_id), "duplicate rule_id {}", e.rule_id);
        }
    }

    #[test]
    fn clause_check_pairs_are_unique() {
        let mut seen = HashSet::new();
        for e in enforcement_register() {
            assert!(
                seen.insert((e.clause_id, e.check_id)),
                "duplicate (clause, check) ({}, {})",
                e.clause_id,
                e.check_id
            );
        }
    }

    // ── Entrenchment (`docs/concepts/philosophy.md` §3.1/§4.1) ───────────────────

    #[test]
    fn entrenched_clauses_all_exist_in_register() {
        // The structural backstop: an entrenched ID that ever stops resolving
        // to a live right/obligation means the correction-core clause was
        // silently removed or renamed — this must fail loudly, immediately.
        for id in entrenched_clauses() {
            assert!(clause_exists(id), "entrenched clause {id} no longer exists in the register");
        }
    }

    #[test]
    fn entrenched_clauses_are_the_expected_correction_core() {
        let mut entrenched = entrenched_clauses();
        entrenched.sort_unstable();
        // The correction core spans all three bind-directions: a principle
        // (tamper-evident chain), four rights (read history, named rejection,
        // propose, non-repudiation), and an obligation (motivated decision —
        // the decider-side mirror of named rejection).
        assert_eq!(
            entrenched,
            vec!["O-1", "P-8.1", "Ri-0.11", "Ri-0.2", "Ri-0.3", "Ri-0.8"]
        );
    }

    #[test]
    fn register_markdown_marks_entrenched_clauses() {
        // The entrenchment a weakening amendment must overcome must be visible
        // at the clause in the published register, not just in the struct.
        let rendered = render_register_markdown();
        for id in entrenched_clauses() {
            let title = clause_title(id).unwrap_or("");
            assert!(
                rendered.contains(&format!("### {id} — {title} *(entrenched)*")),
                "entrenched clause {id} should carry the *(entrenched)* marker in the rendered register"
            );
        }
        // Non-entrenched registered clauses must NOT carry the marker, so the
        // tag stays a meaningful signal rather than decoration.
        for r in rights() {
            if !r.entrenched {
                assert!(
                    !rendered.contains(&format!("### {} — {} *(entrenched)*", r.id, r.title)),
                    "non-entrenched right {} should not carry the entrenched marker",
                    r.id
                );
            }
        }
        for o in obligations() {
            if !o.entrenched {
                assert!(
                    !rendered.contains(&format!("### {} — {} *(entrenched)*", o.id, o.title)),
                    "non-entrenched obligation {} should not carry the entrenched marker",
                    o.id
                );
            }
        }
    }

    #[test]
    fn principle_and_right_ids_do_not_collide() {
        for p in principles() {
            assert!(
                right(p.id).is_none(),
                "id {} is both a principle and a right",
                p.id
            );
        }
    }

    #[test]
    fn required_fields_are_non_empty() {
        for e in enforcement_register() {
            assert!(!e.rule_id.is_empty(), "empty rule_id");
            assert!(!e.check_id.is_empty(), "empty check_id for {}", e.rule_id);
            assert!(!e.code.is_empty(), "empty code for {}", e.rule_id);
            assert!(!e.test.is_empty(), "empty test for {}", e.rule_id);
        }
    }

    // ── Bind direction (RFC #1283 §6) ───────────────────────────────────

    /// **§6.1 completeness** — every clause declares all three relational
    /// fields.
    ///
    /// The fields are non-`Option`, so a clause that omits one does not
    /// compile: completeness is enforced *by construction*, the strongest
    /// modality in [`VerifiedBy`], and this test's job is only to stop that
    /// guarantee from passing **vacuously**. An empty register would satisfy
    /// "every clause declares its fields" trivially, so the shape being
    /// pinned is that the register is populated and every clause resolves
    /// through the public accessors.
    #[test]
    fn every_clause_declares_all_three_relational_fields() {
        let ids: Vec<&str> = clause_relations().into_iter().map(|c| c.id).collect();
        assert!(
            ids.len() >= 19,
            "register shrank to {} clauses — this test would pass vacuously",
            ids.len()
        );
        for id in ids {
            assert!(binds(id).is_some(), "{id} does not resolve a binds field");
            assert!(owed_to(id).is_some(), "{id} does not resolve an owed_to field");
            assert!(
                verified_by(id).is_some(),
                "{id} does not resolve a verified_by field"
            );
        }
        // An unknown clause resolves nothing — distinct from a known clause
        // that resolves `OwedTo::NoOne`, which is a positive claim.
        assert_eq!(binds("nope"), None);
        assert_eq!(owed_to("nope"), None);
        assert_eq!(verified_by("nope"), None);
    }

    /// **§6.2 one power per clause** — enforcing §0's own "exactly one party"
    /// and making an aggregate like `community` unrepresentable.
    ///
    /// Arity is a type property: `binds` holds one [`Binds`], so
    /// "binds agent+gateway" — which is what the `P-15` section comment
    /// actually said before #1284 — cannot be written. What this test pins is
    /// that the power set stays *closed at three*: a fourth power would be a
    /// constitutional change to the separation of powers, not a refactor, and
    /// [`Binds::ALL`] silently growing is how that would slip through.
    #[test]
    fn the_power_set_is_closed_at_three() {
        let labels: Vec<&str> = Binds::ALL.iter().map(|b| b.label()).collect();
        assert_eq!(labels, vec!["reasoner", "enforcer", "decider"]);

        // Every declared value is a member of ALL — no clause reaches a power
        // outside the closed set. Over *all three* families: this loop chained
        // principles and rights only, so an `O-*` obligation could declare an
        // out-of-set power with nothing to catch it.
        let all = clause_relations();
        assert!(
            all.iter().any(|c| c.id.starts_with("O-")),
            "obligations must be in scope — that omission was the original defect"
        );
        for c in &all {
            assert!(
                Binds::ALL.contains(&c.binds),
                "{} binds a power outside the closed set",
                c.id
            );
            assert_eq!(
                binds(c.id),
                Some(c.binds),
                "{} resolves a different power through binds() than it declares",
                c.id
            );
        }
    }

    /// **§6.3 no prefix inference** — the test the RFC specifies as the
    /// fails-before/passes-after artifact.
    ///
    /// Under the old derivation `binds()` returned `Agent` for anything
    /// matching a principle, so a `P-*` clause binding the enforcer was
    /// *unrepresentable*. It is now not merely representable but the
    /// majority: **all six** registered principles bind the enforcer, and
    /// none binds the reasoner.
    ///
    /// That is a fact about this register, not about the constitution: these
    /// six are parent principles seeded because they are mechanism-heavy, and
    /// the ~182 numbered `P-*` in the signed text are expected to split
    /// roughly 44 reasoner / 15 enforcer / 117 needing a clause-by-clause
    /// read (#1284 part 2). So the assertion below is deliberately "at least
    /// one", matching the RFC — a stricter "all" would break the moment the
    /// first reasoner-binding principle is registered, which is the normal
    /// case, not a regression.
    #[test]
    fn binds_reads_the_declared_field_not_the_id_prefix() {
        let enforcer_principles: Vec<&str> = principles()
            .iter()
            .filter(|p| p.binds == Binds::Enforcer)
            .map(|p| p.id)
            .collect();
        assert!(
            !enforcer_principles.is_empty(),
            "no P-* clause binds the enforcer — either the fields regressed to \
             prefix inference, or binds() is deriving again"
        );

        // P-5's statement opens "The gateway normalizes…", and P-15's duty is
        // owed to the served party. Both were reported as binding the agent
        // for as long as the derivation existed; they are the concrete lies
        // the RFC cites, so pin them by name.
        assert_eq!(binds("P-5"), Some(Binds::Enforcer));
        assert_eq!(binds("P-15"), Some(Binds::Enforcer));
        assert_eq!(
            owed_to("P-15"),
            Some(OwedTo::Principal(PrincipalKindTag::ServedUser))
        );

        // A numbered sub-rule inherits its parent's declared direction rather
        // than being re-derived from its own prefix.
        assert_eq!(binds("P-15.1"), Some(Binds::Enforcer));
        assert_eq!(binds("P-9.15"), Some(Binds::Enforcer));
    }

    /// **§6.4 agreement** — where the register and the law table both speak
    /// about a clause, they must say the same thing.
    ///
    /// The RFC specifies this as register-vs-*document* agreement, which has
    /// to wait for the amendment that puts the columns in the signed text
    /// (#1284 part 3). This is the reachable half: `constitution_relations`
    /// is the law side today and the accessors defer to it, so a
    /// disagreement means a reader gets one answer and `binds()` returns
    /// another.
    ///
    /// The two do not overlap completely, by design. The register's
    /// `principles()` uses **section-level** groupings as clause ids —
    /// `P-7` collects `P-7.5`, `P-7.19`, … — and a section is not a clause:
    /// the constitution declares `P-7.5`, never a bare `P-7`. The law table
    /// holds only real clauses, so those groupings have no counterpart and
    /// keep their own declared fields. `P-8.1` is the one register principle
    /// that *is* a numbered clause, and this test is what moved it law-side.
    #[test]
    fn the_register_and_the_law_table_never_disagree() {
        let mut compared = 0usize;
        for c in clause_relations() {
            let Some(law) = crate::constitution_relations::relation(c.id) else {
                // Section-level grouping — no law-side counterpart.
                assert!(
                    !c.id.contains('.'),
                    "{} is a numbered clause but the law table does not classify \
                     it; a numbered clause must be law-side, not register-only",
                    c.id
                );
                continue;
            };
            compared += 1;
            assert_eq!(
                (c.binds, c.owed_to, c.verified_by),
                (law.binds, law.owed_to, law.verified_by),
                "{} declares different relations in the register and the law table",
                c.id
            );
        }
        assert!(
            compared >= 14,
            "expected the register's real clauses to be law-side too; compared \
             only {compared} — this test has gone vacuous"
        );
    }

    /// **§6.5 non-duplication** — no two clauses share
    /// `(binds, owed_to, statement)`.
    ///
    /// This is the check that would have caught `R+9` duplicating `R-4.14` on
    /// the day it was written (#1277): a redundant clause survived for months
    /// because nothing compared clauses to each other. Comparing the
    /// *statement* alongside the relation is what makes it a duplication test
    /// rather than a grouping test — many clauses legitimately share
    /// `(Enforcer, agent)`.
    #[test]
    fn no_two_clauses_share_a_relation_and_statement() {
        let mut seen: HashMap<(Binds, OwedTo, &str), &str> = HashMap::new();
        for c in clause_relations() {
            let (id, b, o, statement) = (c.id, c.binds, c.owed_to, c.statement);
            if let Some(prior) = seen.insert((b, o, statement), id) {
                panic!(
                    "{id} duplicates {prior}: same binds/owed_to and an identical \
                     statement. Two clauses saying one thing is the duplicate-clause \
                     defect of #1277 — merge them, or make the distinct one say what \
                     is distinct."
                );
            }
        }
    }

    /// A right is a **view, not a family**: the substantive set is "enforcer
    /// duties owed to the agent", which does not coincide with the `Ri-*`
    /// prefix set.
    ///
    /// `P-5` is the concrete case — it guarantees the agent that no gateway
    /// judgment about its output is silent, so it is an agent right carrying a
    /// principle's ID. §0 computes its rights/rules ratio from prefixes; this
    /// pins what that ratio would be computed from instead, and the gap is the
    /// measure of how much the prefix scheme distorts it.
    #[test]
    fn agent_rights_are_a_relation_not_a_prefix() {
        let by_relation = agent_rights();
        assert!(
            by_relation.contains(&"P-5"),
            "P-5 is an enforcer duty owed to the agent — a right by relation, \
             whatever its prefix says. Got: {by_relation:?}"
        );
        for r in rights() {
            assert!(
                by_relation.contains(&r.id),
                "{} carries an Ri- prefix but is not an enforcer duty owed to \
                 the agent — if that is intended (the Ri-0.15 seat-standing \
                 shape), this test needs to name the exception",
                r.id
            );
        }
        // Integrity properties are not rights: nobody can invoke them.
        assert!(!by_relation.contains(&"P-8.1"));
        assert!(!by_relation.contains(&"P-7"));
    }

    /// `Detection` floors are **correct**, not deficient, and must not be
    /// "upgraded" — a duty to act within a deadline (`O-6`/`O-7`) or a
    /// universal over behaviour (`P-5`) cannot be proven statically.
    ///
    /// Pinned as a test because the variant order reads as a quality ladder,
    /// so the tempting cleanup is to raise these floors. Doing so would demand
    /// the impossible and produce a false claim of proof.
    #[test]
    fn behavioural_universals_keep_their_detection_floor() {
        for id in ["P-5", "O-6", "O-7"] {
            assert_eq!(
                verified_by(id),
                Some(VerifiedBy::Detection),
                "{id} is a behavioural universal — recording and counting each \
                 lapse is the enforceable form, and the strongest one available"
            );
        }
        // The converse: where construction *is* reachable, claim it. These two
        // are the mechanisms the 2026.09.02 amendment cites for I-8 and I-9.
        assert_eq!(verified_by("Ri-0.13"), Some(VerifiedBy::Construction));
        assert_eq!(verified_by("Ri-0.12"), Some(VerifiedBy::Construction));
    }

    // ── Detection-loop foundation (#302) ────────────────────────────────

    #[test]
    fn clause_of_rule_resolves_and_misses() {
        assert_eq!(clause_of_rule("P-7.19"), Some("P-7"));
        assert_eq!(clause_of_rule("P-7.5"), Some("P-7"));
        assert_eq!(clause_of_rule("Ri-0.14"), Some("Ri-0.14"));
        assert_eq!(clause_of_rule("P-9.99"), None);
    }

    #[test]
    fn contract_health_tallies_by_clause_descending() {
        let occurrences = ["P-7.19", "P-7.19", "P-7.5", "Ri-0.14", "P-9.99"];
        let health = contract_health(occurrences);
        // P-7.19 + P-7.5 both → P-7 (3), Ri-0.14 → Ri-0.14 (1); P-9.99 unattributed.
        assert_eq!(health.by_clause, vec![
            ("P-7".to_string(), 3),
            ("Ri-0.14".to_string(), 1),
        ]);
        assert_eq!(health.unattributed, 1);
    }

    #[test]
    fn contract_health_empty_is_empty() {
        let health = contract_health(Vec::<String>::new());
        assert!(health.by_clause.is_empty());
        assert_eq!(health.unattributed, 0);
    }

    #[test]
    fn dead_clauses_reports_zero_enforcement_registered_clauses() {
        // Nothing enforced at all: every registered clause is dead.
        let health = contract_health(Vec::<String>::new());
        let dead = dead_clauses(&health);
        for p in principles() {
            assert!(dead.contains(&p.id), "{} should be dead when nothing fired", p.id);
        }
        for r in rights() {
            assert!(dead.contains(&r.id), "{} should be dead when nothing fired", r.id);
        }
        for o in obligations() {
            assert!(dead.contains(&o.id), "{} should be dead when nothing fired", o.id);
        }

        // Fire every registered clause at least once: none are dead.
        let all_rule_ids: Vec<&str> = enforcement_register().iter().map(|e| e.rule_id).collect();
        let full_health = contract_health(all_rule_ids);
        assert!(
            dead_clauses(&full_health).is_empty(),
            "expected no dead clauses once every register entry has fired, got {:?}",
            dead_clauses(&full_health)
        );
    }

    #[test]
    fn dead_clauses_excludes_only_the_clauses_that_fired() {
        // P-7 fires (via P-7.19); P-2, the rights, and the obligations do not.
        let health = contract_health(["P-7.19"]);
        let dead = dead_clauses(&health);
        assert!(!dead.contains(&"P-7"), "P-7 fired, should not be dead");
        assert!(dead.contains(&"P-2"), "P-2 did not fire, should be dead");
        assert!(dead.contains(&"Ri-0.13"));
        assert!(dead.contains(&"O-1"));
    }

    // ── Code ↔ register bridge (real, not aspirational) ─────────────────

    /// The loop-guard family in the register must agree with the
    /// `LoopGuardTripReason::rule_id()` mapping in code — each trip reason's
    /// rule_id must appear as a P-7 entry, and its `code()` reason string
    /// must match the register's `check_id`. The concrete code↔register
    /// integrity check the full mechanism generalises.
    #[test]
    fn loop_guard_reasons_agree_with_register() {
        use crate::runtime::guard::LoopGuardTripReason;
        // Canonical detectors: each owns a distinct P-7 register entry whose
        // check_id equals the trip reason's code() (strict 1:1 bridge).
        let canonical = [
            LoopGuardTripReason::ToolFailureBudget { tool: "x".into(), failures: 1 },
            LoopGuardTripReason::NoMeaningfulProgress { cycles: 1 },
            LoopGuardTripReason::RotatingPollingPattern { window_size: 16, distinct_count: 6, floor: 6 },
            LoopGuardTripReason::ChildFailureBudget { failures: 3 },
        ];
        for r in &canonical {
            let entry = enforcement_register()
                .iter()
                .find(|e| e.rule_id == r.rule_id())
                .unwrap_or_else(|| panic!("no register entry for rule {}", r.rule_id()));
            assert_eq!(entry.clause_id, "P-7", "loop-guard rule {} must serve P-7", r.rule_id());
            assert_eq!(
                entry.check_id,
                r.code(),
                "register check_id must match the trip reason code for {}",
                r.rule_id()
            );
        }

        // Auxiliary detectors enforce an *existing* principle via a faster /
        // narrower path and so share its register entry rather than owning a
        // new one (rule_ids stay unique). We assert only that their rule_id is
        // registered and attributes to the same clause — contract-health
        // tallies by rule_id, so a shared entry is correct. RedundantRosterPolling
        // is a fast path for P-7.19 (no semantic progress), canonically
        // detected by rotating_polling_pattern.
        let auxiliary = [
            LoopGuardTripReason::RedundantRosterPolling {
                tool: "agent_list".into(),
                repeats: 3,
                floor: 3,
            },
            // RepeatedIrrecoverableRejection (#718) is the single-tool fast
            // path for P-7.7 (re-asking one already-answered gate), canonically
            // detected by no_meaningful_progress; it shares the P-7.7 entry.
            LoopGuardTripReason::RepeatedIrrecoverableRejection {
                tool: "agent_revision_promote".into(),
                error_hash: 0,
                occurrences: 3,
            },
        ];
        for r in &auxiliary {
            let entry = enforcement_register()
                .iter()
                .find(|e| e.rule_id == r.rule_id())
                .unwrap_or_else(|| panic!("no register entry for rule {}", r.rule_id()));
            assert_eq!(entry.clause_id, "P-7", "auxiliary rule {} must serve P-7", r.rule_id());
        }
    }

    // ── Generation / drift guard ────────────────────────────────────────

    #[test]
    fn render_is_deterministic() {
        assert_eq!(render_register_markdown(), render_register_markdown());
    }

    /// Regenerate the committed register doc. Run with
    /// `BLESS_REGISTER=1 cargo test -p autonoetic-gateway bless_register_doc`
    /// after changing the register, then commit the updated file. No-op
    /// unless the env var is set.
    #[test]
    fn bless_register_doc() {
        if std::env::var("BLESS_REGISTER").is_err() {
            return;
        }
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../docs/constitution/enforcement-register.md"
        );
        std::fs::write(path, render_register_markdown()).expect("write register doc");
    }

    /// The generated register must match the committed doc artifact. If this
    /// fails, regenerate (`BLESS_REGISTER=1`) — the drift guard keeping code
    /// and the published register in sync.
    #[test]
    fn generated_register_matches_committed_doc() {
        let committed = include_str!("../../docs/constitution/enforcement-register.md");
        assert_eq!(
            render_register_markdown(),
            committed,
            "generated enforcement register differs from the committed doc; \
             regenerate docs/constitution/enforcement-register.md (BLESS_REGISTER=1)"
        );
    }
}

/// Mechanical citation verification (issue #820, stage 1). The `code`/`test`
/// fields on [`EnforcementEntry`] are free text — until now nothing checked
/// that the files/symbols they name actually exist, so a refactor could move
/// an enforcement site and the register would silently rot. This module
/// parses each citation and resolves what it can:
///
/// - a `path.rs` token is a **file** reference — resolved by trying it as a
///   relative path under `autonoetic-gateway/src`, `autonoetic-gateway/tests`,
///   `autonoetic-types/src` (in that order), then — for a bare filename with
///   no `/` — falling back to a recursive filename search under those roots;
/// - `path.rs::symbol` additionally asserts `symbol` is a substring of the
///   resolved file (not syntax-aware; it catches renames, not semantics);
/// - a bare identifier clause chains to the most recently resolved file in
///   its own span (`guard.rs::a + b` checks both `a` and `b` in `guard.rs`);
/// - an `a::b::c` Rust module path is resolved by treating the last segment
///   as the symbol and progressively shortening the rest as a candidate file
///   (`a/b.rs`, `a.rs`, …) under the same three roots;
/// - parenthetical asides are parsed independently (their own span, with no
///   inherited "current file"), so descriptive commentary like `(window +
///   trip)` doesn't get force-checked as symbol names against the file named
///   outside the parens — but a self-contained reference inside a paren
///   (e.g. `(principal::decider_principal_kind, #361)`) still resolves;
/// - anything left over — no `.rs`, no resolvable module path — is prose:
///   collected rather than failed. [`prose_only_citations_are_the_known_set`]
///   pins the current set so growth is a deliberate, reviewable choice.
///
/// This closes the module doc's former "no runtime verification that cited
/// tests exist" gap for the file/symbol-existence half of that claim; test
/// *execution* verification and register-digest signing remain open (#298/#299).
#[cfg(test)]
mod citation_check {
    use super::*;
    use std::path::{Path, PathBuf};

    /// The three roots a citation token may resolve against, in search order.
    fn roots() -> [PathBuf; 3] {
        let manifest = env!("CARGO_MANIFEST_DIR");
        [
            PathBuf::from(manifest).join("src"),
            PathBuf::from(manifest).join("tests"),
            PathBuf::from(manifest).join("../autonoetic-types/src"),
        ]
    }

    /// Outcome of checking one parsed token from a citation.
    #[derive(Debug, Clone)]
    enum Finding {
        /// Resolved to a file (and, if cited, its symbol found inside it).
        Ok,
        /// Free text collected rather than force-parsed (grandfathered). The
        /// text itself isn't asserted on — only its presence (vs. absence)
        /// drives [`prose_only_citations_are_the_known_set`] — but it's kept
        /// for anyone debugging a citation change from the REPL/println.
        Prose(#[allow(dead_code)] String),
        /// A `.rs` file token that resolved to no file under any root.
        FileNotFound { token: String },
        /// A file resolved, but the cited symbol was not found inside it.
        SymbolNotFound { token: String, file: PathBuf },
    }

    fn is_identifier(s: &str) -> bool {
        let mut chars = s.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => return false,
        }
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    }

    /// Resolve a `.rs`-bearing token (e.g. `guard.rs`, `runtime/foo.rs`) to a
    /// file under one of [`roots`]: try it as a relative path under each root
    /// first; for a bare filename (no `/`) fall back to a recursive filename
    /// search under each root, in root order.
    fn resolve_file_path(token: &str) -> Option<PathBuf> {
        for root in roots() {
            let candidate = root.join(token);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if !token.contains('/') {
            for root in roots() {
                if !root.is_dir() {
                    continue;
                }
                for entry in walkdir::WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
                    if entry.file_type().is_file() && entry.file_name().to_str() == Some(token) {
                        return Some(entry.path().to_path_buf());
                    }
                }
            }
        }
        None
    }

    /// Resolve a Rust module path like `runtime::guard::tests::test_x` to a
    /// source file + trailing symbol: the last segment is the symbol; the
    /// remaining segments are progressively shortened into a candidate file
    /// (`a/b/c.rs`, `a/b.rs`, `a.rs`, …), each tried under every root, first
    /// hit wins. `None` if nothing resolves (caller treats as prose).
    fn resolve_module_path(path_expr: &str) -> Option<(PathBuf, String)> {
        let segments: Vec<&str> = path_expr.split("::").collect();
        if segments.len() < 2 {
            return None;
        }
        let symbol = (*segments.last().unwrap()).to_string();
        let path_segs = &segments[..segments.len() - 1];
        let mut n = path_segs.len();
        while n >= 1 {
            let joined = path_segs[..n].join("/");
            // Try both file layouts a Rust module can live in:
            // `a/b.rs` and `a/b/mod.rs`.
            for rel in [format!("{joined}.rs"), format!("{joined}/mod.rs")] {
                for root in roots() {
                    let candidate = root.join(&rel);
                    if candidate.is_file() {
                        return Some((candidate, symbol));
                    }
                }
            }
            n -= 1;
        }
        None
    }

    fn file_contains(path: &Path, needle: &str) -> bool {
        std::fs::read_to_string(path)
            .map(|content| content.contains(needle))
            .unwrap_or(false)
    }

    /// Parse one flat (parens-free) span of a citation: `+`/`;`-separated
    /// clauses, each either a single bare-identifier (chains to
    /// `current_file`), a clause containing a `.rs` or `a::b::c` token
    /// (resolved, updating `current_file`), or otherwise prose. `current_file`
    /// threads across clauses *within this span only* — callers reset it per
    /// aside so parenthetical commentary never inherits an outer file.
    fn parse_flat(text: &str, current_file: &mut Option<PathBuf>, out: &mut Vec<Finding>) {
        for clause in text.split(['+', ';']) {
            let clause = clause.trim().trim_matches(',').trim();
            if clause.is_empty() {
                continue;
            }
            // A lone identifier chains to the most recently resolved file.
            if is_identifier(clause) {
                match current_file {
                    Some(file) => {
                        if file_contains(file, clause) {
                            out.push(Finding::Ok);
                        } else {
                            out.push(Finding::SymbolNotFound {
                                token: clause.to_string(),
                                file: file.clone(),
                            });
                        }
                    }
                    None => out.push(Finding::Prose(clause.to_string())),
                }
                continue;
            }
            // Otherwise scan whitespace-separated words in the clause for a
            // `.rs` or `::` token; anything not consumed by one is prose.
            let mut leftover: Vec<&str> = Vec::new();
            for word in clause.split_whitespace() {
                let word = word.trim_matches(|c: char| c == ',' || c == '(' || c == ')');
                if word.is_empty() {
                    continue;
                }
                if word.contains(".rs") {
                    let (file_token, symbol) = match word.split_once("::") {
                        Some((f, s)) => (f, Some(s)),
                        None => (word, None),
                    };
                    match resolve_file_path(file_token) {
                        Some(path) => {
                            *current_file = Some(path.clone());
                            match symbol {
                                Some(sym) if !file_contains(&path, sym) => {
                                    out.push(Finding::SymbolNotFound {
                                        token: word.to_string(),
                                        file: path,
                                    });
                                }
                                _ => out.push(Finding::Ok),
                            }
                        }
                        None => out.push(Finding::FileNotFound { token: word.to_string() }),
                    }
                } else if word.contains("::") {
                    match resolve_module_path(word) {
                        Some((path, sym)) => {
                            *current_file = Some(path.clone());
                            if file_contains(&path, &sym) {
                                out.push(Finding::Ok);
                            } else {
                                out.push(Finding::SymbolNotFound { token: word.to_string(), file: path });
                            }
                        }
                        None => leftover.push(word),
                    }
                } else {
                    leftover.push(word);
                }
            }
            if !leftover.is_empty() {
                out.push(Finding::Prose(leftover.join(" ")));
            }
        }
    }

    /// Split `text` into a parens-free core and its parenthetical asides
    /// (single-level; nested parens are folded into the enclosing aside —
    /// no citation in the register nests parens, so this is not exercised).
    fn extract_asides(text: &str) -> (String, Vec<String>) {
        let mut core = String::new();
        let mut asides = Vec::new();
        let mut depth = 0i32;
        let mut current_aside = String::new();
        for c in text.chars() {
            match c {
                '(' => {
                    if depth == 0 {
                        core.push(' ');
                    } else {
                        current_aside.push(c);
                    }
                    depth += 1;
                }
                ')' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        asides.push(std::mem::take(&mut current_aside));
                    } else {
                        current_aside.push(c);
                    }
                }
                _ => {
                    if depth > 0 {
                        current_aside.push(c);
                    } else {
                        core.push(c);
                    }
                }
            }
        }
        (core, asides)
    }

    /// Parse a whole citation field (`code` or `test`) into findings: the
    /// parens-free core (threading one `current_file`) plus each aside
    /// (each with its own, independent `current_file`).
    fn parse_citation(text: &str) -> Vec<Finding> {
        let (core, asides) = extract_asides(text);
        let mut out = Vec::new();
        let mut current_file: Option<PathBuf> = None;
        parse_flat(&core, &mut current_file, &mut out);
        for aside in &asides {
            let mut aside_file: Option<PathBuf> = None;
            parse_flat(aside, &mut aside_file, &mut out);
        }
        out
    }

    /// Every FILE/MODULE/chained-symbol reference parsed out of a citation
    /// must actually resolve. On failure the message names the clause,
    /// field, offending token, and failure kind — the message a refactorer
    /// who broke a citation needs to fix it.
    #[test]
    fn every_parseable_citation_resolves() {
        for e in enforcement_register() {
            for (field, text) in [("code", e.code), ("test", e.test)] {
                for finding in parse_citation(text) {
                    match finding {
                        Finding::FileNotFound { token } => panic!(
                            "{} entry {} ({}): `{token}` cites a file that does not exist under \
                             autonoetic-gateway/src, autonoetic-gateway/tests, or \
                             autonoetic-types/src — full citation: `{text}`",
                            e.clause_id, e.rule_id, field
                        ),
                        Finding::SymbolNotFound { token, file } => panic!(
                            "{} entry {} ({}): `{token}` not found in {} — full citation: `{text}`",
                            e.clause_id,
                            e.rule_id,
                            field,
                            file.display()
                        ),
                        Finding::Ok | Finding::Prose(_) => {}
                    }
                }
            }
        }
    }

    /// Prose (unverifiable) citation segments, grandfathered: pin the EXACT
    /// set of `(rule_id, field)` pairs whose citation contains at least one
    /// prose fragment today (`rule_id` is unique per entry, so the pin names
    /// the precise entry — e.g. P-7.19's parenthetical aside — rather than
    /// smearing over every entry of its clause). New entries must cite `path.rs::symbol` (or a
    /// resolvable `a::b::c` module path) so they are mechanically verified —
    /// shrinking this list is progress; growing it needs justification in the
    /// PR that adds the new entry.
    ///
    /// `P-5.2` / `P-5.8` were missing from this list despite carrying prose
    /// asides (`(tokio::task_local scope)`, `(gateway-authored repair prompt)`)
    /// — a pre-existing gap that surfaced when regenerating the register doc
    /// alongside the 2026.07.19 amendment. Closing it here is mechanical
    /// reconciliation, not new prose: the entries themselves are unchanged.
    const KNOWN_PROSE_CITATIONS: &[(&str, &str)] = &[
        ("O-1", "code"),
        ("O-2", "code"),
        ("P-5.2", "code"),
        ("P-5.8", "code"),
        ("P-7.19", "code"),
        ("P-8.1", "code"),
        ("Ri-0.11", "code"),
        ("Ri-0.13", "code"),
        ("Ri-0.14", "code"),
        ("Ri-0.17", "code"),
        ("Ri-0.2", "code"),
        ("Ri-0.3", "code"),
    ];

    #[test]
    fn prose_only_citations_are_the_known_set() {
        let mut actual: Vec<(&'static str, &'static str)> = Vec::new();
        for e in enforcement_register() {
            for (field, text) in [("code", e.code), ("test", e.test)] {
                let has_prose = parse_citation(text)
                    .iter()
                    .any(|f| matches!(f, Finding::Prose(_)));
                if has_prose {
                    actual.push((e.rule_id, field));
                }
            }
        }
        actual.sort_unstable();
        actual.dedup();
        let mut expected = KNOWN_PROSE_CITATIONS.to_vec();
        expected.sort_unstable();
        assert_eq!(
            actual, expected,
            "the set of (rule_id, field) citations containing unverifiable prose changed — \
             if a citation became parseable, shrink KNOWN_PROSE_CITATIONS (progress); if a new \
             prose citation was added, that's a deliberate regression needing justification"
        );
    }

    /// A module path whose file lives in the `mod.rs` layout (e.g.
    /// `scheduler/gateway_store/mod.rs`) must resolve, not fall back to
    /// prose (PR #827 review).
    #[test]
    fn module_path_resolves_mod_rs_layout() {
        let (path, symbol) = resolve_module_path("scheduler::gateway_store::GatewayStore")
            .expect("mod.rs-layout module path must resolve");
        assert!(path.ends_with("scheduler/gateway_store/mod.rs"), "{}", path.display());
        assert_eq!(symbol, "GatewayStore");
        assert!(file_contains(&path, &symbol));
    }
}
