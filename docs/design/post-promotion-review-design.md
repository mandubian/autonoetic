# Phase 4 — Post-promotion Background Review

**Status:** Draft
**Refs:** Issue #199, `docs/design/promotion-federation-plan.md` §2.6, `docs/design/sealed-evaluator-replay-design.md`, `docs/design/recording-mode-design.md`

---

## 1. Motivation

### 1.1 What exists today

The sentinel system runs periodic security sweeps (credential leaks, capability accretion, approval bypass, sandbox escape, supply chain, prompt injection, failure clusters). Findings are stored in `security_findings` but are **purely detective** — no operator alerting, no escalation, no behavioral drift detection against recorded baselines.

Promotion federation (Phases 1-3) added:
- Federation evaluation roles with structured verdicts
- Operator escalation (`EscalationMessage`) for promotion decisions
- Recording mode (`--record-network`) for capturing real traffic as fixtures
- Sealed evaluator replay from recorded fixture sets

What's missing: **ongoing review of promoted agents**. Once an agent is promoted and live, there's no mechanism to detect that its behavior has drifted from the recorded baseline, that it's calling new endpoints, or that its failure patterns have changed.

### 1.2 What we want

A background sentinel that periodically reviews promoted agents:

```
┌─────────────────────────────────────────────────────────────┐
│                  Post-promotion Review Sentinel              │
│                                                             │
│  Every N hours, for each promoted agent with recordings:     │
│                                                             │
│  1. Load the latest fixture set for this agent's revision    │
│  2. Run the sealed evaluator against the current revision    │
│     using the OLD fixture set                                │
│     └── Expected: all old requests still work                │
│         (regression detection)                               │
│                                                             │
│  3. If the agent has new fixture sets (re-recordings):       │
│     Compare against OLD fixture set for drift:                │
│     └── New hosts appeared?                                  │
│     └── Old hosts disappeared?                               │
│     └── Response shapes changed? (status codes, body size)   │
│                                                             │
│  4. Aggregate findings → operator escalation                  │
│     - Critical drift    → EscalationMessage (urgent)          │
│     - Minor drift       → security_finding (advisory)         │
│     - No drift          → no-op                              │
│                                                             │
│  5. On operator review:                                       │
│     - Accept drift     → update baseline fixture set         │
│     - Reject revision  → trigger rollback                    │
│     - Investigate      → spawn sealed evaluator for deeper   │
│                          diagnostics                          │
└─────────────────────────────────────────────────────────────┘
```

### 1.3 Two tiers of review

Phase 4 has two tiers, not one:

**Tier 1 — Observability review (all agents).** For every promoted agent, regardless of whether it has fixture sets, the review analyzes:
- Causal event trends: error rate changes, tool failure rate increases, authorization-denied spikes
- Sentinel findings accumulated since last review: new credential leaks, sandbox escape attempts, capability accretion
- Session report anomalies: unusual execution patterns, unexpected suspensions or escalations
- This is the baseline review that applies to ALL promoted agents.

**Tier 2 — Fixture-based drift detection (recorded agents only).** For agents that have been recorded with `--record-network`, the review additionally:
- Compares baseline fixture set (from original recording) against current fixture set (from re-recording)
- Runs sealed evaluator replay of baseline fixtures against the current revision to detect regressions
- Reports endpoint drift (new/removed hosts, changed response shapes)
- This is the richer review that applies ONLY to agents with fixture sets.

Without fixtures, the review is still meaningful. It catches operational drift (is the agent failing more? hitting auth errors? triggering sentinel findings?) even when the operator never ran `--record-network`.

With fixtures, the review is deeper. It catches behavioral drift (is the agent calling different endpoints? have API contracts changed?) in addition to operational drift.

### 1.4 Key insight: operational drift vs behavioral drift

The existing sentinel (Phases 1-3) already covers security-oriented drift (credential leaks, sandbox escapes). Phase 4's operational drift detection fills the gap:

| Drift type | Data source | Requires fixtures? |
|-----------|-------------|-------------------|
| **Security drift** | Sentinel checks (existing) | No |
| **Operational drift** | Causal events, session reports, sentinel findings | No |
| **Behavioral drift** | Fixture set comparison, sealed eval replay | Yes |

Phase 4 ships Tier 1 (observability review for all agents). Tier 2 (fixture-based drift) is deferred — it depends on fixture sets being widely adopted, which requires Phase 2 to be in regular use.

---

## 2. Design

### 2.1 Review lifecycle

