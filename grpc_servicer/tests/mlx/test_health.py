"""Tests for MlxHealthServicer."""

import asyncio
from unittest.mock import AsyncMock, MagicMock

import pytest
from grpc_health.v1 import health_pb2

from smg_grpc_servicer.mlx.health_servicer import MlxHealthServicer


@pytest.fixture
def servicer():
    return MlxHealthServicer()


@pytest.fixture
def context():
    ctx = AsyncMock()
    ctx.set_code = MagicMock()
    ctx.set_details = MagicMock()
    return ctx


class TestCheck:
    @pytest.mark.asyncio
    async def test_not_serving_before_set(self, servicer, context):
        request = health_pb2.HealthCheckRequest(service="")
        resp = await servicer.Check(request, context)
        assert resp.status == health_pb2.HealthCheckResponse.NOT_SERVING

    @pytest.mark.asyncio
    async def test_serving_after_set(self, servicer, context):
        servicer.set_serving()
        request = health_pb2.HealthCheckRequest(service="")
        resp = await servicer.Check(request, context)
        assert resp.status == health_pb2.HealthCheckResponse.SERVING

    @pytest.mark.asyncio
    async def test_not_serving_after_shutdown(self, servicer, context):
        servicer.set_serving()
        servicer.set_not_serving()
        request = health_pb2.HealthCheckRequest(service="")
        resp = await servicer.Check(request, context)
        assert resp.status == health_pb2.HealthCheckResponse.NOT_SERVING

    @pytest.mark.asyncio
    async def test_vllm_service_name(self, servicer, context):
        servicer.set_serving()
        request = health_pb2.HealthCheckRequest(service="vllm.grpc.engine.VllmEngine")
        resp = await servicer.Check(request, context)
        assert resp.status == health_pb2.HealthCheckResponse.SERVING

    @pytest.mark.asyncio
    async def test_unknown_service(self, servicer, context):
        servicer.set_serving()
        request = health_pb2.HealthCheckRequest(service="unknown.Service")
        resp = await servicer.Check(request, context)
        assert resp.status == health_pb2.HealthCheckResponse.SERVICE_UNKNOWN
        context.set_code.assert_called()


class TestWatch:
    @pytest.mark.asyncio
    async def test_watch_yields_current_status(self, servicer, context):
        servicer.set_serving()
        request = health_pb2.HealthCheckRequest(service="")
        responses = []
        async for resp in servicer.Watch(request, context):
            responses.append(resp)
        assert len(responses) == 1
        assert responses[0].status == health_pb2.HealthCheckResponse.SERVING
