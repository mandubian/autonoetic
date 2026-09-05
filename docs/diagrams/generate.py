#!/usr/bin/env python3
"""Generate the three README plates, dark + light, from one token palette.

Run from the workspace root:

    python3 docs/diagrams/generate.py

Writes `peers-under-one-law{,-light}.svg`, `correction-loop{,-light}.svg` and
`the-bet{,-light}.svg` into this directory — six files. The light variants are
generated rather than hand-maintained, so a colour only ever has one definition
per theme. Adding a plate means adding its template and one row in the loop at
the bottom; keep this list in step with that loop.
"""
import pathlib

DARK = dict(
    BG="#0d1117", FRAME="#21262d", GUIDE="#30363d", DIMLINE="#484f58",
    CARD="#161b22", CARDS="#30363d",
    TXT="#f0f6fc", SEC="#8b949e", DIM="#6e7681", BODY="#c9d1d9",
    GOLD="#d29922", GOLDBG="#1a1610",
    ROSE="#e3919b", ROSEBG="#1b1417",
    BLUE="#58a6ff", BLUEBG="#101c2c",
    GREY="#9198a1", GREYBG="#12161c",
    PURPLE="#bc8cff", PURPLEBG="#16121f", PURPLECHIP="#1d1729",
    PURPLECHIPS="#3a2f52", PURPLETXT="#c9a9f5",
    TEAL="#5cc7b6", TEALBG="#0f1e1c",
)

LIGHT = dict(
    BG="#ffffff", FRAME="#d8dee4", GUIDE="#d0d7de", DIMLINE="#8c959f",
    CARD="#f6f8fa", CARDS="#d0d7de",
    TXT="#1f2328", SEC="#59636e", DIM="#6e7781", BODY="#24292f",
    GOLD="#9a6700", GOLDBG="#fff8c5",
    ROSE="#a40e26", ROSEBG="#fff0ee",
    BLUE="#0969da", BLUEBG="#eef7ff",
    GREY="#59636e", GREYBG="#f6f8fa",
    PURPLE="#6639ba", PURPLEBG="#f7f0ff", PURPLECHIP="#faf5ff",
    PURPLECHIPS="#d8c0f5", PURPLETXT="#6639ba",
    TEAL="#0f6e5e", TEALBG="#eefbf8",
)

STYLE = """
      .bg       {{ fill: {BG}; }}
      .frame    {{ fill: none; stroke: {FRAME}; stroke-width: 1; }}
      .card     {{ fill: {CARD}; stroke: {CARDS}; stroke-width: 1.5; }}
      .card-law {{ fill: {GOLDBG}; stroke: {GOLD}; stroke-width: 1.6; }}
      .frame-law{{ fill: none; stroke: {GOLD}; stroke-width: 1.6; }}
      .card-gw  {{ fill: {GREYBG}; stroke: {GREY}; stroke-width: 1.6; }}
      .card-evo {{ fill: {PURPLEBG}; stroke: {PURPLE}; stroke-width: 1.5; }}
      .card-srv {{ fill: {TEALBG}; stroke: {TEAL}; stroke-width: 1.5; stroke-dasharray: 7 4; }}
      .chip     {{ fill: {PURPLECHIP}; stroke: {PURPLECHIPS}; stroke-width: 1; }}

      .h1  {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; font-size: 20px; font-weight: 700; fill: {TXT}; }}
      .h2  {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; font-size: 15px; font-weight: 600; fill: {TXT}; }}
      .h3  {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; font-size: 13px; font-weight: 700; fill: {TXT}; }}
      .dek {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif; font-size: 12px; fill: {SEC}; }}
      .mono{{ font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace; font-size: 10.5px; fill: {BODY}; }}
      .tag {{ font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace; font-size: 9px; font-weight: 700; letter-spacing: 0.10em; }}
      .dim {{ font-family: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace; font-size: 8.5px; letter-spacing: 0.06em; fill: {DIM}; }}

      .fig-hum {{ fill: none; stroke: {ROSE}; stroke-width: 1.6; stroke-linecap: round; stroke-linejoin: round; }}
      .fig-ai  {{ fill: none; stroke: {BLUE}; stroke-width: 1.6; stroke-linecap: round; stroke-linejoin: round; }}

      .dimline {{ stroke: {DIMLINE}; stroke-width: 0.9; }}
      .guide   {{ fill: none; stroke: {GUIDE}; stroke-width: 1; stroke-dasharray: 6 4; }}

      .e-hum {{ fill: none; stroke: {ROSE}; stroke-width: 1.5; }}
      .e-ai  {{ fill: none; stroke: {BLUE}; stroke-width: 1.5; }}
      .e-law {{ fill: none; stroke: {GOLD}; stroke-width: 1.5; }}
      .e-evo {{ fill: none; stroke: {PURPLE}; stroke-width: 1.4; }}
      .e-srv {{ fill: none; stroke: {TEAL}; stroke-width: 1.4; stroke-dasharray: 6 4; }}
      .e-run {{ fill: none; stroke: {GREY}; stroke-width: 1.5; }}
"""

