---
title: Priority Scheduler Reference
---

# Priority Scheduler Reference

Precise contract for the priority-aware admission scheduler: the request header clients send, the response codes the gateway returns, and every configuration knob with its exact name and default. For how it works conceptually, see [Priority Scheduling](../concepts/reliability/priority-scheduling.md).

The scheduler is **disabled by default**. When off, the gateway uses its [legacy concurrency-limit admission path](../concepts/reliability/rate-limiting.md).

---

## Request header: `x-smg-priority`

Clients request a priority class with the `x-smg-priority` request header.

| Property | Behavior |
|----------|----------|
| **Header name** | `x-smg-priority` |
| **Values** | `system`, `interactive`, `default`, `bulk` |
| **Case** | Case-insensitive (`Bulk`, `INTERACTIVE`, `SyStEm` all parse). Surrounding whitespace is trimmed. |
| **Missing header** | Treated as `default`. |
| **Unknown value** | Any unrecognized value (including the empty string) silently degrades to `default` — admission never fails because of a typo in this header. Counted under `smg_scheduler_unknown_priority_value_total`. |

### Tenant clamp

The header chooses a class; the **tenant's configured maximum class caps it**. The effective class is:

```text
effective = min(requested_class, tenant_max_class)
```

- The clamp only ever moves a request **down**. The header can never promote a request above the tenant's ceiling.
- A tenant whose `max_class` is `default` that sends `x-smg-priority: system` is admitted as `default`.
- A clamp (effective class below requested) is counted under `smg_scheduler_clamp_total`.

A tenant's `max_class` comes from the per-tenant policy in the YAML config, or from the gateway-wide default (`--priority-scheduler-default-max-class`) for tenants not listed. See [Tenant policy](#tenant-policy).

---

## Response codes

The scheduler surfaces admission and preemption outcomes as HTTP status codes. Each rejection also carries the gateway's standard JSON error body and `X-SMG-Error-Code` header.

| Status | Condition | `X-SMG-Error-Code` | Extra headers |
|--------|-----------|--------------------|---------------|
| **503** Service Unavailable | **Preempted** — admitted, then cancelled before its first byte to make room for a higher-priority request | `scheduler_preempted` | `X-SMG-Preempted: true`, `Retry-After: 1` |
| **429** Too Many Requests | **Queue full** — the request's per-class queue is at its configured depth | `scheduler_queue_full` | — |
| **408** Request Timeout | **Queue timeout** — the request waited longer than its class's `queue_timeout` | `scheduler_queue_timeout` | — |
| **499** Client Closed Request | **Client gone** — the client disconnected before admission completed (nginx convention; never actually read) | `scheduler_client_cancelled` | — |

!!! tip "Telling a preemption apart from an overload"
    Both preemption and a genuinely overloaded backend can return `503`. The `X-SMG-Preempted: true` header is what distinguishes a preemption. A preempted request is safe to retry immediately, which is why it carries `Retry-After: 1`.

---

## Enabling the scheduler

The scheduler is controlled by CLI flags (also settable in the config file). Per-class tuning and per-tenant policy live in a separate optional YAML file.

```bash
smg \
  --worker-urls http://w1:8000 http://w2:8000 \
  --priority-scheduler-enabled \
  --priority-scheduler-default-max-class interactive \
  --priority-scheduler-config /etc/smg/priority.yaml
```

### CLI flags

