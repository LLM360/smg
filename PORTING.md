# Porting notes: comet/v1.15.0

This branch is upstream `smg-project/smg` tag `v1.15.0` (82c0f1fc) plus the
LLM360 fork features that Comet uses today. It is not a merge of the fork
`prod` history. Fork `prod` d636bcd1 is the source of the ported code.

Scope rules (owner decisions):

- Port only the features that Comet uses today.
- Prefer the upstream code over the fork code.
- Keep the flag names and the JSON/YAML schemas that Comet renders.
- Do not port: adaptive admission, distribution seeds, owner-pressure routing,
  `max_cached_owners_per_prefix`, owner-spill cooldown, cache-aware engine load,
  least-load cache credit, `size_aware_power_of_two`,
  `--priority-scheduler-adaptive-capacity`, `--output-token-estimate`,
  global rate limit.

Use these upstream mechanisms (no port): priority scheduler and its classes
file, `/workers` API with `PATCH` labels, `cache_aware` thresholds, health and
queue flags, `--enable-igw`, the standard `smg_*` metrics.

## Feature table

| # | Feature | Fork source | Adaptation to upstream v1.15.0 | Tests ported |
|---|---|---|---|---|
| 1 | `fair_share` block and per-model tenant scheduler | #14-era scheduler work: ffa7d67d, a4770e85, 76506248, 40d9117d, 5364c5df, f6163ad9; #21 (72db928c), #24 (7ddd2782); `middleware/scheduler/{fair_share,body,output_tokens}.rs` | Fork scheduler files are taken from the merge of fork `prod` into v1.15.0 (the WIP tree) and joined with the upstream `engine`, `queue`, `state`, `config`. The edge body (`edge_body.rs`) gets `data_frame` and `body_finished` hooks so the permit settles the output tokens when the body ends. `worker/capacity.rs` keeps a global `max_capacity` ceiling. `error.rs` keeps the retry-after estimate. | `fair_share.rs` (15), `body.rs` (5), `output_tokens.rs` (3), `engine.rs`, `queue.rs`, `config.rs`, `error.rs`, `token_bucket.rs`, `worker/capacity.rs` unit tests |
| 2 | Admission partitions, `capacity_from_healthy_replicas`, `smg_scheduler_partition_*` metrics | #27 (b02c6376), #28 (e96a1d87), 4a307e39 | `SchedulerState` owns one `PriorityScheduler` per partition. A coordinator task recomputes partition capacity when the worker set changes. Trusted header `x-smg-admission-partition`; worker label `admission_partition`. Metrics `smg_scheduler_partition_{inflight,queue_depth,capacity,queue_size_limit,healthy_replicas,...}` in `scheduler/metrics.rs`. The adaptive capacity provider hook is removed. | `state.rs` (11), `config.rs` partition validation tests |
| 3 | `smg_http_admission_*` metrics | #14 (7ede15d3), #19 (102fe311) | `middleware/admission_metrics.rs` guards (active, queued, pending) are shared by the legacy concurrency limiter and the priority scheduler. Metric functions and `describe_*` calls are in `observability/metrics.rs`. `Metrics::initialize_http_admission` runs in `startup` right after `start_prometheus` installs the recorder (it ran before it in the first port, so the process-start gauge, the zero counters and the limit gauges were lost). The scheduler sampler replaces `smg_http_admission_limit` with the sum of the partition limits on its first tick. | `tests/serial/scheduler_admission_test.rs::priority_scheduler_updates_generic_http_admission_metrics`, `tests/startup/http_admission_series_startup_test.rs` (first scrape of a started gateway), `concurrency.rs` tests |
| 4 | Capacity credit, trusted `x-smg-*` headers, `--prefer-trusted-tenant-header` | 59e1f782, #59 (8cf8b1d0), #62 (2653d7d5), #66 (3819111d); prefer-trusted in ffa7d67d and 8cf8b1d0 | `capacity_credit.rs` (registry) and `capacity_credit_api.rs` (issue, arm, cancel, capabilities) are mounted under `/internal/capacity-credits` on the control plane with the shared-key auth. `admission_proof.rs` is the one-shot proof of a redeemed credit; its consumer (adaptive target binding) is not ported, so it is only type-checked. Flags `--capacity-credit-{generation,ttl-ms,terminal-retention-secs,required}`. `tenant_resolution.rs` reads the trusted header first when `prefer_trusted_tenant_header` is set. Trusted headers `x-smg-output-token-estimate`, `x-smg-request-model`, `x-smg-admission-partition` are read by the scheduler (`fair_share.rs`, `state.rs`). | `capacity_credit.rs` (13), `capacity_credit_api.rs` (11), `admission_proof.rs` (3), `tenant_resolution.rs` (3 new) |
| 5 | Trusted worker constraints `x-smg-target-worker-url`, `x-smg-excluded-worker-urls` | #16 (b75ca608), #29 (4736d3e8) | The fork filtered inside its own cache-aware and gRPC selection code. Upstream v1.15.0 has one choke point, `routers/common/placement.rs`. The filter runs in `select_from` and `select_pair`, so HTTP, gRPC and PD placement all obey it. Helpers `worker_url_is_allowed` and `has_worker_constraints` are in `crates/external_router/src/header_utils.rs`. The common `WorkerSelector` in `routers/common/worker_selection.rs` is not changed. | `header_utils.rs::test_comet_worker_target_and_exclusions`, `placement.rs::trusted_worker_constraints_narrow_the_candidates` |
| 6 | `--model-policy MODEL=POLICY` | #20 (9d788ab9, only the model-policy part) | `RouterConfig.model_policies` holds fully built `PolicyConfig` values. `PolicyRegistry::with_model_policies` makes them win over the default policy and over the worker policy-hint label. `size_aware_power_of_two` is not accepted. | `registry.rs::test_explicit_model_policy_overrides_default_and_worker_hint`, Python `TestParseRouterArgs` model-policy tests |

