"""Tests for GetTokenizer RPC helper: zip creation, chunking, SHA256."""

import hashlib
import io
import json
import os
import tempfile
import zipfile

import pytest

from smg_grpc_servicer.mlx.servicer import MlxEngineServicer


@pytest.fixture
def tokenizer_dir():
    with tempfile.TemporaryDirectory() as tmpdir:
        with open(os.path.join(tmpdir, "tokenizer.json"), "w") as f:
            json.dump({"version": "1.0", "model": {"type": "BPE"}}, f)
        with open(os.path.join(tmpdir, "tokenizer_config.json"), "w") as f:
            json.dump({"model_max_length": 4096}, f)
        with open(os.path.join(tmpdir, "special_tokens_map.json"), "w") as f:
            json.dump({"eos_token": "</s>"}, f)
        with open(os.path.join(tmpdir, "config.json"), "w") as f:
            json.dump({"hidden_size": 4096}, f)
        yield tmpdir


class TestBuildTokenizerZip:
    def test_creates_valid_zip(self, tokenizer_dir):
        zip_bytes, sha256 = MlxEngineServicer._build_tokenizer_zip(tokenizer_dir)
        assert len(zip_bytes) > 0
        zf = zipfile.ZipFile(io.BytesIO(zip_bytes))
        names = zf.namelist()
        assert "tokenizer.json" in names
        assert "tokenizer_config.json" in names
        assert "special_tokens_map.json" in names

    def test_sha256_matches(self, tokenizer_dir):
        zip_bytes, sha256 = MlxEngineServicer._build_tokenizer_zip(tokenizer_dir)
        expected = hashlib.sha256(zip_bytes).hexdigest()
        assert sha256 == expected

    def test_excludes_non_tokenizer_files(self, tokenizer_dir):
        zip_bytes, _ = MlxEngineServicer._build_tokenizer_zip(tokenizer_dir)
        zf = zipfile.ZipFile(io.BytesIO(zip_bytes))
        names = zf.namelist()
        assert "config.json" not in names


class TestChunkTokenizerZip:
    def test_single_chunk_for_small_zip(self, tokenizer_dir):
        zip_bytes, sha256 = MlxEngineServicer._build_tokenizer_zip(tokenizer_dir)
        chunks = list(MlxEngineServicer._chunk_tokenizer_zip(zip_bytes, sha256, chunk_size=1024 * 1024))
        assert len(chunks) == 1
        assert chunks[0].sha256 == sha256
        assert chunks[0].data == zip_bytes

    def test_multiple_chunks(self, tokenizer_dir):
        zip_bytes, sha256 = MlxEngineServicer._build_tokenizer_zip(tokenizer_dir)
        chunks = list(MlxEngineServicer._chunk_tokenizer_zip(zip_bytes, sha256, chunk_size=50))
        assert len(chunks) > 1
        for chunk in chunks[:-1]:
            assert chunk.sha256 == ""
        assert chunks[-1].sha256 == sha256
        reassembled = b"".join(c.data for c in chunks)
        assert reassembled == zip_bytes
