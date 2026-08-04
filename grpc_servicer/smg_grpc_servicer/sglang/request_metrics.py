"""Metric helpers for SGLang gRPC generation requests."""

from __future__ import annotations

import copy
import inspect
from collections.abc import Mapping
from typing import Any


def streaming_scheduler_request(obj: Any) -> Any:
    """Return a shallow request copy that makes scheduler output observable."""
    scheduler_obj = copy.copy(obj)
    scheduler_obj.stream = True
    return scheduler_obj


def request_logs_metrics(obj: Any) -> bool:
    """Return whether this SGLang request should emit tokenizer metrics."""
    return not getattr(obj, "no_logs", False) and getattr(obj, "log_metrics", True)


def disable_request_metrics(obj: Any) -> None:
    """Suppress metrics across old ``no_logs`` and new ``log_metrics`` requests."""
    if hasattr(obj, "no_logs"):
        obj.no_logs = True
    if hasattr(obj, "log_metrics"):
        obj.log_metrics = False


def metric_suppression_kwargs(request_type: Any) -> dict[str, bool]:
    """Build compatible constructor kwargs for the installed SGLang request type."""
    try:
        parameters = inspect.signature(request_type).parameters
    except (TypeError, ValueError):
        return {}
    if "log_metrics" in parameters:
        return {"log_metrics": False}
    if "no_logs" in parameters:
        return {"no_logs": True}
    return {}


def _request_has_grammar(obj: Any) -> bool:
    sampling_params = getattr(obj, "sampling_params", None)
    grammar_fields = ("json_schema", "regex", "ebnf", "structural_tag")
    if isinstance(sampling_params, Mapping):
        return any(sampling_params.get(field) for field in grammar_fields)
    return any(getattr(sampling_params, field, None) for field in grammar_fields)


def observe_generation_metrics(
    collector: Any,
    state: Any,
    *,
    prompt_tokens: int,
    completion_tokens: int,
    cached_tokens: int,
    observe_ttft: bool,
) -> None:
    """Mirror SGLang tokenizer metrics for one gRPC scheduler output."""
    if collector is None or not request_logs_metrics(state.obj):
        return

    labels = dict(collector.labels)
    custom_labels = getattr(state.obj, "custom_labels", None)
    if isinstance(custom_labels, Mapping):
        labels.update({key: value for key, value in custom_labels.items() if key in labels})
    priority = getattr(state.obj, "priority", None)
    if "priority" in labels and priority is not None:
        labels["priority"] = str(priority)

    if not state.ttft_observed and observe_ttft:
        state.ttft_observed = True
        state.last_completion_tokens = completion_tokens
        collector.observe_time_to_first_token(
            labels,
            state.time_stats.get_first_token_latency(),
        )
    else:
        num_new_tokens = completion_tokens - state.last_completion_tokens
        if num_new_tokens > 0:
            collector.observe_inter_token_latency(
                labels,
                state.time_stats.get_interval(),
                num_new_tokens,
            )
            state.time_stats.set_last_time()
            state.last_completion_tokens = completion_tokens

    if state.finished:
        collector.observe_one_finished_request(
            labels,
            prompt_tokens,
            completion_tokens,
            cached_tokens,
            state.time_stats.get_e2e_latency(),
            _request_has_grammar(state.obj),
        )
