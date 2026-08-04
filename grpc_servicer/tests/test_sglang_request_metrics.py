import importlib.util
from pathlib import Path
from types import SimpleNamespace

_MODULE_PATH = Path(__file__).parents[1] / "smg_grpc_servicer" / "sglang" / "request_metrics.py"
_SPEC = importlib.util.spec_from_file_location("sglang_request_metrics", _MODULE_PATH)
assert _SPEC is not None and _SPEC.loader is not None
_MODULE = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(_MODULE)
observe_generation_metrics = _MODULE.observe_generation_metrics
disable_request_metrics = _MODULE.disable_request_metrics
metric_suppression_kwargs = _MODULE.metric_suppression_kwargs
request_logs_metrics = _MODULE.request_logs_metrics
streaming_scheduler_request = _MODULE.streaming_scheduler_request


class FakeTimeStats:
    def __init__(self) -> None:
        self.last_updates = 0

    def get_first_token_latency(self) -> float:
        return 1.25

    def get_interval(self) -> float:
        return 0.4

    def set_last_time(self) -> None:
        self.last_updates += 1

    def get_e2e_latency(self) -> float:
        return 2.5


class FakeCollector:
    def __init__(self) -> None:
        self.labels = {"model_name": "kimi-k3", "engine_type": "unified"}
        self.ttft = []
        self.tpot = []
        self.finished = []

    def observe_time_to_first_token(self, labels, value) -> None:
        self.ttft.append((labels, value))

    def observe_inter_token_latency(self, labels, interval, num_new_tokens) -> None:
        self.tpot.append((labels, interval, num_new_tokens))

    def observe_one_finished_request(self, *args) -> None:
        self.finished.append(args)


def make_state(*, finished: bool = False, log_metrics: bool = True, stream: bool = False):
    return SimpleNamespace(
        obj=SimpleNamespace(
            log_metrics=log_metrics,
            stream=stream,
            sampling_params=SimpleNamespace(
                json_schema=None,
                regex=None,
                ebnf=None,
                structural_tag=None,
            ),
        ),
        time_stats=FakeTimeStats(),
        ttft_observed=False,
        last_completion_tokens=1,
        finished=finished,
    )


def test_non_streaming_scheduler_copy_preserves_client_mode():
    client_request = SimpleNamespace(stream=False, request_id="req-1")

    scheduler_request = streaming_scheduler_request(client_request)

    assert scheduler_request is not client_request
    assert scheduler_request.stream is True
    assert client_request.stream is False


def test_generation_metrics_record_ttft_then_tpot_and_finished_request():
    collector = FakeCollector()
    state = make_state()

    observe_generation_metrics(
        collector,
        state,
        prompt_tokens=100,
        completion_tokens=3,
        cached_tokens=40,
        observe_ttft=True,
    )
    assert collector.ttft == [({"model_name": "kimi-k3", "engine_type": "unified"}, 1.25)]
    assert collector.tpot == []
    assert state.last_completion_tokens == 3

    state.finished = True
    observe_generation_metrics(
        collector,
        state,
        prompt_tokens=100,
        completion_tokens=7,
        cached_tokens=40,
        observe_ttft=True,
    )

    assert collector.tpot == [({"model_name": "kimi-k3", "engine_type": "unified"}, 0.4, 4)]
    assert state.time_stats.last_updates == 1
    assert collector.finished == [
        (
            {"model_name": "kimi-k3", "engine_type": "unified"},
            100,
            7,
            40,
            2.5,
            False,
        )
    ]


def test_older_collector_receives_external_client_stream_mode():
    class OlderCollector(FakeCollector):
        def observe_time_to_first_token(self, labels, value, *, stream) -> None:
            self.ttft.append((labels, value, stream))

    non_streaming_collector = OlderCollector()
    non_streaming_state = make_state(stream=False)
    observe_generation_metrics(
        non_streaming_collector,
        non_streaming_state,
        prompt_tokens=100,
        completion_tokens=3,
        cached_tokens=40,
        observe_ttft=True,
    )

    streaming_collector = OlderCollector()
    streaming_state = make_state(stream=True)
    observe_generation_metrics(
        streaming_collector,
        streaming_state,
        prompt_tokens=100,
        completion_tokens=3,
        cached_tokens=40,
        observe_ttft=True,
    )

    labels = {"model_name": "kimi-k3", "engine_type": "unified"}
    assert non_streaming_collector.ttft == [(labels, 1.25, False)]
    assert streaming_collector.ttft == [(labels, 1.25, True)]


def test_older_collector_records_tpot_and_finished_request():
    class OlderCollector(FakeCollector):
        def observe_time_to_first_token(self, labels, value, *, stream) -> None:
            self.ttft.append((labels, value, stream))

    collector = OlderCollector()
    state = make_state(stream=False)
    observe_generation_metrics(
        collector,
        state,
        prompt_tokens=100,
        completion_tokens=3,
        cached_tokens=40,
        observe_ttft=True,
    )
    state.finished = True
    observe_generation_metrics(
        collector,
        state,
        prompt_tokens=100,
        completion_tokens=7,
        cached_tokens=40,
        observe_ttft=True,
    )

    labels = {"model_name": "kimi-k3", "engine_type": "unified"}
    assert collector.tpot == [(labels, 0.4, 4)]
    assert collector.finished == [(labels, 100, 7, 40, 2.5, False)]


def test_collector_internal_type_error_is_not_masked():
    class BrokenCollector(FakeCollector):
        def observe_time_to_first_token(self, labels, value, *, stream) -> None:
            raise TypeError("collector implementation failed")

    collector = BrokenCollector()
    state = make_state()

    try:
        observe_generation_metrics(
            collector,
            state,
            prompt_tokens=100,
            completion_tokens=3,
            cached_tokens=40,
            observe_ttft=True,
        )
    except TypeError as exc:
        assert str(exc) == "collector implementation failed"
    else:
        raise AssertionError("collector TypeError should propagate")


def test_generation_metrics_skip_health_checks():
    collector = FakeCollector()
    state = make_state(finished=True, log_metrics=False)

    observe_generation_metrics(
        collector,
        state,
        prompt_tokens=1,
        completion_tokens=1,
        cached_tokens=0,
        observe_ttft=True,
    )

    assert collector.ttft == []
    assert collector.tpot == []
    assert collector.finished == []


def test_no_logs_request_skips_metrics_for_older_sglang():
    collector = FakeCollector()
    state = make_state(finished=True)
    state.obj.no_logs = True

    observe_generation_metrics(
        collector,
        state,
        prompt_tokens=1,
        completion_tokens=1,
        cached_tokens=0,
        observe_ttft=True,
    )

    assert collector.ttft == []
    assert collector.tpot == []
    assert collector.finished == []


def test_metric_suppression_uses_installed_request_contract():
    class NewRequest:
        def __init__(self, *, log_metrics=True):
            self.log_metrics = log_metrics

    class OldRequest:
        __slots__ = ("no_logs",)

        def __init__(self, *, no_logs=False):
            self.no_logs = no_logs

    assert metric_suppression_kwargs(NewRequest) == {"log_metrics": False}
    assert metric_suppression_kwargs(OldRequest) == {"no_logs": True}

    old_request = OldRequest()
    disable_request_metrics(old_request)
    assert old_request.no_logs is True
    assert request_logs_metrics(old_request) is False