MARKERS = """
    <marker id="m-hum" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="5.5" markerHeight="5.5" orient="auto"><path d="M 0 1 L 10 5 L 0 9 z" fill="{ROSE}" /></marker>
    <marker id="m-ai"  viewBox="0 0 10 10" refX="8" refY="5" markerWidth="5.5" markerHeight="5.5" orient="auto"><path d="M 0 1 L 10 5 L 0 9 z" fill="{BLUE}" /></marker>
    <marker id="m-law" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="5.5" markerHeight="5.5" orient="auto"><path d="M 0 1 L 10 5 L 0 9 z" fill="{GOLD}" /></marker>
    <marker id="m-evo" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="5.5" markerHeight="5.5" orient="auto"><path d="M 0 1 L 10 5 L 0 9 z" fill="{PURPLE}" /></marker>
    <marker id="m-srv" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="5.5" markerHeight="5.5" orient="auto"><path d="M 0 1 L 10 5 L 0 9 z" fill="{TEAL}" /></marker>
    <marker id="m-run" viewBox="0 0 10 10" refX="8" refY="5" markerWidth="5.5" markerHeight="5.5" orient="auto"><path d="M 0 1 L 10 5 L 0 9 z" fill="{GREY}" /></marker>
"""

PEERS = """<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1000 720" width="100%" height="100%" role="img" aria-label="One law over humans and AI agents: the constitution above, three bound powers inside the frame, and the served party outside it, owed but never bound.">
  <!--
    Autonoetic - One law over humans and AI agents.

    GENERATED. Edit the generator, not this file:
      docs/diagrams/generate.py  (writes both the dark and light variants)

    The claims this plate makes, and does not overstate:
      - ONE law over every party (the seal, Rule Zero).
      - EQUAL STANDING, not identical rights: the two figures are dimensioned
        to the same height because a clause binds a *seat*, not a species
        (P-2.20). Each party is bound by a different clause family - that
        asymmetry is the design, and it is what produces symmetry of standing.
      - INSIDE the frame are the parties that can be BOUND, because a seat
        exists through which to oblige them. The served party sits OUTSIDE it:
        owed, never bound, because no seat is theirs yet.
      - A community that CORRECTS ITSELF (the footer cycle).

    Every clause ID printed here is checked against the active constitution by
    docs_link_guard::tests::every_clause_id_in_a_diagram_resolves.
  -->
  <defs>
    <style>{STYLE}</style>{MARKERS}
  </defs>

  <rect width="100%" height="100%" class="bg" />

  <rect x="14" y="14" width="972" height="692" class="frame" />
  <path d="M 14 44 L 44 44 M 44 14 L 44 44" class="frame" />
  <path d="M 986 44 L 956 44 M 956 14 L 956 44" class="frame" />
  <path d="M 14 676 L 44 676 M 44 706 L 44 676" class="frame" />
  <path d="M 986 676 L 956 676 M 956 706 L 956 676" class="frame" />

  <text x="500" y="48" text-anchor="middle" class="h1">One law over humans and AI agents</text>
  <text x="500" y="70" text-anchor="middle" class="dek">Every clause binds exactly one power &#183; equal standing, not identical rights &#183; a community that corrects itself</text>

  <circle cx="500" cy="150" r="64" fill="{GOLDBG}" stroke="{GOLD}" stroke-width="2" />
  <circle cx="500" cy="150" r="55" fill="none" stroke="{GOLD}" stroke-width="0.8" opacity="0.5" />
  <text x="500" y="138" text-anchor="middle" class="tag" fill="{GOLD}">THE CONSTITUTION</text>
  <line x1="470" y1="145" x2="530" y2="145" stroke="{GOLD}" stroke-width="0.7" opacity="0.5" />
  <text x="500" y="163" text-anchor="middle" class="h3">RULE ZERO</text>
  <text x="500" y="182" text-anchor="middle" class="dim">SIGNED &#183; P-10.9</text>

  <rect x="306" y="224" width="388" height="24" rx="4" class="card-law" />
  <text x="500" y="240" text-anchor="middle" class="tag" fill="{GOLD}">EQUAL STANDING&#160;&#160;&#183;&#160;&#160;SYMMETRIC DUTIES&#160;&#160;&#183;&#160;&#160;ONE LAW</text>

  <path d="M 452 192 L 300 226 L 240 288" class="e-law" marker-end="url(#m-law)" />
  <path d="M 548 192 L 700 226 L 760 288" class="e-law" marker-end="url(#m-law)" />
  <path d="M 500 212 L 500 224" class="e-law" />
  <path d="M 500 248 L 500 324" class="e-law" marker-end="url(#m-law)" />

  <rect x="52" y="268" width="896" height="252" rx="8" class="guide" />
  <text x="68" y="288" class="tag" fill="{SEC}">BOUND PARTIES</text>

  <path d="M 76 322 L 76 468 M 68 322 L 84 322 M 68 468 L 84 468" class="dimline" />
  <path d="M 76 322 L 73 330 M 76 322 L 79 330 M 76 468 L 73 460 M 76 468 L 79 460" class="dimline" />
  <text x="64" y="395" text-anchor="middle" class="dim" transform="rotate(-90 64 395)">PEER LEVEL</text>
  <path d="M 924 322 L 924 468 M 916 322 L 932 322 M 916 468 L 932 468" class="dimline" />
  <path d="M 924 322 L 921 330 M 924 322 L 927 330 M 924 468 L 921 460 M 924 468 L 927 460" class="dimline" />
  <text x="936" y="395" text-anchor="middle" class="dim" transform="rotate(-90 936 395)">PEER LEVEL</text>

  <text x="190" y="312" text-anchor="middle" class="tag" fill="{ROSE}">HUMAN</text>
  <g class="fig-hum">
    <circle cx="190" cy="336" r="13" />
    <path d="M 190 349 L 190 358" />
    <path d="M 172 358 L 208 358 L 206 412 L 174 412 Z" />
    <path d="M 172 361 L 156 400" />
    <path d="M 208 361 L 224 400" />
    <path d="M 181 412 L 178 466" />
    <path d="M 199 412 L 202 466" />
    <path d="M 170 467 L 185 467" />
    <path d="M 195 467 L 210 467" />
  </g>
  <text x="200" y="488" text-anchor="middle" class="mono">seats: operator &#183; decider &#183; auditor</text>
  <text x="200" y="503" text-anchor="middle" class="dim">O-1 &#183; O-2 BIND WHOEVER DECIDES</text>

  <text x="810" y="312" text-anchor="middle" class="tag" fill="{BLUE}">AI AGENT</text>
  <g class="fig-ai">
    <rect x="794" y="322" width="32" height="32" rx="6" />
    <circle cx="803" cy="338" r="2.5" />
    <circle cx="817" cy="338" r="2.5" />
    <path d="M 810 354 L 810 361" />
    <rect x="782" y="361" width="56" height="57" rx="3" />
    <path d="M 789 381 L 831 381" opacity="0.5" />
    <path d="M 789 399 L 831 399" opacity="0.5" />
    <path d="M 782 369 L 765 404" />
    <path d="M 838 369 L 855 404" />
    <path d="M 795 418 L 792 466" />
    <path d="M 825 418 L 828 466" />
    <path d="M 784 467 L 799 467" />
    <path d="M 821 467 L 836 467" />
  </g>
  <text x="800" y="488" text-anchor="middle" class="mono">seats: reasoner &#183; decider &#183; auditor</text>
  <text x="800" y="503" text-anchor="middle" class="dim">38 RIGHTS OWED BY RELATION</text>

  <rect x="398" y="330" width="204" height="138" rx="6" class="card-gw" />
  <text x="500" y="352" text-anchor="middle" class="h3">Gateway</text>
  <text x="500" y="368" text-anchor="middle" class="tag" fill="{GREY}">LAWFUL EXECUTOR</text>
  <line x1="412" y1="378" x2="588" y2="378" stroke="{CARDS}" stroke-width="1" />
  <text x="500" y="398" text-anchor="middle" class="mono" fill="{SEC}">Deterministic enforcement,</text>
  <text x="500" y="412" text-anchor="middle" class="mono" fill="{SEC}">no improvised judgment.</text>
  <text x="500" y="436" text-anchor="middle" class="mono">118 of 124 classified</text>
  <text x="500" y="450" text-anchor="middle" class="mono">clauses bind it.</text>

  <text x="312" y="368" text-anchor="middle" class="tag" fill="{ROSE}">DECIDES</text>
  <text x="312" y="379" text-anchor="middle" class="dim">AND OWES A MOTIVATION</text>
  <path d="M 233 386 L 392 386" class="e-hum" marker-end="url(#m-hum)" />
  <path d="M 392 430 L 233 430" class="e-hum" marker-end="url(#m-hum)" />
  <text x="312" y="446" text-anchor="middle" class="tag" fill="{ROSE}">GIVES CONTEXT</text>
  <text x="312" y="457" text-anchor="middle" class="dim">Ri-0.15 &#183; OWED TO THE SEAT</text>

  <text x="688" y="368" text-anchor="middle" class="tag" fill="{BLUE}">PROPOSES</text>
  <text x="688" y="379" text-anchor="middle" class="dim">A TYPED INTENT, NEVER AN ACT</text>
  <path d="M 767 386 L 608 386" class="e-ai" marker-end="url(#m-ai)" />
  <path d="M 608 430 L 767 430" class="e-ai" marker-end="url(#m-ai)" />
  <text x="688" y="446" text-anchor="middle" class="tag" fill="{BLUE}">ATTESTS</text>
  <text x="688" y="457" text-anchor="middle" class="dim">Ri-0.1 &#183; Ri-0.11 &#183; EVERY TURN</text>

  <path d="M 500 548 L 500 474" class="e-srv" marker-end="url(#m-srv)" />
  <text x="512" y="536" class="tag" fill="{TEAL}">ACCOUNTS&#160;&#183;&#160;U-2&#160;&#183;&#160;MISSING</text>
  <rect x="52" y="548" width="896" height="76" rx="8" class="card-srv" />
  <text x="70" y="570" class="tag" fill="{TEAL}">OWED, NEVER BOUND</text>
  <text x="70" y="590" class="mono">The served party &#8212; the end-user a session runs on behalf of.</text>
  <text x="930" y="590" text-anchor="end" class="mono" fill="{SEC}">P-15.1&#8211;P-15.3 enforced &#183; U-1 &#183; U-2 &#183; U-3 MISSING</text>
  <text x="70" y="608" class="dim">NO SEAT IS THEIRS YET &#8212; WHICH IS WHY NOTHING CAN OBLIGE THEM, AND WHY ENFORCING U-1 MEANS CREATING ONE</text>

  <rect x="52" y="644" width="896" height="58" rx="8" class="card-evo" />
  <text x="68" y="682" class="h3" fill="{PURPLE}">The community corrects itself</text>
  <rect x="290" y="664" width="110" height="28" rx="3" class="chip" />
  <text x="345" y="682" text-anchor="middle" class="tag" fill="{PURPLETXT}">PROPOSE Ri-0.8</text>
  <rect x="423" y="664" width="110" height="28" rx="3" class="chip" />
  <text x="478" y="682" text-anchor="middle" class="tag" fill="{PURPLETXT}">ADJUDICATE O-1</text>
  <rect x="556" y="664" width="110" height="28" rx="3" class="chip" />
  <text x="611" y="682" text-anchor="middle" class="tag" fill="{PURPLETXT}">RECORD P-8.1</text>
  <rect x="689" y="664" width="110" height="28" rx="3" class="chip" />
  <text x="744" y="682" text-anchor="middle" class="tag" fill="{PURPLETXT}">PROMOTE P-9.15</text>
  <rect x="822" y="664" width="110" height="28" rx="3" class="chip" />
  <text x="877" y="682" text-anchor="middle" class="tag" fill="{PURPLETXT}">AMEND P-10.9</text>
  <path d="M 402 678 L 419 678" class="e-evo" marker-end="url(#m-evo)" />
  <path d="M 535 678 L 552 678" class="e-evo" marker-end="url(#m-evo)" />
  <path d="M 668 678 L 685 678" class="e-evo" marker-end="url(#m-evo)" />
  <path d="M 801 678 L 818 678" class="e-evo" marker-end="url(#m-evo)" />
</svg>
"""