```
Post-promotion review fires (scheduled job, daily)
         │
         ├── TIER 1: Observability review (ALL promoted agents)
         │   ├── 1. Query causal events since last review for this agent
         │   │       ├── Error rate trend (tool failures, suspension count)
         │   │       └── Authorization-denied spikes, unexpected escalations
         │   ├── 2. Query sentinel findings accumulated since last review
         │   │       └── New findings for this agent (any severity)
         │   ├── 3. Query recent session reports for anomalies
         │   │       └── Unusual execution patterns, excessive durations
         │   └── 4. Compute operational health score
         │
         ├── TIER 2: Fixture-based drift (ONLY agents with fixture sets)
         │   ├── 1. Load FS_v1 (baseline fixture set)
         │   ├── 2. Run sealed evaluator against current revision with FS_v1
         │   │       └── All pass? → no regression
         │   │       └── Any fail? → regression detected
         │   ├── 3. If FS_v2 exists (operator re-recorded):
         │   │       ├── Compare hosts: FS_v1 vs FS_v2
         │   │       └── Compare endpoints: FS_v1 vs FS_v2
         │   └── 4. Detect behavioral drift (new/removed hosts, response changes)
         │
         └── Emit review result:
                 ├── No issues → causal event, no escalation
                 ├── Minor drift → security_finding (advisory)
                 └── Critical drift → EscalationMessage (operator review)
```

Every promoted agent gets Tier 1 (observability). Only agents with fixture sets also get Tier 2.

### 2.2 Drift detection criteria

**Hard drift** (operator escalation required):
| Criterion | Detection |
|-----------|-----------|
| New host appeared | FS_v2 has host not in FS_v1 |
| Old host disappeared | FS_v1 has host not in FS_v2 |
| Sealed eval regression | FS_v1 fixture replay against current revision returns different status/body |
| Endpoint removed | FS_v1 endpoint missing in FS_v2 |

**Soft drift** (security finding, advisory):
| Criterion | Detection |
|-----------|-----------|
| New endpoint on existing host | FS_v2 has new path on a host from FS_v1 |
| Response status changed | Same endpoint returned 200 → now returns 4xx |
| Response body size changed significantly | >50% change in body length |
| Redaction coverage changed | New headers appear that should be redacted |

### 2.3 Scheduled review

Reuses the existing sentinel cron job infrastructure (`scheduler/scheduler.rs`).

A new scheduled job `sentinel.post_promotion_review` is registered at startup:

```rust
ScheduledAction::Workflow {
    workflow_template: "post_promotion_review",
    interval_secs: 24 * 3600,  // daily
}
```

The review workflow:
1. Query all promoted agents with at least one `FixtureSet` where `status = Ready`
2. For each agent, group fixture sets by revision_id
3. Pick the newest revision with fixtures (current) and the oldest (baseline)
4. Run drift detection
5. Persist findings + optionally escalate

### 2.4 Sealed evaluator regression check

The sealed evaluator replay (Phase 3) provides the mechanism:

```
1. Pre-populate artifact temp_base with baseline fixtures (FS_v1)
2. Run artifact_exec on the current revision's entrypoint
3. If any request returns unfixtured_target → regression
4. If any response differs from recorded → regression
```

This reuses `artifact_exec`'s `fixture_set_ref` parameter.

### 2.5 Fixture set comparison

A new `compare_fixture_sets` function compares two fixture sets:

```rust
pub struct FixtureSetComparison {
    pub baseline_id: String,
    pub current_id: String,
    pub new_hosts: Vec<String>,
    pub removed_hosts: Vec<String>,
    pub new_endpoints: Vec<String>,
    pub removed_endpoints: Vec<String>,
    pub changed_responses: Vec<ChangedEndpoint>,
    pub regression_count: u64,
    pub drift_severity: DriftSeverity,
}

pub enum DriftSeverity {
    None,
    Minor,     // new endpoints on existing hosts
    Major,     // new hosts, removed endpoints
    Critical,  // sealed eval regression
}
```

### 2.6 Escalation path

When critical drift is detected, the review sends an `EscalationMessage`:

```json
{
  "escalation_type": "PostPromotionAnomaly",
  "artifact_id": "art_xxx",
  "agent_id": "agent.name",
  "revision_id": "rev_xxx",
  "role_verdicts": [
    {
      "role": "post_promotion_review",
      "agent_id": "security_sentinel",
      "passed": false,
      "findings_summary": "2 new hosts detected, 1 endpoint removed",
      "evidence_ref": "fs_xxx"
    }
  ],
  "planner_synthesis": "Agent 'agent.name' revision 'rev_xxx' shows behavioral drift
   from recorded baseline. New hosts: cdn.example.com, api-v2.example.com. Removed: api-v1.example.com.
   Sealed evaluation of baseline fixtures against current revision: FAILED (3 regressions).",
  "root_session_id": "root-xxx"
}
```

The operator resolves the escalation (accept drift / reject revision / investigate).

### 2.7 No new types

Phase 4 reuses:
- `EscalationMessage` + `EscalationStatus` (Phase 1) — for operator escalation
- `FixtureSet` + `RecordingSession` (Phase 2) — for fixture baselines
- `artifact_exec` with `fixture_set_ref` (Phase 3) — for regression check
- `security_findings` (sentinel) — for advisory findings
- `scheduled_jobs` (sentinel) — for periodic review scheduling

