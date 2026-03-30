# Gateway Primitives for Agent Evolution and Federation

This note restates the simplification goal from [plan_extended.md](../archived/plan_extended.md) against the current codebase, then extends it toward two future concerns:

1. agents that can improve themselves safely over time;
2. gateways and agents that can operate across multiple nodes.

The key adjustment is this: the gateway should not be a dumb pipe in the literal sense. It should be a generic authority.

It must keep execution, policy, durability, audit, approval, and replication responsibilities. It should stop carrying role-specific narratives, semantic heuristics, and opinionated orchestration models.

Implementation-oriented MVP follow-up: [spec-agent-revision-evaluation-federation-mvp.md](../spec-agent-revision-evaluation-federation-mvp.md)

## Updated Thesis

The gateway provides generic mechanisms.

Agents compose those mechanisms into planning, delegation, learning, adaptation, evaluation, promotion, and collaboration behaviors.

That means:

- keep gateway-owned authority and durability;
- remove gateway-owned role semantics;
- make agent evolution and distribution first-class through generic primitives.

## Delegation Boundary

Agents may propose, author, request, evaluate, and recommend.

The gateway must seal, bind, enforce, persist, and audit.

In practice:

- agents may author candidate bundles, write eval suites, request promotion, and interpret evidence;
- the gateway must remain the authority for revision identity, immutable materialization, alias movement, session pinning, capability checks, approvals, runtime permits, provenance, and any future portable export sealing.

This boundary is the point of the architecture. Nondeterministic agent behavior is welcome upstream of activation, but the activation path itself must stay deterministic and gateway-owned.

## What Stays in the Gateway

These are still the right responsibilities for the gateway:

| Area | Why it belongs in the gateway |
|---|---|
| Policy and capability enforcement | It is the trust boundary |
| Sandbox execution | Agents must never execute directly |
| Content, artifact, and layer stores | They are shared, durable, and content-addressed |
| Workflow and task durability | This is generic runtime infrastructure, not domain logic |
| Approval suspension and continuation resume | It is an execution-authority concern |
| Causal chain and execution traces | Audit must not depend on agent behavior |
| Session checkpoints and forks | Recovery and reproducibility are platform responsibilities |
| Peer transport, replication, and placement | Cross-node execution is infrastructure |

The current code already has valuable generic building blocks here: workflow task durability, turn continuation, content-addressed artifacts, runtime locks, script-mode execution, schema enforcement, and OFP transport.

## What Should Move Out of the Gateway

These areas still fit the simplification target and should be externalized into agent-authored behavior or adapter configuration:

| Area | Direction |
|---|---|
| Implicit ingress routing | Caller or adapter chooses target agent explicitly |
| Session lead bindings | Remove as routing policy |
| Planner and specialist identity semantics | Keep as conventions in agent bundles, not runtime logic |
| Semantic wake predicates | Replace with timer plus signal primitives |
| Install-specific role gates | Replace with generic revision, approval, and promotion primitives |
| Disclosure taxonomy complexity | Replace with simple restricted-output marking |
| Mandatory state file conventions | Document as best practice, do not enforce |

## The Agent Model Should Split into Four Layers

To go further on autonomy without losing control, the platform should distinguish four different things that are currently too easy to conflate.

### 1. Agent identity

The stable logical name, such as `planner.default`.

This is what users, adapters, and other agents normally refer to.

### 2. Agent revision

An immutable content-addressed snapshot of:

- `SKILL.md`;
- agent files;
- declared capabilities;
- runtime metadata;
- `runtime.lock`;
- optionally referenced skills, artifacts, layer mounts captured in the runtime closure, and model bindings.

An agent revision is created from an immutable bundle or imported exchange object. Repository-local `agents/<agent_id>/` directories may still exist as authoring inputs, but they are not runtime resolution targets and are never executed directly by the gateway.

This is the agent equivalent of an artifact.

Suggested notation:

- logical identity: `planner.default`
- immutable reference: `planner.default@rev_sha256:abcd...`

### 3. Agent alias or channel binding

A mutable pointer from a stable name to a chosen revision.

Examples:

- `planner.default -> rev_a`
- `planner.canary -> rev_b`
- `planner.stable -> rev_a`

Promotion and rollback become pointer moves, not file mutation.

The broader model allows multiple aliases or channels per logical agent. The current MVP intentionally narrows this to one mutable default alias per logical agent so revision pinning and promotion semantics land first.

### 4. Agent state and knowledge

Mutable data outside the immutable revision:

- session working state;
- durable knowledge and user profile;
- evaluation history;
- trajectories and datasets;
- metrics and budgets.

This separation is critical. Otherwise "self-learning" becomes an unsafe mix of prompt mutation, memory mutation, code mutation, and model mutation with no clean provenance.

## Sessions Must Pin Revisions

At session start, the gateway should resolve the target alias to a concrete immutable revision and pin that revision for the life of the session.

This gives:

- reproducibility;
- reliable rollback;
- causal attribution;
- safe canarying;
- clean distributed execution.

Resolution should come from revision and alias registry state only. There should be no mutable-directory fallback once revision semantics exist.

The session should carry something like:

```json
{
  "agent_id": "planner.default",
  "agent_revision": "rev_sha256:abcd1234",
  "runtime_lock_hash": "sha256:...",
  "home_node": "gateway-eu-1"
}
```

## Self-Improvement Should Be Layered

Self-learning is not one thing. The gateway should support several distinct improvement loops without hardcoding any specific learning strategy.

| Layer | What changes | Example |
|---|---|---|
| Memory learning | Facts and lessons | "This API times out on weekends" |
| Skill learning | Reusable procedures and code | New parser or repair skill |
| Agent revision learning | Instructions, capabilities, toolset, middleware | Better planner or debugger revision |
| Model learning | Provider/model binding or fine-tuned weights | Fine-tuned coder model |

Each layer needs different controls.

### Memory learning

Cheap, online, frequent.

The gateway should provide:

- durable knowledge storage;
- bounded wake-time memory projection;
- provenance and tagging;
- security scanning for injected memory;
- cross-agent sharing with explicit visibility and trust policy.

### Skill learning

Agents should be able to publish reusable skill bundles, but publication and activation must be separate.

The gateway should provide:

- immutable skill revisions;
- skill attestations and provenance;
- skill evaluation hooks;
- approval and promotion from candidate to active.

### Agent revision learning

This is the most important missing mechanism.

Agents should not mutate their live installed directory in place. They should create candidate revisions from an existing base revision, evaluate them, and only then promote them.

### Model learning

Fine-tuning, RL, preference optimization, or other training strategies should be pluggable backends.

The gateway should not encode training logic. It should provide:

- datasets;
- trajectories;
- training job submission and tracking;
- model revision registry;
- eval-driven promotion and rollback.

## Generic Primitives for Agent Evolution

The gateway should expose generic primitives like these.

### Revision primitives

- `agent.revision.create(base_ref, artifact_id, change_summary, metadata?)`
- `agent.revision.list(agent_id)`
- `agent.revision.inspect(agent_ref)`
- `agent.revision.diff(from_ref, to_ref)`
- `agent.revision.promote(alias, candidate_ref, reason?)`
- `agent.revision.rollback(alias, target_ref?, reason?)`
- `agent.revision.archive(agent_ref)`

These primitives should not assume human approval, auto-promotion, or role-specific gates. Policy decides what is allowed.

### Evaluation primitives

- `eval.suite.publish(name, spec)`
- `eval.run(agent_ref, suite_id, mode?, input_set?)`
- `eval.compare(baseline_ref, candidate_ref, suite_id)`
- `eval.shadow(alias, candidate_ref, traffic_selector)`
- `eval.canary(alias, candidate_ref, percentage, stop_policy)`
- `eval.report(eval_run_id)`

This makes self-improvement measurable instead of narrative-only.

### Learning data primitives

- `trajectory.record(step)`
- `trajectory.export(session_id, format)`
- `dataset.append(dataset_id, source_ref, metadata?)`
- `dataset.snapshot(dataset_id)`
- `feedback.record(subject_ref, score, evidence?)`
- `knowledge.publish(memory_id, visibility, trust_policy?)`
- `knowledge.import(ref, merge_policy?)`

These primitives support self-learning and learning from others without forcing one algorithm.

### Training and model registry primitives

- `training.submit(kind, dataset_ref, base_model, output_target, config?)`
- `training.status(job_id)`
- `training.cancel(job_id)`
- `model.revision.register(model_ref, provenance)`
- `model.revision.promote(alias, model_ref)`
- `model.revision.rollback(alias, model_ref?)`

`kind` might be `fine_tune`, `preference_opt`, `rl`, or any backend-specific adapter. The gateway tracks the job and its artifacts; the backend performs the actual training.

## Promotion Should Be a Generic Policy, Not a Special Workflow

The current system still carries evolution-specific promotion logic. The more general model should be:

1. an agent or external actor creates a candidate revision;
2. one or more eval runs execute against it;
3. optional approvals or policy checks run;
4. a stable alias is moved to the candidate;
5. rollback is always possible by pointing the alias back.

This applies equally to:

- agent revisions;
- skill revisions;
- model revisions;
- capsules imported from other nodes.

## Social Learning Needs Provenance and Trust

Learning from others should be possible, but never blind.

The gateway should treat imported knowledge, skills, revisions, and models as foreign objects with provenance and trust metadata.

Useful concepts:

| Concept | Purpose |
|---|---|
| Origin node | Where the asset came from |
| Signer | Who attested to it |
| Lineage | What base revision or dataset produced it |
| Eval evidence | Which tests and scores justify trust |
| Trust domain | Whether it is local, federated, partner, or untrusted |

Agents can then learn from others through controlled import, comparison, and evaluation.

## Federation Model

Autonoetic already points toward distribution through OFP, remote HTTP access, runtime locks, and cognitive capsules. To make that coherent, the platform should separate identity, placement, and execution authority.

### Identity

An agent identity is global enough to be referenced across nodes.

### Placement

Where a session or task should run is a separate concern.

Suggested placement inputs:

- required sandbox backend;
- data affinity;
- trust domain;
- cost or latency preference;
- network locality;
- required secrets domain.

### Execution authority

The node that owns the resource executes the action and logs the authoritative result.

This is especially important for:

- secret access;
- network access;
- local filesystem mounts;
- approvals;
- training jobs.

## Generic Primitives for Federation

### Peer and placement primitives

- `peer.list()`
- `peer.describe(peer_id)`
- `placement.plan(agent_ref, constraints)`
- `execution.lease.request(agent_ref, peer_id, constraints)`
- `execution.lease.release(lease_id)`

### Replication primitives

- `artifact.replicate(handle, peer_id)`
- `layer.replicate(layer_id, peer_id)`
- `capsule.export(agent_ref, mode)`
- `capsule.import(capsule_ref)`
- `knowledge.replicate(ref, peer_id, visibility)`

### Cross-node session primitives

- `signal.send(target_session, name, payload)`
- `message.send(target_agent_or_session, payload)`
- `session.fragment.attach(root_session_id, fragment_ref)`
- `trace.export(session_id)`

The important point is that a distributed session should be represented as one logical root session with multiple node-local execution fragments, not as one mutable shared process.

## Distributed Design Rules

1. Sessions pin immutable agent revisions, not mutable directories.
2. Cross-node execution always uses content-addressed references.
3. Approvals are resolved by the gateway that owns the protected resource.
4. Capabilities attenuate across delegation and never widen when crossing gateways.
5. Imported revisions, skills, knowledge, and models are foreign until attested and optionally promoted.
6. Causal history is append-only and mergeable by reference, not a shared mutable database.
7. Network partitions and delayed replication are expected conditions, not edge cases.

## Anti-Goals

The gateway should not:

- auto-learn hidden heuristics on behalf of agents;
- auto-promote revisions because one run looked good;
- encode a privileged concept of planner, builder, steward, or curator;
- mutate installed agent directories in place;
- assume one global shared memory or one global shared database across nodes;
- couple federation to one transport-specific execution model.

## Suggested Implementation Order

### Phase 1

Finish the original simplification work:

- explicit ingress targeting;
- timer plus signal wake model;
- generic approval queue;
- removal of role-specific install and promotion gates;
- simpler disclosure model.

### Phase 2

Introduce immutable revision semantics:

- `agent_id` versus `agent_ref`;
- alias pointers;
- session pinning;
- revision diff and inspect.

### Phase 3

Add evaluation and promotion primitives:

- eval suites;
- candidate revisions;
- shadow and canary modes;
- rollback.

### Phase 4

Add learning data and training registry primitives:

- trajectories;
- datasets;
- feedback;
- training jobs;
- model revision promotion.

### Phase 5

Make federation first-class:

- peer registry;
- replication;
- placement and execution leases;
- capsule import and export;
- cross-node session fragments.

## Bottom Line

If Autonoetic wants more autonomous agents without gateway bloat, the platform needs immutable revisions plus generic evolution mechanisms.

If Autonoetic wants distributed execution without losing control, the platform needs content-addressed exchange, pinned revisions, leased placement, and resource-owner authority.

The gateway should not tell agents how to evolve.

It should give them the primitives to:

- create candidate revisions;
- evaluate themselves;
- learn from trajectories and peers;
- promote safely;
- rollback deterministically;
- move across nodes without losing provenance.