LOOP = """<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1000 380" width="100%" height="100%" role="img" aria-label="The correction loop: an actor proposes a typed intent, the gateway validates it, executes it in a sandbox, records it, and attests a verified self-model back - with the constitution as the frame both sides sit inside.">
  <!--
    Autonoetic - the correction loop, once.

    GENERATED. Edit the generator, not this file:
      docs/diagrams/generate.py  (writes both the dark and light variants)

    Deliberately five nodes and six edges. The law is the FRAME rather than a
    node with dashed edges to everything, because "both parties are inside it"
    is the claim - not "the law is connected to things".

    Every clause ID printed here is checked against the active constitution by
    docs_link_guard::tests::every_clause_id_in_a_diagram_resolves.
  -->
  <defs>
    <style>{STYLE}</style>{MARKERS}
  </defs>

  <rect width="100%" height="100%" class="bg" />

  <text x="500" y="36" text-anchor="middle" class="h2">One correction loop, under a law that binds both sides</text>

  <rect x="24" y="56" width="952" height="300" rx="8" class="frame-law" />
  <text x="44" y="80" class="tag" fill="{GOLD}">THE CONSTITUTION&#160;&#160;&#183;&#160;&#160;SIGNED&#160;&#160;&#183;&#160;&#160;DIGEST-PINNED</text>
  <text x="956" y="344" text-anchor="end" class="dim">THE ACTOR AND THE EXECUTOR ARE BOTH INSIDE THE FRAME</text>

  <rect x="56" y="150" width="150" height="100" rx="6" class="card" />
  <text x="131" y="178" text-anchor="middle" class="h3">Actor</text>
  <text x="131" y="194" text-anchor="middle" class="tag" fill="{SEC}">AI &#183; HUMAN &#183; SCRIPT</text>
  <text x="131" y="216" text-anchor="middle" class="mono" fill="{SEC}">a low-privilege</text>
  <text x="131" y="231" text-anchor="middle" class="mono" fill="{SEC}">reasoner</text>

  <rect x="252" y="150" width="150" height="100" rx="6" class="card" />
  <text x="327" y="178" text-anchor="middle" class="h3">Validate</text>
  <text x="327" y="194" text-anchor="middle" class="tag" fill="{SEC}">POLICY &#183; CAPABILITIES</text>
  <text x="327" y="216" text-anchor="middle" class="mono" fill="{SEC}">the intent against</text>
  <text x="327" y="231" text-anchor="middle" class="mono" fill="{SEC}">what was declared</text>

  <rect x="432" y="150" width="150" height="100" rx="6" class="card" />
  <text x="507" y="178" text-anchor="middle" class="h3">Execute</text>
  <text x="507" y="194" text-anchor="middle" class="tag" fill="{SEC}">SANDBOX</text>
  <text x="507" y="216" text-anchor="middle" class="mono" fill="{SEC}">bubblewrap &#183; docker</text>
  <text x="507" y="231" text-anchor="middle" class="mono" fill="{SEC}">microvm &#183; wasm</text>

  <rect x="612" y="150" width="150" height="100" rx="6" class="card" />
  <text x="687" y="178" text-anchor="middle" class="h3">Record</text>
  <text x="687" y="194" text-anchor="middle" class="tag" fill="{SEC}">CAUSAL CHAIN</text>
  <text x="687" y="216" text-anchor="middle" class="mono" fill="{SEC}">hash-chained and</text>
  <text x="687" y="231" text-anchor="middle" class="mono" fill="{SEC}">append-only (P-8.1)</text>

  <rect x="792" y="150" width="150" height="100" rx="6" class="card" />
  <text x="867" y="178" text-anchor="middle" class="h3">Attest</text>
  <text x="867" y="194" text-anchor="middle" class="tag" fill="{SEC}">SIGNED, EVERY TURN</text>
  <text x="867" y="216" text-anchor="middle" class="mono" fill="{SEC}">budget &#183; caps &#183; gates</text>
  <text x="867" y="231" text-anchor="middle" class="mono" fill="{SEC}">law in force (P-6.23)</text>

  <path d="M 210 200 L 246 200" class="e-run" marker-end="url(#m-run)" />
  <path d="M 406 200 L 426 200" class="e-run" marker-end="url(#m-run)" />
  <path d="M 586 200 L 606 200" class="e-run" marker-end="url(#m-run)" />
  <path d="M 766 200 L 786 200" class="e-run" marker-end="url(#m-run)" />

  <path d="M 867 250 L 867 314 L 131 314 L 131 254" class="e-run" marker-end="url(#m-run)" />
  <text x="499" y="306" text-anchor="middle" class="mono">a verified self-model, every turn &#8212; past &#183; rights &#183; budget &#183; identity</text>

  <path d="M 327 150 L 327 108 L 131 108 L 131 146" class="e-run" marker-end="url(#m-run)" />
  <text x="229" y="100" text-anchor="middle" class="mono" fill="{SEC}">rejected &#8212; and the denial names its rule (Ri-0.3)</text>
</svg>
"""