| Flag | Default | Description |
|------|---------|-------------|
| `--priority-scheduler-enabled` | `false` | Master switch. When unset, the legacy concurrency-limit middleware stays wired and no scheduler is constructed. |
| `--priority-scheduler-default-max-class` | `default` | Maximum class for tenants not listed in the YAML (`system` \| `interactive` \| `default` \| `bulk`). Parsed with the same rules as the header — an unknown value falls back to `default`. |
| `--priority-scheduler-config` | unset | Path to the optional priority-scheduler YAML (per-class overrides + per-tenant policy). Absent → built-in defaults and an empty tenant policy map. |
| `--priority-scheduler-tenant-metric-top-n` | `32` | Cap on configured tenants emitted as distinct fair-share metric labels. Remaining tenants use `tenant="other"`. Existing non-fair-share tenant counters still intern their raw tenant label. |
| `--capacity-credit-generation` | unset | Enable the authenticated capacity-credit protocol for one allocator generation. Omit to leave the protocol completely unwired. |
| `--capacity-credit-ttl-ms` | `30000` | How long an unredeemed credit holds its scheduler slot before expiring. Must be greater than zero. |
| `--capacity-credit-terminal-retention-secs` | `600` | How long redeemed, cancelled, and expired token tombstones remain for replay protection. Must be greater than zero. |
| `--capacity-credit-required` | `false` | Reject protected inference requests that do not redeem a valid credit. Requires `--capacity-credit-generation`. |
| `--priority-scheduler-adaptive-capacity` | `false` | Let enforced engine-feedback telemetry lower explicit partition capacities. Requires adaptive admission in `enforce` mode with the `engine_feedback` strategy. |

!!! warning "Fail-safe startup"
    If the scheduler is enabled but cannot start — unparsable YAML, or class reservation floors + shares that sum to more than the live backend capacity — the gateway logs at `ERROR` and **falls back to legacy admission** instead of aborting. It does not take the data plane down.

    Once capacity credits or adaptive scheduler capacity are configured, the
    gateway instead fails startup on scheduler construction errors. An
    explicitly configured admission boundary must not silently disappear.

---

## YAML configuration

The file referenced by `--priority-scheduler-config` has two top-level maps, both optional. An empty or absent file means "use built-in defaults for every class, no per-tenant overrides."

```yaml
# Per-class tuning. Any class you omit keeps its built-in default.
classes:
  interactive:
    reserved_floor: 128       # always at least 128 slots
    reserved_per_slot: 0.25   # ...and 25% of capacity once the fleet is large
    queue_size: 256
    queue_timeout_secs: 30
    starvation_threshold_secs: 5
    can_preempt: true
  bulk:
    reserved_floor: 0
    queue_size: 1024
    queue_timeout_secs: 300
    starvation_threshold_secs: 120
    can_preempt: false

# Per-tenant priority ceiling. Tenants not listed use
# --priority-scheduler-default-max-class.
tenant_policies:
  "auth:acme":
    max_class: interactive
  "auth:internal-cron":
    max_class: system

# Optional weighted sharing among tenants that contend in the same model pool.
fair_share:
  default_weight: 1
  default_output_tokens: 256
  trust_output_token_estimate_header: false
  trust_request_model_header: false
  tenant_weights:
    "header:alice": 10
    "header:bob": 5
```

Class keys and `max_class` values are lowercase: `system`, `interactive`, `default`, `bulk`. An unknown class name in the YAML is a parse error (which triggers the fail-safe fallback above), unlike the lenient request header.

### Per-class knobs

Each entry under `classes` accepts the following fields. All are per-class.

| Field | Type | Meaning |
|-------|------|---------|
| `reserved_floor` | integer (slots) | Minimum slots reserved for this class — the value the effective reservation never drops below. A higher class's *unused* reservation is held back from lower classes; a class's own reservation never reduces its own headroom. |
| `reserved_per_slot` | float (0.0–1.0+) | Share of live capacity reserved for this class, on top of the floor: `effective = max(reserved_floor, ceil(reserved_per_slot × capacity))`, recomputed as capacity changes so the reservation tracks the fleet. `0.0` (the default) means purely absolute (just the floor). Must be finite and ≥ 0. At startup, if the floors + shares exceed capacity the scheduler fails safe to legacy admission; at runtime a capacity dip is absorbed by clamping the lowest classes first. |
| `queue_size` | integer | Per-class queue depth limit. A request that arrives when the queue is full is rejected with **429**. |
| `queue_timeout_secs` | integer (seconds) | How long a queued request waits before it is rejected with **408**. Must be `> 0`. |
| `starvation_threshold_secs` | integer (seconds) | Head-of-queue age past which the dispatcher promotes a waiter out of normal priority order (and lets it use a reserved-but-unused slot) to avoid starvation. Must be `> 0`. |
| `can_preempt` | boolean | Whether admissions in this class may preempt a lower-class in-flight request that has not yet emitted its first byte. |

