# Proposal: the README as a presentation of the constitutional model

> **Archived — complete.** All four PRs shipped: the truth pass, the diagram
> pass and the structure pass in #1311, and the guard pass plus §4.3's Pages-hub
> link in the follow-up. The front page it proposed is live: `README.md` is the
> source of truth for what the page says, and
> `autonoetic-gateway/src/docs_link_guard.rs` for what keeps it true.
>
> Retired rather than left open because its whole subject was a document that had
> drifted, and a proposal about drift that outlives its own delivery is the same
> defect one level up. The lasting content is not the sequencing table but §1
> (what the bind-direction robustification actually changed), §2's defect table
> (how a front page goes wrong: a stale "current version" pointer, a prefix count
> presented as a ratio, and a claim the code had already disproved), §3's framing
> of one job per section, and §4.1's diagnosis of why the mermaid plate read as
> noise. Those are the parts worth reading before rewriting a front page again.
>
> Two things it got wrong in flight, recorded rather than smoothed over: the
> "~180 lines" target (the result is 428, and §3 says why that was the wrong
> constraint to hold), and the "features you may not find elsewhere" framing,
> which was untrue — almost every item exists elsewhere; what is peculiar is
> *how* it is held.

Shipped: 2026-09-04, via #1311 and its follow-up.
Scope: `README.md`, the three generated plates it leads with and the four
published HTML maps, the bind-direction section in
[`../concepts/philosophy.md`](../concepts/philosophy.md), and one guard
extension. No code behaviour, no clause statement, no amendment.