### 2.8 CLI command

The operator needs tools to inspect review status:

```
autonoetic review status [--agent <agent_id>]
autonoetic review inspect <review_id>
autonoetic review history [--agent <agent_id>]
```

This is a new top-level subcommand:
```rust
pub enum Commands {
    // ... existing ...
    Review(ReviewArgs),
}

pub enum ReviewCommands {
    Status {
        agent: Option<String>,
        json: bool,
    },
    Inspect {
        review_id: String,
        json: bool,
    },
    History {
        agent: Option<String>,
        limit: i64,
        json: bool,
    },
}
```

---

## 3. Acceptance criteria

### Tier 1 (observability review — all agents)
- [ ] Scheduled `sentinel.post_promotion_review` job runs daily
- [ ] Review queries causal events for error rate trends (tool failures, suspensions)
- [ ] Review queries sentinel findings accumulated since last review
- [ ] Review computes operational health score per agent
- [ ] No-issues review emits a casual event only
- [ ] Minor findings written to `security_findings` table (advisory)
- [ ] Critical findings (e.g., >50% error rate increase) trigger `EscalationMessage`

### Tier 2 (fixture-based drift — recorded agents only)
- [ ] Drift detection: compares fixture sets for new/removed hosts and endpoints
- [ ] Sealed evaluator regression check: replays baseline fixtures against current revision
- [ ] Fixture-based findings merged into the same escalation path

### CLI
- [ ] `autonoetic review status|inspect|history` CLI command
- [ ] Integration test: review finds sentinel findings accumulated since last check
- [ ] Integration test: review with fixture sets detects new/removed hosts

---

## 4. Security & invariants

### 4.1 No automatic rollback

The post-promotion review is **advisory only**. It never rolls back a promoted revision automatically. The operator decides all remediation actions. This preserves the principle of operator-as-trust-root.

### 4.2 Minimal performance impact

Fixture set comparison operates on metadata (host lists, endpoint lists from fixture file paths), not on full fixture contents. The sealed evaluator regression check is the heavyweight operation — it runs the artifact in a sandbox. This runs at most once per agent per day, scheduled during low-activity hours (default 03:00 UTC like the full sentinel sweep).

### 4.3 Fixture set staleness

If the agent has been promoted to a newer revision since the last recording, the comparison still works (comparing fixture sets from different revisions). The operator sees "revision X baseline vs revision Y current" context in the escalation message.

---

## 5. Dependencies & boundaries

### 5.1 Dependencies
- Sentinel scheduled job infrastructure (exists)
- `FixtureSet` + `RecordingSession` (Phase 2, shipped)
- `artifact_exec` with `fixture_set_ref` (Phase 3, shipped)
- `EscalationMessage` types and storage (Phase 1, shipped)
- `security_findings` table (sentinel, exists)

### 5.2 Scope

**Phase 4 ships Tier 1 (observability review for all agents).** Tier 2 (fixture-based drift) requires the operator workflow around `--record-network` to be well-established. Tier 2 is documented here for architectural continuity but scheduled separately.

### 5.3 Out of scope
- **Automated re-recording** — the operator must explicitly re-record (Phase 2) to produce new fixture sets
- **Rollback automation** — operator decides all rollbacks
- **Planner-driven background review** — the planner is not involved in post-promotion review
- **Cross-agent anomaly correlation** — each agent reviewed independently

---

## 6. Open questions

1. **Review schedule granularity**: daily for all agents? Configurable per-agent? **Proposed:** daily default, configurable via `sentinel.post_promotion_interval_secs` in gateway config. Agents without recent activity skip (no new causal events since last review).

2. **Fixture set retention** (Tier 2): after an agent has been promoted N times, old fixture sets accumulate. Should old fixture sets be auto-expired when a newer recording exists? **Proposed:** keep all fixture sets (immutable artifacts). The review always compares against the baseline (first recording for the promoted revision) and the latest recording.

3. **False positives from legitimate changes**: an agent that adds a new feature legitimately calls new endpoints. How does the operator distinguish legitimate change from drift? **Proposed:** the escalation message includes the drift diff. The operator sees exactly what changed and decides. Tier 1 (observability) has fewer false positives since it measures operational health (error rates) rather than behavioral changes.

4. **What constitutes "critical" in observability review?** Error rate increase >50% over baseline? Any new sentinel finding of severity `error` or `critical`? **Proposed:** critical = any new `critical`-severity sentinel finding OR >50% increase in tool failure rate. Minor = new `warning`-severity findings OR >20% error rate increase. Configurable thresholds.

5. **No fixtures = no Tier 2.** For agents running locally without `--record-network`, the review is still valuable (Tier 1). Tier 2 is only meaningful once fixture sets exist. This is intentional — Phase 4 still ships Tier 1 for all agents.