### Built-in defaults

These apply to any class with no YAML override.

| Class | `reserved_floor` | `reserved_per_slot` | `queue_size` | `queue_timeout_secs` | `starvation_threshold_secs` | `can_preempt` |
|-------|-----------------:|--------------------:|-------------:|---------------------:|----------------------------:|:-------------:|
| `system` | 32 | 0.0 | 64 | 30 | 5 | `true` |
| `interactive` | 128 | 0.25 | 256 | 30 | 5 | `true` |
| `default` | 0 | 0.10 | 512 | 60 | 30 | `false` |
| `bulk` | 0 | 0.0 | 1024 | 300 | 120 | `false` |

Higher classes fail fast (short queues, short timeouts) and reserve capacity — `interactive` and `default` reserve a share that grows with the fleet, while `system` keeps a fixed floor (control-plane traffic is low-volume regardless of fleet size). Lower classes wait patiently (deep queues, long timeouts) and reserve nothing.

### Validation

At startup the scheduler validates:

- `queue_timeout_secs > 0` for every class (else startup fails for that class).
- `starvation_threshold_secs > 0` for every class.
- The sum of all `reserved` values must not exceed the live backend capacity. On a capacity *shrink* that would otherwise break this invariant, the scheduler scales reservations down proportionally rather than locking itself out.