BET = """<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1000 580" width="100%" height="100%" role="img" aria-label="The bet: an agent handed a truthful self-model and the same readable law as everyone else is claimed to become more intelligible in the human register, more understandable and more controllable - stated as a falsifiable wager, with the ways it could be lost named.">
  <!--
    Autonoetic - the bet.

    GENERATED. Edit the generator, not this file:
      docs/diagrams/generate.py  (writes both the dark and light variants)

    This plate argues rather than describes, so it is drawn to keep the two
    halves apart: what the runtime SUPPLIES (left - mechanical, cited, true
    today) and what is CLAIMED to follow (right). The inference between them is
    labelled a wager, because that is what it is. The bottom band names how the
    bet could be lost, and the instruments that would show it - a plate that
    only stated the upside would be marketing.

    Every clause ID printed here is checked against the active constitution by
    docs_link_guard::tests::every_clause_id_in_a_diagram_resolves.
  -->
  <defs>
    <style>{STYLE}</style>{MARKERS}
  </defs>

  <rect width="100%" height="100%" class="bg" />

  <text x="500" y="42" text-anchor="middle" class="h1">The bet</text>
  <text x="500" y="66" text-anchor="middle" class="dek">An actor that knows itself, and knows what everyone is owed, is easier to understand &#8212; and easier to govern</text>

  <rect x="36" y="100" width="270" height="140" rx="6" class="card" />
  <rect x="36" y="100" width="3.5" height="140" fill="{BLUE}" />
  <text x="54" y="124" class="h3" fill="{BLUE}">It knows itself</text>
  <text x="54" y="140" class="tag" fill="{SEC}">HANDED OVER, EVERY TURN</text>
  <text x="54" y="162" class="mono">past &#8212; its own causal chain (Ri-0.2)</text>
  <text x="54" y="178" class="mono">present &#8212; signed attestation (P-6.23)</text>
  <text x="54" y="194" class="mono">standing &#8212; the law by digest (Ri-0.10)</text>
  <text x="54" y="210" class="mono">future &#8212; a closed list (Ri-0.12)</text>
  <text x="54" y="226" class="mono">identity &#8212; non-repudiable (Ri-0.11)</text>

  <rect x="36" y="256" width="270" height="140" rx="6" class="card" />
  <rect x="36" y="256" width="3.5" height="140" fill="{GOLD}" />
  <text x="54" y="280" class="h3" fill="{GOLD}">It knows the others</text>
  <text x="54" y="296" class="tag" fill="{SEC}">ONE LAW, READABLE BY ALL</text>
  <text x="54" y="318" class="mono">38 rights it may invoke, by relation</text>
  <text x="54" y="334" class="mono">what a decider owes it (O-1 &#183; O-2)</text>
  <text x="54" y="350" class="mono">what the served party is owed (U-2)</text>
  <text x="54" y="366" class="mono">authority sits in the seat (P-2.20)</text>
  <text x="54" y="382" class="mono" fill="{SEC}">so: no theory of the other&#8217;s mind</text>

  <path d="M 310 170 L 330 170 L 330 262" class="e-run" />
  <path d="M 310 326 L 330 326 L 330 262" class="e-run" />
  <path d="M 330 262 L 348 262" class="e-run" marker-end="url(#m-run)" />

  <rect x="352" y="186" width="276" height="150" rx="6" class="card-evo" />
  <text x="490" y="210" text-anchor="middle" class="tag" fill="{PURPLE}">THE WAGER</text>
  <text x="490" y="234" text-anchor="middle" class="mono">An actor that can name its own</text>
  <text x="490" y="250" text-anchor="middle" class="mono">obligations, and yours, reasons in</text>
  <text x="490" y="266" text-anchor="middle" class="mono">the register humans reason in:</text>
  <text x="490" y="282" text-anchor="middle" class="mono">duties, standing and reasons &#8212;</text>
  <text x="490" y="298" text-anchor="middle" class="mono">not only tasks.</text>
  <text x="490" y="322" text-anchor="middle" class="dim">THIS IS THE HYPOTHESIS, NOT A FINDING</text>

  <path d="M 632 262 L 650 262" class="e-run" />
  <path d="M 650 148 L 650 376" class="e-run" />
  <path d="M 650 148 L 664 148" class="e-run" marker-end="url(#m-run)" />
  <path d="M 650 262 L 664 262" class="e-run" marker-end="url(#m-run)" />
  <path d="M 650 376 L 664 376" class="e-run" marker-end="url(#m-run)" />

  <rect x="668" y="104" width="296" height="88" rx="6" class="card" />
  <rect x="668" y="104" width="3.5" height="88" fill="{ROSE}" />
  <text x="686" y="126" class="h3" fill="{ROSE}">More intelligent, the human way</text>
  <text x="686" y="146" class="mono" fill="{SEC}">weighs an obligation against a goal</text>
  <text x="686" y="162" class="mono" fill="{SEC}">asks instead of guessing</text>
  <text x="686" y="178" class="mono" fill="{SEC}">escalates instead of rejecting (P-2.21)</text>

  <rect x="668" y="218" width="296" height="88" rx="6" class="card" />
  <rect x="668" y="218" width="3.5" height="88" fill="{GREY}" />
  <text x="686" y="240" class="h3">Understandable</text>
  <text x="686" y="260" class="mono" fill="{SEC}">every act attributed (Ri-0.11)</text>
  <text x="686" y="276" class="mono" fill="{SEC}">every denial names its rule (Ri-0.3)</text>
  <text x="686" y="292" class="mono" fill="{SEC}">every decision owes a reason (O-1)</text>

  <rect x="668" y="332" width="296" height="88" rx="6" class="card" />
  <rect x="668" y="332" width="3.5" height="88" fill="{TEAL}" />
  <text x="686" y="354" class="h3" fill="{TEAL}">Controllable</text>
  <text x="686" y="374" class="mono" fill="{SEC}">bounded by declared capabilities</text>
  <text x="686" y="390" class="mono" fill="{SEC}">halts on mechanical budgets (P-7.19)</text>
  <text x="686" y="406" class="mono" fill="{SEC}">control by law, not by supervision</text>

  <rect x="36" y="442" width="928" height="104" rx="8" class="card" stroke-dasharray="7 4" />
  <text x="54" y="466" class="tag" fill="{PURPLE}">HOW THE BET COULD BE LOST</text>
  <text x="54" y="490" class="mono">A truthful self-model the agent ignores &#183; a right that exists in text and not in tests &#183;</text>
  <text x="54" y="506" class="mono">a served party owed clauses nobody enforces &#183; an enforcer whose own lapses stop being counted.</text>
  <text x="54" y="530" class="mono" fill="{SEC}">Each is measured rather than assumed &#8212; the enforcement register, contract health and the discretion-leak ledger are the instruments.</text>
</svg>
"""

out = pathlib.Path(__file__).resolve().parent
for template, stem in (
    (PEERS, "peers-under-one-law"),
    (LOOP, "correction-loop"),
    (BET, "the-bet"),
):
    body = template.replace("{STYLE}", STYLE).replace("{MARKERS}", MARKERS)
    for palette, suffix in ((DARK, ""), (LIGHT, "-light")):
        (out / f"{stem}{suffix}.svg").write_text(body.format(**palette))
        print(f"wrote {stem}{suffix}.svg")
