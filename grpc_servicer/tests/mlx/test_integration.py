"""Integration test: gRPC round-trip with mock BatchGenerator."""

import asyncio
import time
from concurrent import futures
from dataclasses import dataclass
from typing import Optional

import grpc
import pytest
import pytest_asyncio
from smg_grpc_proto import mlx_engine_pb2, mlx_engine_pb2_grpc

from smg_grpc_servicer.mlx.health_servicer import MlxHealthServicer
from smg_grpc_servicer.mlx.servicer import MlxEngineServicer


@dataclass
class FakeGenResponse:
    uid: int
    token: int
    logprobs: object
    finish_reason: Optional[str] = None
    current_state: Optional[str] = None
    match_sequence: Optional[list] = None
    prompt_cache: Optional[list] = None
    all_tokens: Optional[list] = None


class FakeLogprobs:
    def __getitem__(self, idx):
        return type("S", (), {"item": lambda self: -0.5})()

    @property
    def shape(self):
        return (100,)


class FakeBatchGenerator:
    def __init__(self):
        self._responses = {}
        self._insert_count = 0
        self._removed = []

    def insert(self, prompts, max_tokens=None, samplers=None,
               logits_processors=None, state_machines=None):
        uids = []
        for _ in prompts:
            uid = self._insert_count
            self._insert_count += 1
            lp = FakeLogprobs()
            self._responses[uid] = [
                FakeGenResponse(uid=uid, token=100 + i, logprobs=lp)
                for i in range(3)
            ] + [
                FakeGenResponse(uid=uid, token=2, logprobs=lp,
                                finish_reason="stop", match_sequence=[2])
            ]
            uids.append(uid)
        return uids

    def next(self):
        gen_responses = []
        for uid, responses in list(self._responses.items()):
            if uid not in self._removed and responses:
                gen_responses.append(responses.pop(0))
        return [], gen_responses

    def remove(self, uids):
        self._removed.extend(uids)

    def close(self):
        pass


@pytest_asyncio.fixture
async def grpc_server():
    """Start a gRPC server with mock MLX servicer."""
    fake_bg = FakeBatchGenerator()

    servicer = MlxEngineServicer(
        batch_generator=fake_bg,
        model_path="test-model",
        model_dir="/tmp",
        model_config={"vocab_size": 200, "max_position_embeddings": 2048, "eos_token_id": 2},
        eos_token_ids=[2],
        start_time=time.time(),
    )

    health = MlxHealthServicer()

    server = grpc.aio.server(futures.ThreadPoolExecutor(max_workers=4))
    mlx_engine_pb2_grpc.add_MlxEngineServicer_to_server(servicer, server)
    port = server.add_insecure_port("[::]:0")
    await server.start()

    servicer.start_generation_loop()
    health.set_serving()

    yield port, servicer, health

    servicer.stop_generation_loop()
    await server.stop(0)


@pytest.mark.asyncio
async def test_streaming_round_trip(grpc_server):
    """Full gRPC round-trip: client → server → streaming response."""
    port, servicer, health = grpc_server

    async with grpc.aio.insecure_channel(f"localhost:{port}") as channel:
        stub = mlx_engine_pb2_grpc.MlxEngineStub(channel)

        request = mlx_engine_pb2.GenerateRequest(
            request_id="int-test-1",
            tokenized=mlx_engine_pb2.TokenizedInput(input_ids=[1, 2, 3]),
            sampling_params=mlx_engine_pb2.SamplingParams(),
            stream=True,
        )

        responses = []
        async for resp in stub.Generate(request):
            responses.append(resp)

        # 4 chunks (tokens 100, 101, 102, 2) + 1 complete
        chunks = [r for r in responses if r.HasField("chunk")]
        completes = [r for r in responses if r.HasField("complete")]
        assert len(chunks) == 4
        assert len(completes) == 1
        assert completes[0].complete.finish_reason == "stop"


@pytest.mark.asyncio
async def test_non_streaming_round_trip(grpc_server):
    """Full gRPC round-trip: non-streaming returns all tokens at once."""
    port, servicer, health = grpc_server

    async with grpc.aio.insecure_channel(f"localhost:{port}") as channel:
        stub = mlx_engine_pb2_grpc.MlxEngineStub(channel)

        request = mlx_engine_pb2.GenerateRequest(
            request_id="int-test-2",
            tokenized=mlx_engine_pb2.TokenizedInput(input_ids=[1, 2, 3]),
            sampling_params=mlx_engine_pb2.SamplingParams(),
            stream=False,
        )

        responses = []
        async for resp in stub.Generate(request):
            responses.append(resp)

        assert len(responses) == 1
        assert responses[0].HasField("complete")
        assert list(responses[0].complete.output_ids) == [100, 101, 102, 2]
        assert responses[0].complete.finish_reason == "stop"