Any validation failure triggers the [fail-safe fallback to legacy admission](#enabling-the-scheduler).

---

## Weighted fair sharing by output tokens

The optional `fair_share` map replaces FIFO ordering within each priority-class
queue with weighted ordering by output tokens. Weights are relative and do not
need to sum to 100. For example, weights `10` and `5` target a 2:1 token split
while both tenants remain backlogged and eligible for the same model pool.
Priority class selection remains the outer policy.

| Field | Default | Meaning |
|-------|---------|---------|
| `default_weight` | `1.0` | Weight assigned to a resolved tenant absent from `tenant_weights`. Must be finite and greater than zero. |
| `default_output_tokens` | `256` | Provisional output-token charge used when no trusted estimate is available. Must be greater than zero. |
| `trust_output_token_estimate_header` | `false` | Honor `x-smg-output-token-estimate`. Enable only behind a proxy that strips client copies and injects a validated estimate. |
| `trust_request_model_header` | `false` | Honor `x-smg-request-model` for per-model profile selection. Model profiles require this setting. Enable only behind a proxy that strips client copies and injects the parsed request model. |
| `tenant_weights` | `{}` | Relative weights keyed by canonical tenant key. Every value must be finite and greater than zero. |
| `model_profiles` | `{}` | Optional model-scoped hierarchical policies. Each profile contains explicit `tenant_weights` plus one positive aggregate `other_weight`. |

Per-model profiles use two-level weighted fair queueing. Explicit tenants and
one aggregate `other` bucket contend at the outer level. Every unlisted real
tenant retains its own identity and shares the `other` bucket equally at the
inner level:

```yaml
fair_share:
  default_output_tokens: 256
  trust_output_token_estimate_header: true
  trust_request_model_header: true
  model_profiles:
    deepseek-v4-flash:
      tenant_weights:
        "header:junu": 30
        "header:xuezhou": 40
        "header:zhenting": 10
      other_weight: 20
    kimi-k3:
      tenant_weights:
        "header:mukhesh": 80
      other_weight: 20
```

The percentages apply while the corresponding buckets are simultaneously
backlogged for that model. Idle shares are borrowed, so a free model slot is
never held empty. Requests for models without a configured profile continue to
use the legacy flat process-wide weights.

The scheduler reserves the estimate when a request is admitted. A trustworthy
terminal usage record replaces that estimate with actual output tokens. If a
client disconnects, the backend fails, or terminal usage is missing or
truncated, the provisional charge remains. This prevents cancellation from
evading fair-share accounting.

Fairness credit accrues only during active contention. When a new or returning
tenant becomes active, its virtual finish is rebased to the current active-set
virtual time, so idle tenants do not bank unlimited catch-up credit. The queue
is work-conserving: a model partition with a free slot and an eligible local
request never idles for an underserved tenant that can use only another model.

One `GlobalFairShare` instance aggregates actual-token accounting across every
partition built by one SMG process. Flat configuration keeps one canonical
tenant virtual finish across partitions. Per-model profiles instead keep
scheduling debt and active-set virtual time separate by canonical model. A
partition that contains multiple models selects the group containing its
oldest eligible waiter before applying that model's policy. Consequently:

- Batch and interactive requests resolve to the same tenant ledger when they
  enter the same SMG process with the same canonical tenant identity. Priority
  classes still decide which class is considered first.
- Configured percentages converge when tenants are simultaneously backlogged
  for the same constrained pool. Exact fleet-wide percentages are not
  enforceable for tenants targeting disjoint pools without idling capacity.
- Per-model service never creates scheduling debt in another model, while
  charged and reserved output-token accounting remains process-global by real
  tenant.
- The ledger does not coordinate separate M1 and M2 gateways, or overlapping
  old and new gateway processes during a rollout. Strict cross-gateway fairness
  requires a distributed ledger or one authoritative admission front door.

### External capacity credits

Capacity credits let an external allocator keep large user backlogs outside
SMG while SMG remains authoritative for real model capacity. The allocator
asks for the next request selected by its per-model fair-share policy, then
calls `POST /internal/capacity-credits`. SMG either returns one short-lived
credit immediately or returns retryable **429** without placing another request
in an internal queue.

Each active credit holds exactly one scheduler slot plus that request's
provisional output-token reservation. The inference request must redeem the
credit once using the exact generation, policy epoch, admission partition,
canonical model, tenant, request ID, and output-token estimate used at issue
time. A mismatched or replayed credit is rejected. Cancellation or expiry
releases the slot and reservation. A successful response replaces the estimate
with terminal actual output tokens; a missing or invalid terminal usage record
keeps the conservative provisional charge. The credit TTL applies only before
redemption and never imposes a deadline on a running inference request.

The issue and cancel routes are mounted only when
`--capacity-credit-generation` is set and use the configured shared service-key
authentication. The trusted proxy must strip client copies and inject the
canonical end-user, model, partition, and estimate headers. Capacity credits
therefore also require preferred trusted tenant identity, trusted model and
estimate headers in `fair_share`, and explicit admission partitions.

When `--capacity-credit-required` is set, a raw inference request without a
valid credit receives retryable **429**. This is the enforcement switch and can
be enabled after observing issue traffic. Local adaptive rejection remains a
handler-side safety backstop; it cancels the redeemed fair-share reservation so
rejected work is not charged output tokens.

With `--priority-scheduler-adaptive-capacity`, engine feedback supplies a total
ceiling for each explicit partition. The scheduler's slot pool remains the sole
authority that subtracts local in-flight work. A falling ceiling stops new
credit issue while existing work drains; a real capacity increase wakes the
allocator-facing path without per-request polling.

Credits are process-local and generation-bound. They do not survive a gateway
handoff and cannot be redeemed by another SMG process. The allocator must treat
gateway generation changes as a new epoch and reissue outstanding work rather
than replaying old credits.

### Preferred trusted tenant identity

By default, tenant resolution remains authenticated caller, then trusted
header, then client IP, then anonymous. A deployment with a shared authenticated
proxy identity can deliberately make the proxy-injected end-user header
canonical by setting all three flags:

```bash
--trust-tenant-header \
--prefer-trusted-tenant-header \
--tenant-header-name x-comet-user
```

`--prefer-trusted-tenant-header` requires `--trust-tenant-header`. A missing,
empty, or invalid preferred header falls back to the existing authenticated
caller path. The option is disabled by default. Enabling it changes the
canonical `RouteRequestMeta` tenant for every tenant-aware subsystem, including
rate-limit policy lookup, tenant metrics, priority clamps, and fair sharing.
The upstream proxy must strip caller-supplied copies and reinject only its
authenticated user identity.

---

## Tenant policy

A tenant's priority ceiling is resolved per request:

1. If the tenant key appears in `tenant_policies`, its `max_class` is used.
2. Otherwise the gateway-wide `--priority-scheduler-default-max-class` applies.

The resolved `max_class` is the upper bound for the [tenant clamp](#tenant-clamp). Tenant keys are the same keys the gateway uses elsewhere for tenancy (for example `auth:acme`).

| Field | Type | Meaning |
|-------|------|---------|
| `max_class` | `system` \| `interactive` \| `default` \| `bulk` | Highest class this tenant may be admitted under. A request's effective class is `min(header_class, max_class)`. |

---

## Metrics

The scheduler exposes these Prometheus metrics (see the [Metrics Reference](metrics.md) for the full catalog):

| Metric | Type | Key labels | Use |
|--------|------|------------|-----|
| `smg_scheduler_admit_total` | Counter | `class`, `outcome` | Admission outcomes (`admitted`, `rejected_queue_full`, `rejected_queue_timeout`, `preempted`, `client_cancelled`). |
| `smg_scheduler_queue_wait_seconds` | Histogram | `class` | Time spent queued before admission, timeout, or cancel. |
| `smg_scheduler_preemption_total` | Counter | `victim_class`, `by_class` | Successful preemptions. Authoritative preemption count. |
| `smg_scheduler_clamp_total` | Counter | `tenant`, `requested_class`, `effective_class` | Requests clamped below the class they asked for. |
| `smg_scheduler_unknown_priority_value_total` | Counter | `tenant` | Requests with an unrecognized `x-smg-priority` value. |
| `smg_scheduler_starvation_promotion_total` | Counter | `class` | Waiters admitted via the starvation override. |
| `smg_scheduler_inflight` | Gauge | `class` | Current in-flight requests per class. |
| `smg_scheduler_queue_depth` | Gauge | `class` | Current queued waiters per class. |
| `smg_scheduler_queue_size_limit` | Gauge | `class` | Configured queue limit per class. |
| `smg_scheduler_utilization` | Gauge | — | Total in-flight divided by backend capacity. |
| `smg_scheduler_class_capacity_pressure` | Gauge | `class` | Normalized 0.0–1.0 pressure (worse of queue and slot pressure). |
| `smg_fair_share_charged_output_tokens_total` | Counter | `tenant` | Actual or conservative fallback output tokens charged across the process-local ledger. |
| `smg_fair_share_virtual_finish` | Gauge | `model`, `tenant` | Active-set normalized tenant virtual finish. Flat mode uses `model="global"`. |
| `smg_fair_share_other_bucket_virtual_finish` | Gauge | `model` | Outer virtual finish of a model profile's aggregate `other` bucket. |
| `smg_fair_share_reserved_output_tokens` | Gauge | `tenant` | Provisional output-token charges held by active requests. |
| `smg_fair_share_queue_wait_seconds` | Histogram | `model`, `tenant`, `class` | Fair-share queue wait by model profile, tenant, and priority class. |
| `smg_fair_share_fallback_total` | Counter | `reason` | Settlement or estimate paths that used a configured fallback. |
| `smg_fair_share_unknown_tenant_total` | Counter | `tenant` | Requests whose canonical tenant has no explicit configured weight. |
| `smg_capacity_credit_operations_total` | Counter | `partition`, `outcome` | Bounded lifecycle outcomes including issue, idempotent retry, redemption, cancellation, expiry, no capacity, bad binding, and replay. |
| `smg_capacity_credit_active` | Gauge | `partition` | Active unredeemed credits currently holding scheduler slots. |

---

## See also

<div class="grid" markdown>

<div class="card" markdown>

### :material-priority-high: Priority Scheduling Concept

How slots, reservations, preemption, and starvation promotion fit together.

[Priority Scheduling →](../concepts/reliability/priority-scheduling.md)

</div>

<div class="card" markdown>

### :material-cog: Configuration Reference

All gateway CLI flags and configuration options.

[Configuration →](configuration.md)

</div>

<div class="card" markdown>

### :material-chart-box: Metrics Reference

Full catalog of Prometheus metrics.

[Metrics →](metrics.md)

</div>

</div>