Python CLI (`bindings/python`): the image runs `python -m smg.launch_router`,
so each ported flag is also in `router_args.py`, `router.py` and `lib.rs`.

## Not ported (decision: owner)

Adaptive admission and its flags, distribution seeds, owner-pressure routing,
`--max-cached-owners-per-prefix`, `--cache-owner-spill-cooldown-secs`,
`--cache-aware-engine-load`, least-load cache credit flags,
`size_aware_power_of_two`, `--output-token-estimate`,
`--priority-scheduler-adaptive-capacity`, `--global-rate-limit-requests-per-second`,
the fork load-snapshot compatibility (`monitor.rs` SGLang `/get_load` array),
fork KV-event sequence-gap recovery, fork gRPC servicer patches.

Comet sets `deepseek-v4-flash-0731` to `least_load` instead of
`size_aware_power_of_two`.

## gRPC wire compatibility with the deployed servicer

The deployed `smg-grpc-servicer` overlay pairs with `smg-grpc-proto` 0.4.14. A
router from this branch must still talk to it, so the proto change from 0.4.14
to v1.15.0 is additive only. The fork changes no proto file; the difference is
upstream's own 0.4.14 to v1.15.0 delta.

- No message is removed and no field changes number or wire type.
- `sglang_scheduler.proto` `GenerateRequest.data_parallel_rank` stays field 16
  and moves from implicit to `optional int32`, so an unset rank stays off the
  wire and a pin to rank 0 is told from no pin.
- `common.proto` `KvCacheCleared` gains `optional string ownership = 1`. An
  empty message from 0.4.14 still decodes.
- New messages and new optional fields are additive. A peer that does not know
  a field ignores it.
- `TensorData.dtype` and `GenerateComplete.finish_reason` gain comments only.

`crates/grpc_client/python/tests/test_wire_compat.py` holds the 0.4.14 field
inventory and checks every deployed field against the generated descriptors. It
fails when a field moves number or wire type, when a message disappears, or when
a new field reuses a deployed number.

## Commits

Fork code for the scheduler is interleaved: `state.rs`, `engine.rs`, `config.rs`
and `metrics.rs` hold the code of features 1 and 2 together, and the plumbing
files (`main.rs`, `cli/configure.rs`, `config/types.rs`, `bindings/python`)
hold the flags of features 4 and 6. Commits split by file and by hunk where
the code allows it. A single commit of a shared file can therefore not compile
alone; the branch tip compiles.