> **Why now.** Four PRs (#1293, #1298, #1302, #1304, #1306) retired the model
> the README uses to explain the constitution. The front page now states, as
> the design's structural novelty, the prefix convention the RFC dismantled —
> and the version of the claim that *is* true is stronger than the one printed.
> The presentation debt and the correctness debt are the same debt, which is
> why this is one proposal rather than a styling pass.

---

## 1. What the robustification changed

The mechanical change is small to state: bind direction stopped being inferred
from a clause's ID prefix and became declared data in
`autonoetic-gateway/src/constitution_relations.rs`. The old `binds()` read the
prefix — `P-*` → agent, `Ri-*` → gateway, `O-*` → decider — and a test pinned
that inference, so the falsehood was asserted rather than drifted into. Three
examples from the RFC's own evidence table
([`constitution-bind-direction-model.md`](../proposals/constitution-bind-direction-model.md) §1.1):
`binds("P-8.1")` reported that the *agent* keeps the causal chain append-only;
`binds("P-3.1")` that the agent passes `--unshare-all`; `binds("P-15.1")` that
the agent withholds labelled envelopes — which `I-14` forbids the agent from so
much as reading.

Six concepts came out of the classification sharper than they went in. Four of
them contradict what the README says today.

| Concept | What the README says | What the classification established |
|---|---|---|
| The relational fields | one party, read off the prefix | three declared fields: `binds` (a **power**), `owed_to` (a **principal**, a seat, or nobody), and verification |
| What a right *is* | the `Ri-*` family — 18 clauses | a **view**, not a family: any enforcer duty owed to the agent. **27** clauses qualify, 10 of them under another prefix |
| Integrity properties | absent — gateway duties wear an "agent" prefix | `owed_to: none`, **81** clauses. Nobody can *claim* their own sandbox confinement, and saying so beats inventing a pseudo-party to fill the slot |
| Who the law mostly binds | "rules bind the agent" | of 124 classified clauses: **118 enforcer, 5 decider, 1 reasoner** |
| The served party | `U-*` binds "the community" | binds the **enforcer**, owed to `served_user`. "Community" is an aggregate, and §0 says every clause binds exactly one party |
| Law vs conformance | one register | two surfaces: [`law-table.md`](../constitution/law-table.md) (what a clause obliges, of whom, to whom — identical for any implementation) and [`enforcement-register.md`](../constitution/enforcement-register.md) (which of *our* code sites hold it up) |

Three further precisions belong in front of a reader, because each is load-bearing
for a claim the README already makes and cannot currently support:

- **Obligations attach to seats; standing attaches to principals.** That is why
  `binds` ranges over `reasoner` / `enforcer` / `decider` rather than over humans
  and agents: a clause binds a *function*, so "can an agent hold decider
  authority?" (`P-2.20`) stopped being a special case instead of needing one.
  `Ri-0.15` turned out to be owed to the **seat** — decision context is owed to
  whoever decides the gate, human or agent — the one case of seat-standing in the
  document, and unrecordable before `OwedTo` existed.
- **There is no judiciary, by design.** Interpretation is abolished in favour of
  determinism plus amendment; where the text is insufficient the remedies are
  rejection and amendment, and improvisation is recorded as DISCRETION LEAK debt.
  This is what makes the law portable: decidable rules transfer between
  implementations, jurisprudence does not.
- **The bar is re-implementability, not documentation.** The constitution is meant
  to be the specification from which a compatible gateway could be rebuilt in
  another language, such that two implementations verify shared law through the
  `P-10.9` digest handshake and federate. That reframes the document from *our
  safety configuration* to *a specification* — the strongest claim available on
  the front page, and one the README does not currently make.

### 1.1 The numbers, and the one sentence to lead with

Verified against the signed text of `2026.09.02` and the generated
[`law-table.md`](../constitution/law-table.md):

| | count |
|---|---|
| Clauses, five families (`Ri-*` 18, `P-*` 182, `O-*` 4, `U-*` 3, `I-*` 14) | 221 |
| Classified bind direction | 124 — the outstanding 97 are ratchet-pinned, exact equality |
| `requires` declared (#1307) | 40 — 25 `preventive`, 6 `detective`, 9 **both** (all split candidates, §2.4.3) |
| Resolved by inheriting a section summary | 0 |
| `binds`: enforcer / decider / reasoner | 118 / 5 / 1 |
| `owed_to`: agent / served party / decider seat / nobody | 32 / 6 / 5 / 81 |
| Agent rights by relation (enforcer duties owed to the agent) | 27 |
| Status of the 207 clauses carried in tables — `ENFORCED` / `PARTIAL` / `DESIGN DEBT` / `MISSING` | 201 / 2 / 1 / 3 |

The 14 `I-*` invariants are bullets rather than table rows; their status is
carried inline (`I-3` is `PARTIAL — named gap`), which is why the status row above
counts 207 and not 221.

And the sentence that does more for the project's credibility than the whole
current *Status* section:

> Six clauses are owed to the served party. The three that are enforced are the
> egress plane (`P-15.1`–`P-15.3`). The three that name them as a party — refuse
> a result, obtain an account, take your data (`U-1`–`U-3`) — are `MISSING`, and
> the constitution says so in its own vocabulary.

---

## 2. Defects in the current README, with evidence

| Where | Claim | Reality | Fix |
|---|---|---|---|
| `README.md:214` | `versions/2026.07.30/constitution.md` is "the canonical law (current version)" | `CURRENT` is `2026.09.02` — two amendments behind | Link [`CURRENT`](../constitution/CURRENT) beside the versioned path, so the pointer cannot rot again |
| `README.md:372` | "current constitution (`2026.08.30`)" | one version behind. The counts (18 rights, 182 rules, 179 enforced) are still exactly right | Update the version; replace the prefix counts with §1.1's table, which the law table generates |
| `README.md` § *A constitution, not a config file* | bind-direction discipline = `P-*` bind the agent, `Ri-*` bind the gateway | true of the prefixes, false of the law. 118 of 124 classified clauses bind the enforcer | State the declared model; it is the stronger claim |
| [`../concepts/philosophy.md`](../concepts/philosophy.md) §2 | same, plus "`U-*` binds the *community*" | resolved: `U-*` bind the enforcer, owed to the served party | Same edit, same PR — the README links this as the deep version |
| README pointers | the register is cited, the law table is not | the law table is the surface a re-implementer reads | Add it, with what distinguishes the two |
| `README.md:43`, `README.md:334` | egress plane "constitution v2026.07.30" | **correct** — §15 was enacted there | Keep; reword to "enacted in" so it reads as provenance, not as the current version |

**One guard gap, worth closing in the same work.** `docs_link_guard` checks
printed clause IDs on `.svg` / `.html` under `docs/` only
(`every_clause_id_in_a_diagram_resolves`); markdown is scanned for paths, links,
anchors and symbols, but not for clause IDs. So every clause the README cites is
unguarded — the same defect class that let a fabricated `U-4` marked "enforced"
survive in a pedagogical map. A README that leads with the constitutional model
should be inside that check.

---

## 3. The structural problem: 422 lines doing six jobs

The file is a manifesto, a comparison table, a documentation index, an agent
roster, a quickstart and a changelog. Two of those belong on a front page.

There is also a genuine duplication: the opening four paragraphs argue
community-under-law, the gateway-as-Lawful-Executor and trust-is-structural, and
*Why this exists* then argues the same three points again at greater length. The
prose is good in both passes, which is why nobody deleted either.

### The shape, as applied (428 lines)

1. **Hero** — the one-sentence claim, the status disclaimer, the peer plate (§4).
2. **The bet** — the thesis plate and three paragraphs: what is mechanical,
   what is wagered, and how the wager could be lost.
3. **The idea in three moves** — self-knowledge is a runtime service · one law
   binds both sides · trust is structural. The existing prose kept, the
   duplicate pass deleted.
4. **The loop, once** — the mechanism plate and one paragraph.
5. **The constitution in one screen** — the declared model, the two reader
   surfaces, the numbers. Drafted below.
6. **Four ways to read the system** — the four published HTML maps keep a
   section of their own, one row each: they are a *reading path*, not a
   catalogue entry, and collapsing them into a single pointer (an earlier draft
   of this rewrite did) loses the only place a reader learns that the runtime
   is documented at four distinct altitudes.
7. **What is done differently here** — one numbered list, ordered by how much
   the rest of the design leans on each item, replacing *both* tables an
   earlier draft had. The framing matters and was got wrong twice: a
   "features you may not find elsewhere" heading is false — almost every item
   exists somewhere else — and a two-column harness comparison invites a
   feature race. What is actually peculiar is *how* each property is held, and
   in nearly every case the answer is the same shape: it is made mechanical
   instead of conventional. So each entry names the property, its **maturity**,
   and the mechanism that makes it unusual. The maturity vocabulary is defined
   inline (*mature · shipped · partial · experimental · declared*) and is the
   load-bearing part — a feature list without one, on a project this early, is
   a promise. Item 5 carries a nested per-driver table, because "sandboxed" is
   not one claim: `guarantees_network_off` is answered per driver and fails
   closed, and only two of the four tiers can promise it.
8. **Try it** — quickstart, ten lines.
9. **The nouns you will meet** — six terms, so the vocabulary is not a lookup.
10. **What is not built yet** — the frontier only. The "runtime core is
    implemented and self-hosting" inventory came out, because the maturity
    annotations in §7 now say it per item.
11. **Where to go next** — five pointers. The agent roster, the documentation
    index and the archived-examples notes moved to
    [`../README.md`](../README.md) and `examples/archived/README.md`, which are
    the files that exist to hold them.
12. **Lineage**, **License**.

**On length.** The target was ~180 lines and the result is 428. The reduction
was real — the roster, the documentation catalogue and the duplicated argument
came out — but §7's list put more back than the tables it replaced, on purpose:
it is the page's substance now, and it is scannable rather than dense. The
claim this proposal made was never "shorter"; it was "one job per section, and
nothing on the page that is untrue". Both hold. If length becomes the binding
constraint, §7 is the section to split into its own doc, not to compress.

### Draft of §3, the part this proposal exists to specify

> **One law, two directions.** Every clause binds exactly one party and is owed
> to at most one. Bind direction is **declared data**, not a naming convention —
> which is what makes the law re-implementable, and what makes "a right" a
> *relation* rather than a family of IDs.
>
> | | binds | owed to | clauses |
> |---|---|---|---|
> | **Agent rights** | `enforcer` | the agent | 27 — *17 of the 18 `Ri-` rights, plus 10 clauses filed under another prefix* |
> | **Integrity properties** | `enforcer` | nobody | 81 — *an agent cannot demand its own confinement* |
> | **Decider obligations** | `decider` | the agent | 5 |
> | **The served party's charter** | `enforcer` | the served user | 6 — *3 of them `MISSING`* |
> | **Agent rules** | `reasoner` | nobody | 1 classified; 97 clauses await their tranche |
>
> The last row is the honest one: the classification is not finished, a test
> pins the exact remainder, and no clause resolves its bind direction by
> inheriting a section summary. What each clause obliges, of whom, to whom lives
> in [`law-table.md`](../constitution/law-table.md); which code holds it up lives
> in [`enforcement-register.md`](../constitution/enforcement-register.md). A
> second implementation inherits the first and writes its own second.

Note what the table does that prose cannot: the reader sees in one glance that
the enforcer is the primary bound party, that the largest single category is a
duty owed to nobody, and that the front page is willing to print an unfinished
number.

---

## 4. The diagrams

### 4.1 Why the current hero reads as noise

The mermaid plate is 9 nodes, 2 nested subgraphs and 11 edges attempting two
pictures at once — a dataflow diagram and the social contract. Specifically:

- the four dashed `binds` edges express a different *kind* of relation than the
  solid dataflow edges, but render nearly identically;
- one edge label is a full paragraph ("verified self-model, every turn: past ·
  present · rights · budget · identity");
- nested subgraphs render as boxes-in-boxes, which reads as hierarchy that is not
  claimed;
- the `classDef` fills are light-mode values, so the plate washes out on
  GitHub's dark theme, which is what most readers are using.

The result is neither picture. A front-page diagram gets one claim.

### 4.2 Three pictures, one job each

**The hero: the peer plate.**
[`diagrams/human-ai-peers.svg`](../proposals/diagrams/human-ai-peers.svg) is the right
composition — one law over both parties, both figures dimensioned to the same
height, the four one-way bindings as arrows, the correction cycle as a footer
strip. It commits to a single claim and drops mechanism entirely, which is
exactly what the top of a README needs. Four corrections before it can carry the
page:

1. **Split the served party out.** The left figure is currently both "human
   citizen (operator · decider · auditor seats)" *and* "served party, owed
   `U-1`–`U-3`". The classification established these are different parties: the
   served party has **no seat**, which is precisely why implementing `U-1`
   requires giving it one (#1274). Drawing them as a third, seatless figure
   standing *outside* the session frame — the party the whole arrangement exists
   to serve, with no door of its own yet — is both more accurate and more
   evocative than the merge. The constitution's own hedge ("today the operator
   and the served user are usually the same person") becomes the caption.
2. **Say `MISSING`, not "declared".** The gateway → human `ACCOUNTS (U-*)` arrow
   is labelled "declared". All three clauses are `MISSING`. The diagram's best
   moment is the one where it admits that.
3. **Label arrows by act; put families in the captions.** `PROPOSES (P-*)` no
   longer holds — 1 of 124 classified clauses binds the reasoner. Arrows:
   *proposes* / *validates and attests* / *decides* / *accounts*. The clause
   families move to the caption blocks, where the law table backs them.
4. **Ship a light variant behind `<picture>`** with `prefers-color-scheme`, or
   the plate is a dark hole for half the readers.

Do **not** use `human-ai-peers.jpg` on the front page: its seal text is garbled
("SYMME TRIC DUTIES", "BREUME DUTIES"), which is exactly what a reader zooms in
on. The hand-authored SVG is the asset.

**The second picture: the loop, and only the loop.** Keep one mechanism diagram,
reduced to what it actually claims: actor → typed intent → validate → sandbox →
chain → signed attestation → actor, with the law as a **frame** around the whole
rather than a node with dashed edges to everything else. Five nodes, six edges,
no subgraphs, no sentence labels. Authoring it as an SVG rather than
mermaid gives the plates a shared visual language and puts the clause IDs inside
the guard that already checks them.

**The third picture: the bet.** Added after review — the hero states *what the
arrangement is*, the loop states *how it runs*, and neither states *why anyone
should expect it to work*. `the-bet.svg` carries the thesis, and it is drawn as
an argument rather than a description: what the runtime supplies (left,
mechanical and cited), what is claimed to follow (right), and between them an
inference labelled a **wager** rather than a result. A fourth band names how
the bet could be lost — a truthful self-model the agent ignores, a right that
exists in text and not in tests, a served party owed clauses nobody enforces,
an enforcer whose own lapses stop being counted — with the register, contract
health and the discretion-leak ledger named as the instruments that would show
it. A plate that stated only the upside would be marketing, and this project's
front page cannot afford that: its whole argument is that the gap between text
and enforcement is a measured quantity.

### 4.3 Asset placement

If these plates become README-load-bearing they should not live under
`proposals/` — a hero sourced from a proposals folder reads as provisional. Move
the two into [`../diagrams`](../diagrams) alongside the four published maps, and
add [`diagrams/community-and-constitution.html`](../proposals/diagrams/community-and-constitution.html)
to the Pages hub ([`../index.html`](../index.html)): it is an interactive clause
catalogue with a searchable bind-direction map, and nothing links to it from the
front door.

---

## 5. Sequencing

Four PRs, each independently shippable, deliberately smallest-first so the
factual errors are gone before anyone argues about layout.

| # | PR | Contents | State |
|---|---|---|---|
| 1 | **Truth pass** | the `CURRENT` link, the version label, the bind-direction section in `README.md` and [`../concepts/philosophy.md`](../concepts/philosophy.md) §2, the law table added to the pointers | **applied** |
| 2 | **Diagram pass** | the peer plate corrected and redrawn, the loop plate, the bet plate, each with a generated light variant; the mermaid block deleted | **applied** — [`../diagrams/generate.py`](../diagrams/generate.py) writes all six SVGs |
| 3 | **Structure pass** | the section rewrite; the roster and doc index replaced by pointers, the archived-examples note preserved as `examples/archived/README.md`, the four visual maps kept as their own section | **applied** |
| 4 | **Guard pass** | extend the clause-ID check to markdown so the front page cannot drift | **open** — the plates are guarded, the prose is not |

Applied means "in the working tree and passing the doc guards", not merged.
Two things named above are deliberately still open: PR 4, and §4.3's link from
the Pages hub ([`../index.html`](../index.html)) to the interactive clause
catalogue.

**Why the plates are generated.** Each has a light and a dark variant, and a
colour with two definitions drifts. [`../diagrams/generate.py`](../diagrams/generate.py)
holds one token palette per theme and emits all six files, so the light
variant is never hand-maintained. The clause IDs printed on them are checked
against the active constitution by `every_clause_id_in_a_diagram_resolves` —
negative-tested on the new plates by planting `Ri-0.99`, which fails with the
file, line and offending ID.

---

## 6. What this proposal does not propose

- No clause statement, no amendment, no change to the signed text. The §2.4.1
  reshaping of the verification field ([`constitution-bind-direction-model.md`](../proposals/constitution-bind-direction-model.md))
  is the amendment-adjacent work, and it is not this.
- No new diagram *concepts* — the compositions exist; this corrects and promotes
  them.
- Not the launch messaging. [`launch-presentation.md`](../proposals/launch-presentation.md)
  owns the pitch, the demo and the rollout; this owns the front page's accuracy
  and shape. Where they overlap, the one-liner should be the same sentence in
  both, and this proposal defers to that one.
