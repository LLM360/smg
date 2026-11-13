"""The generated descriptors stay wire-compatible with the deployed servicer.

The deployed `smg-grpc-servicer` image pairs with `smg-grpc-proto` 0.4.14. A
router built from this tree must still exchange messages with that servicer
over a running deployment, so every descriptor difference must be additive:
no message is removed, no field number changes name, and no field changes wire
type.

`DEPLOYED` below is the field inventory of `smg-grpc-proto` 0.4.14, taken from
the `crates/grpc_client/proto/*.proto` sources at the commit that set that
version. Each entry is `(name, number, wire_type)`. This module compares the
installed `smg_grpc_proto` descriptors against it, so it runs offline and reads
no image.

A field that `DEPLOYED` does not name is allowed: it is new, and a peer that
does not know it ignores it. A field that `DEPLOYED` names must still be
present with the same number and the same wire type. Presence may move from
implicit to explicit (`optional`), because an absent explicit field decodes as
absent on an implicit peer and the reverse. A message `DEPLOYED` does not name
is new.
"""

from __future__ import annotations

import pytest

pytest.importorskip("smg_grpc_proto")

from google.protobuf.descriptor import FieldDescriptor  # noqa: E402

import smg_grpc_proto  # noqa: E402

# Field number -> proto wire type. A change inside one wire type (int32 to
# optional int32) is compatible; a change across wire types is not.
_WIRE = {
    FieldDescriptor.TYPE_DOUBLE: 1,
    FieldDescriptor.TYPE_FLOAT: 5,
    FieldDescriptor.TYPE_INT64: 0,
    FieldDescriptor.TYPE_UINT64: 0,
    FieldDescriptor.TYPE_INT32: 0,
    FieldDescriptor.TYPE_FIXED64: 1,
    FieldDescriptor.TYPE_FIXED32: 5,
    FieldDescriptor.TYPE_BOOL: 0,
    FieldDescriptor.TYPE_STRING: 2,
    FieldDescriptor.TYPE_GROUP: 3,
    FieldDescriptor.TYPE_MESSAGE: 2,
    FieldDescriptor.TYPE_BYTES: 2,
    FieldDescriptor.TYPE_UINT32: 0,
    FieldDescriptor.TYPE_ENUM: 0,
    FieldDescriptor.TYPE_SFIXED32: 5,
    FieldDescriptor.TYPE_SFIXED64: 1,
    FieldDescriptor.TYPE_SINT32: 0,
    FieldDescriptor.TYPE_SINT64: 0,
}

# Module -> message -> [(field name, field number, wire type)] as 0.4.14 had it.
DEPLOYED: dict[str, dict[str, list[tuple[str, int, int]]]] = {
    "common_pb2": {
        "FlushCacheRequest": [("timeout_s", 1, 5)],
        "FlushCacheResponse": [("success", 1, 0), ("message", 2, 2)],
        "GetTokenizerChunk": [("data", 1, 2), ("sha256", 2, 2)],
        "GetTokenizerRequest": [],
        "KvBlock": [("block_hash", 1, 0), ("token_ids", 2, 0), ("block_size", 3, 0), ("lora_id", 4, 0), ("cache_level", 5, 0)],
        "KvBlocksRemoved": [("block_hashes", 1, 0), ("cache_level", 2, 0)],
        "KvBlocksStored": [("blocks", 1, 2), ("parent_block_hash", 2, 0)],
        "KvCacheCleared": [],
        "KvCacheEvent": [("event_id", 1, 0), ("stored", 2, 2), ("removed", 3, 2), ("cleared", 4, 2)],
        "KvEventBatch": [("sequence_number", 1, 0), ("timestamp", 2, 1), ("events", 3, 2), ("dp_rank", 4, 0)],
        "ProfileResponse": [("success", 1, 0), ("message", 2, 2)],
        "RemoteTensorHandle": [("transport", 1, 2), ("descriptor", 2, 2), ("nbytes", 3, 0)],
        "ShmHandle": [("name", 1, 2), ("offset", 2, 0), ("nbytes", 3, 0), ("owner_id", 4, 2)],
        "StartProfileRequest": [("output_dir", 1, 2), ("start_step", 2, 0), ("num_steps", 3, 0), ("activities", 4, 2), ("with_stack", 5, 0), ("record_shapes", 6, 0), ("profile_by_stage", 7, 0)],
        "StopProfileRequest": [],
        "SubscribeKvEventsRequest": [("start_sequence_number", 1, 0)],
    },
    "sglang_scheduler_pb2": {
        "AbortRequest": [("request_id", 1, 2), ("reason", 2, 2)],
        "AbortResponse": [("success", 1, 0), ("message", 2, 2)],
        "AggregateMetrics": [("total_running_reqs", 1, 0), ("total_waiting_reqs", 2, 0), ("total_reqs", 3, 0), ("avg_token_usage", 4, 1), ("avg_throughput", 5, 1), ("avg_utilization", 6, 1)],
        "DisaggregatedParams": [("bootstrap_host", 1, 2), ("bootstrap_port", 2, 0), ("bootstrap_room", 3, 0)],
        "DisaggregationMetrics": [("mode", 1, 2), ("prefill_prealloc_queue_reqs", 2, 0), ("prefill_inflight_queue_reqs", 3, 0), ("decode_prealloc_queue_reqs", 4, 0), ("decode_transfer_queue_reqs", 5, 0), ("decode_retracted_queue_reqs", 6, 0), ("kv_transfer_speed_gb_s", 7, 1), ("kv_transfer_latency_ms", 8, 1)],
        "EmbedRequest": [("request_id", 1, 2), ("tokenized", 2, 2), ("mm_inputs", 4, 2), ("sampling_params", 5, 2), ("token_type_ids", 7, 0), ("data_parallel_rank", 8, 0), ("is_cross_encoder", 9, 0), ("texts", 10, 2)],
        "EmbedResponse": [("embedding_dim", 4, 0), ("embedding", 5, 5), ("prompt_tokens", 6, 0)],
        "GenerateComplete": [("output_ids", 1, 0), ("finish_reason", 2, 2), ("prompt_tokens", 3, 0), ("completion_tokens", 4, 0), ("cached_tokens", 5, 0), ("output_logprobs", 6, 2), ("all_hidden_states", 7, 2), ("matched_token_id", 8, 0), ("matched_stop_str", 9, 2), ("reasoning_tokens", 12, 0)],
        "GenerateRequest": [("request_id", 1, 2), ("tokenized", 2, 2), ("mm_inputs", 3, 2), ("sampling_params", 4, 2), ("return_logprob", 5, 0), ("logprob_start_len", 6, 0), ("top_logprobs_num", 7, 0), ("token_ids_logprob", 8, 0), ("return_hidden_states", 9, 0), ("disaggregated_params", 10, 2), ("custom_logit_processor", 11, 2), ("timestamp", 12, 2), ("log_metrics", 13, 0), ("input_embeds", 14, 5), ("lora_id", 15, 2), ("data_parallel_rank", 16, 0), ("stream", 17, 0), ("require_reasoning", 18, 0)],
        "GenerateResponse": [("request_id", 1, 2), ("chunk", 2, 2), ("complete", 3, 2)],
        "GenerateStreamChunk": [("token_ids", 1, 0), ("prompt_tokens", 2, 0), ("completion_tokens", 3, 0), ("cached_tokens", 4, 0), ("output_logprobs", 5, 2), ("hidden_states", 6, 5), ("input_logprobs", 7, 2), ("index", 8, 0), ("reasoning_tokens", 9, 0)],
        "GetInternalStateRequest": [("state_keys", 1, 2)],
        "GetInternalStateResponse": [("state", 1, 2)],
        "GetLoadsRequest": [("dp_rank", 1, 0), ("include", 2, 2)],
        "GetLoadsResponse": [("timestamp", 1, 2), ("version", 2, 2), ("dp_rank_count", 3, 0), ("loads", 4, 2), ("aggregate", 5, 2)],
        "GetModelInfoRequest": [],
        "GetModelInfoResponse": [("model_path", 1, 2), ("tokenizer_path", 2, 2), ("is_generation", 3, 0), ("preferred_sampling_params", 4, 2), ("weight_version", 5, 2), ("served_model_name", 6, 2), ("max_context_length", 7, 0), ("vocab_size", 8, 0), ("supports_vision", 9, 0), ("model_type", 10, 2), ("eos_token_ids", 11, 0), ("pad_token_id", 12, 0), ("bos_token_id", 13, 0), ("max_req_input_len", 14, 0), ("architectures", 15, 2), ("id2label_json", 16, 2), ("num_labels", 17, 0), ("default_sampling_params_json", 20, 2)],
        "GetServerInfoRequest": [],
        "GetServerInfoResponse": [("server_args", 1, 2), ("scheduler_info", 2, 2), ("active_requests", 3, 0), ("is_paused", 4, 0), ("last_receive_timestamp", 5, 1), ("uptime_seconds", 6, 1), ("sglang_version", 7, 2), ("server_type", 8, 2), ("start_time", 9, 2), ("max_total_num_tokens", 10, 0)],
        "HealthCheckRequest": [],
        "HealthCheckResponse": [("healthy", 1, 0), ("message", 2, 2)],
        "HiddenStates": [("values", 1, 5), ("layer", 2, 0), ("position", 3, 0)],
        "InputLogProbs": [("token_logprobs", 1, 2), ("token_ids", 2, 0), ("top_logprobs", 3, 2)],
        "InputTokenLogProb": [("value", 1, 5)],
        "ListLoadedLoRAAdaptersRequest": [],
        "ListLoadedLoRAAdaptersResponse": [("loaded_adapters", 1, 2)],
        "LoRAMetrics": [("slots_used", 1, 0), ("slots_total", 2, 0), ("utilization", 3, 1)],
        "LoadLoRAAdapterRequest": [("lora_name", 1, 2), ("lora_path", 2, 2), ("pinned", 3, 0), ("lora_id", 4, 2)],
        "LoadLoRAAdapterResponse": [("success", 1, 0), ("message", 2, 2), ("loaded_lora_ids", 3, 2)],
        "LoadedLoRAAdapter": [("lora_id", 1, 2), ("lora_name", 2, 2), ("lora_path", 3, 2), ("pinned", 4, 0)],
        "MemoryMetrics": [("weight_gb", 1, 1), ("kv_cache_gb", 2, 1), ("graph_gb", 3, 1), ("token_capacity", 4, 0)],
        "MultimodalInputs": [("image_urls", 1, 2), ("video_urls", 2, 2), ("audio_urls", 3, 2), ("pixel_values", 4, 2), ("image_data", 5, 2), ("video_data", 6, 2), ("audio_data", 7, 2), ("modalities", 8, 2), ("im_token_id", 10, 0), ("mm_placeholders", 11, 2)],
        "OutputLogProbs": [("token_logprobs", 1, 5), ("token_ids", 2, 0), ("top_logprobs", 3, 2)],
        "PlaceholderRange": [("offset", 1, 0), ("length", 2, 0)],
        "QueueMetrics": [("waiting", 1, 0), ("grammar", 2, 0), ("paused", 3, 0), ("retracted", 4, 0)],
        "SamplingParams": [("temperature", 1, 5), ("top_p", 2, 5), ("top_k", 3, 0), ("min_p", 4, 5), ("frequency_penalty", 5, 5), ("presence_penalty", 6, 5), ("repetition_penalty", 7, 5), ("max_new_tokens", 8, 0), ("stop", 9, 2), ("stop_token_ids", 10, 0), ("skip_special_tokens", 11, 0), ("spaces_between_special_tokens", 12, 0), ("regex", 13, 2), ("json_schema", 14, 2), ("ebnf_grammar", 15, 2), ("structural_tag", 16, 2)],
        "SchedulerLoad": [("dp_rank", 1, 0), ("num_running_reqs", 2, 0), ("num_waiting_reqs", 3, 0), ("num_total_reqs", 4, 0), ("num_used_tokens", 5, 0), ("max_total_num_tokens", 6, 0), ("token_usage", 7, 1), ("gen_throughput", 8, 1), ("cache_hit_rate", 9, 1), ("utilization", 10, 1), ("max_running_requests", 11, 0), ("memory", 12, 2), ("speculative", 13, 2), ("lora", 14, 2), ("disaggregation", 15, 2), ("queues", 16, 2), ("num_waiting_uncached_tokens", 17, 0)],
        "SetInternalStateRequest": [("state", 1, 2)],
        "SetInternalStateResponse": [("success", 1, 0), ("message", 2, 2)],
        "SpeculativeMetrics": [("accept_length", 1, 1), ("accept_rate", 2, 1)],
        "TensorData": [("data", 1, 2), ("shape", 2, 0), ("dtype", 3, 2)],
        "TokenizedInput": [("original_text", 1, 2), ("input_ids", 2, 0)],
        "TopLogProbs": [("values", 1, 5), ("token_ids", 2, 0)],
        "UnloadLoRAAdapterRequest": [("lora_name", 1, 2), ("lora_id", 2, 2)],
        "UnloadLoRAAdapterResponse": [("success", 1, 0), ("message", 2, 2), ("loaded_lora_ids", 3, 2)],
        "UpdateWeightsRequest": [("disk_path", 1, 2), ("tensor_data", 2, 2), ("remote_url", 3, 2)],
        "UpdateWeightsResponse": [("success", 1, 0), ("message", 2, 2)],
    },
    "tokenspeed_scheduler_pb2": {
        "AbortRequest": [("request_id", 1, 2), ("reason", 2, 2)],
        "AbortResponse": [("success", 1, 0), ("message", 2, 2)],
        "AggregateMetrics": [("total_running_reqs", 1, 0), ("total_waiting_reqs", 2, 0), ("total_reqs", 3, 0), ("avg_token_usage", 4, 1), ("avg_throughput", 5, 1), ("avg_utilization", 6, 1)],
        "EncodeBootstrapInfo": [("items", 1, 2)],
        "EncodeItemBootstrapInfo": [("item_index", 1, 0), ("bootstrap_host", 2, 2), ("bootstrap_port", 3, 0), ("bootstrap_room", 4, 0)],
        "GenerateComplete": [("output_ids", 1, 0), ("finish_reason", 2, 2), ("prompt_tokens", 3, 0), ("completion_tokens", 4, 0), ("cached_tokens", 5, 0), ("output_logprobs", 6, 2), ("matched_token_id", 7, 0), ("matched_stop_str", 8, 2)],
        "GenerateRequest": [("request_id", 1, 2), ("tokenized", 2, 2), ("sampling_params", 3, 2), ("return_logprob", 4, 0), ("logprob_start_len", 5, 0), ("top_logprobs_num", 6, 0), ("token_ids_logprob", 7, 0), ("stream", 8, 0), ("mm_inputs", 9, 2), ("encode_bootstrap_info", 10, 2), ("kv_bootstrap_info", 11, 2)],
        "GenerateResponse": [("request_id", 1, 2), ("chunk", 2, 2), ("complete", 3, 2)],
        "GenerateStreamChunk": [("token_ids", 1, 0), ("prompt_tokens", 2, 0), ("completion_tokens", 3, 0), ("cached_tokens", 4, 0), ("output_logprobs", 5, 2), ("index", 6, 0)],
        "GetLoadsRequest": [("dp_rank", 1, 0), ("include", 2, 2)],
        "GetLoadsResponse": [("timestamp", 1, 2), ("version", 2, 2), ("dp_rank_count", 3, 0), ("loads", 4, 2), ("aggregate", 5, 2)],
        "GetModelInfoRequest": [],
        "GetModelInfoResponse": [("model_path", 1, 2), ("tokenizer_path", 2, 2), ("served_model_name", 3, 2), ("model_type", 4, 2), ("architectures", 5, 2), ("max_context_length", 6, 0), ("max_req_input_len", 7, 0), ("vocab_size", 8, 0), ("eos_token_ids", 9, 0), ("pad_token_id", 10, 0), ("bos_token_id", 11, 0), ("weight_version", 12, 2), ("default_sampling_params_json", 13, 2), ("supports_vision", 14, 0), ("supports_multimodal", 15, 0), ("supported_modalities", 16, 0), ("model_dtype", 17, 2), ("multimodal_encoder_dtype", 18, 2)],
        "GetServerInfoRequest": [],
        "GetServerInfoResponse": [("server_args", 1, 2), ("scheduler_info", 2, 2), ("active_requests", 3, 0), ("is_paused", 4, 0), ("uptime_seconds", 5, 1), ("max_total_num_tokens", 6, 0), ("tokenspeed_version", 7, 2), ("start_time", 8, 2)],
        "HealthCheckRequest": [],
        "HealthCheckResponse": [("healthy", 1, 0), ("message", 2, 2)],
        "KvBootstrapInfo": [("bootstrap_host", 1, 2), ("bootstrap_port", 2, 0), ("bootstrap_room", 3, 0)],
        "MemoryMetrics": [("weight_gb", 1, 1), ("kv_cache_gb", 2, 1), ("graph_gb", 3, 1), ("token_capacity", 4, 0)],
        "MultimodalInputs": [("items", 1, 2)],
        "MultimodalItem": [("modality", 1, 0), ("content_hash", 2, 2), ("encoder_input", 3, 2), ("placeholders", 5, 2), ("placeholder_token_id", 6, 0)],
        "OutputLogProbs": [("token_logprobs", 1, 5), ("token_ids", 2, 0), ("top_logprobs", 3, 2)],
        "PlaceholderRange": [("offset", 1, 0), ("length", 2, 0)],
        "QueueMetrics": [("waiting", 1, 0), ("grammar", 2, 0), ("paused", 3, 0), ("retracted", 4, 0)],
        "SamplingParams": [("temperature", 1, 5), ("top_p", 2, 5), ("top_k", 3, 0), ("min_p", 4, 5), ("frequency_penalty", 5, 5), ("presence_penalty", 6, 5), ("repetition_penalty", 7, 5), ("max_new_tokens", 8, 0), ("min_new_tokens", 9, 0), ("stop", 10, 2), ("stop_token_ids", 11, 0), ("ignore_eos", 12, 0), ("skip_special_tokens", 13, 0), ("spaces_between_special_tokens", 14, 0), ("n", 15, 0), ("regex", 17, 2), ("json_schema", 18, 2), ("ebnf_grammar", 19, 2), ("structural_tag", 20, 2)],
        "SchedulerLoad": [("dp_rank", 1, 0), ("num_running_reqs", 2, 0), ("num_waiting_reqs", 3, 0), ("num_total_reqs", 4, 0), ("num_used_tokens", 5, 0), ("max_total_num_tokens", 6, 0), ("max_running_requests", 7, 0), ("token_usage", 8, 1), ("gen_throughput", 9, 1), ("cache_hit_rate", 10, 1), ("utilization", 11, 1), ("memory", 12, 2), ("queues", 13, 2), ("num_waiting_uncached_tokens", 14, 0)],
        "TensorData": [("shape", 1, 0), ("dtype", 2, 2), ("inline", 3, 2), ("shm", 4, 2), ("remote", 5, 2)],
        "TokenizedInput": [("input_ids", 1, 0), ("original_text", 2, 2)],
        "TopLogProbs": [("values", 1, 5), ("token_ids", 2, 0)],
    },
    "vllm_engine_pb2": {
        "AbortRequest": [("request_ids", 1, 2)],
        "AbortResponse": [],
        "ChoiceConstraint": [("choices", 1, 2)],
        "EmbedRequest": [("request_id", 1, 2), ("tokenized", 2, 2)],
        "EmbedResponse": [("embedding", 1, 5), ("prompt_tokens", 2, 0), ("embedding_dim", 3, 0)],
        "GenerateComplete": [("output_ids", 1, 0), ("finish_reason", 2, 2), ("prompt_tokens", 3, 0), ("completion_tokens", 4, 0), ("cached_tokens", 5, 0), ("output_logprobs", 6, 2), ("input_logprobs", 7, 2), ("index", 8, 0), ("kv_transfer_params", 9, 2), ("matched_token_id", 10, 0), ("matched_stop_str", 11, 2)],
        "GenerateRequest": [("request_id", 1, 2), ("tokenized", 2, 2), ("text", 3, 2)],
        "GenerateResponse": [("chunk", 1, 2), ("complete", 2, 2)],
        "GenerateStreamChunk": [("token_ids", 1, 0), ("prompt_tokens", 2, 0), ("completion_tokens", 3, 0), ("cached_tokens", 4, 0), ("output_logprobs", 5, 2), ("input_logprobs", 6, 2), ("index", 7, 0)],
        "GetLoadsRequest": [("dp_rank", 1, 0), ("include", 2, 2)],
        "GetLoadsResponse": [("timestamp", 1, 2), ("version", 2, 2), ("dp_rank_count", 3, 0), ("loads", 4, 2)],
        "GetModelInfoRequest": [],
        "GetModelInfoResponse": [("model_path", 1, 2), ("is_generation", 2, 0), ("max_context_length", 3, 0), ("vocab_size", 4, 0), ("supports_vision", 5, 0), ("served_model_name", 6, 2), ("tokenizer_path", 7, 2), ("model_type", 8, 2), ("architectures", 9, 2), ("eos_token_ids", 10, 0), ("pad_token_id", 11, 0), ("bos_token_id", 12, 0), ("max_req_input_len", 13, 0), ("default_sampling_params_json", 14, 2)],
        "GetServerInfoRequest": [],
        "GetServerInfoResponse": [("active_requests", 1, 0), ("is_paused", 2, 0), ("last_receive_timestamp", 3, 1), ("uptime_seconds", 4, 1), ("server_type", 5, 2), ("kv_connector", 6, 2), ("kv_role", 7, 2), ("kv_engine_id", 8, 2), ("data_parallel_size", 9, 0), ("shm_namespace_id", 10, 2)],
        "HealthCheckRequest": [],
        "HealthCheckResponse": [("healthy", 1, 0), ("message", 2, 2)],
        "InputLogProbs": [("token_logprobs", 1, 2), ("token_ids", 2, 0), ("top_logprobs", 3, 2)],
        "InputTokenLogProb": [("value", 1, 5)],
        "KvTransferParams": [("remote_host", 1, 2), ("remote_port", 2, 0)],
        "MultimodalInputs": [("pixel_values", 2, 2), ("im_token_id", 4, 0), ("mm_placeholders", 5, 2), ("mm_hashes", 6, 2), ("batched_keys", 7, 2), ("keep_on_cpu_keys", 9, 2), ("modality", 10, 0)],
        "OutputLogProbs": [("token_logprobs", 1, 5), ("token_ids", 2, 0), ("top_logprobs", 3, 2)],
        "PlaceholderRange": [("offset", 1, 0), ("length", 2, 0)],
        "SamplingParams": [("temperature", 1, 5), ("top_p", 2, 5), ("top_k", 3, 0), ("min_p", 4, 5), ("frequency_penalty", 5, 5), ("presence_penalty", 6, 5), ("repetition_penalty", 7, 5), ("max_tokens", 8, 0), ("min_tokens", 9, 0), ("stop", 10, 2), ("stop_token_ids", 11, 0), ("skip_special_tokens", 12, 0), ("spaces_between_special_tokens", 13, 0), ("ignore_eos", 14, 0), ("n", 15, 0), ("json_schema", 16, 2), ("regex", 17, 2), ("grammar", 18, 2), ("structural_tag", 19, 2), ("json_object", 20, 0), ("choice", 21, 2), ("logprobs", 22, 0), ("prompt_logprobs", 23, 0), ("seed", 24, 0), ("include_stop_str_in_output", 25, 0), ("truncate_prompt_tokens", 27, 0)],
        "SchedulerLoad": [("dp_rank", 1, 0), ("num_running_reqs", 2, 0), ("num_waiting_reqs", 3, 0), ("num_total_reqs", 4, 0), ("num_used_tokens", 5, 0), ("max_total_num_tokens", 6, 0), ("token_usage", 7, 1), ("gen_throughput", 8, 1), ("cache_hit_rate", 9, 1), ("utilization", 10, 1), ("max_running_requests", 11, 0)],
        "TensorData": [("inline", 1, 2), ("shape", 2, 0), ("dtype", 3, 2), ("shm", 4, 2), ("remote", 5, 2)],
        "TokenizedInput": [("original_text", 1, 2), ("input_ids", 2, 0)],
        "TopLogProbs": [("values", 1, 5), ("token_ids", 2, 0)],
    },
    "trtllm_service_pb2": {
        "AbortRequest": [("request_id", 1, 2)],
        "AbortResponse": [("success", 1, 0), ("message", 2, 2)],
        "ContextPhaseParams": [("first_gen_token_id", 1, 0), ("kv_cache_blocks", 2, 2)],
        "DisaggregatedParams": [],
        "EmbedRequest": [("request_id", 1, 2), ("tokenized", 2, 2)],
        "EmbedResponse": [("request_id", 1, 2), ("embedding", 2, 5), ("prompt_tokens", 3, 0)],
        "GenerateComplete": [("output_token_ids", 1, 0), ("sequence_index", 2, 0), ("finish_reason", 3, 2), ("matched_stop_str", 4, 2), ("matched_token_id", 13, 0)],
        "GenerateRequest": [("request_id", 1, 2), ("tokenized", 2, 2), ("sampling_config", 3, 2), ("output_config", 4, 2), ("max_tokens", 5, 0), ("streaming", 6, 0), ("guided_decoding", 11, 2), ("embedding_bias", 12, 5), ("lora_config", 13, 2), ("prompt_tuning_config", 14, 2), ("multimodal_input", 15, 2), ("kv_cache_retention", 16, 2), ("disaggregated_params", 17, 2), ("lookahead_config", 18, 2), ("cache_salt_id", 19, 0), ("arrival_time", 20, 1), ("stop", 21, 2), ("stop_token_ids", 22, 0), ("ignore_eos", 23, 0), ("bad", 24, 2), ("bad_token_ids", 25, 0), ("include_stop_token_in_output", 26, 0)],
        "GenerateResponse": [("request_id", 1, 2), ("chunk", 2, 2), ("complete", 3, 2)],
        "GenerateStreamChunk": [("token_ids", 1, 0), ("sequence_index", 2, 0), ("prompt_tokens", 3, 0), ("completion_tokens", 4, 0), ("cached_tokens", 5, 0), ("logprobs", 6, 2)],
        "GetModelInfoRequest": [],
        "GetModelInfoResponse": [("model_id", 1, 2), ("max_input_len", 2, 0), ("max_seq_len", 3, 0), ("max_batch_size", 4, 0), ("vocab_size", 5, 0), ("hidden_size", 6, 0), ("num_layers", 7, 0), ("num_heads", 8, 0), ("supported_features", 9, 2)],
        "GetServerInfoRequest": [],
        "GetServerInfoResponse": [("version", 1, 2), ("backend", 2, 2), ("tensor_parallel_size", 3, 0), ("pipeline_parallel_size", 4, 0), ("context_parallel_size", 5, 0), ("world_size", 6, 0)],
        "GuidedDecodingParams": [],
        "HealthCheckRequest": [],
        "HealthCheckResponse": [("status", 1, 2)],
        "KvCacheRetentionConfig": [("policy", 1, 2), ("config_json", 2, 2)],
        "LookaheadConfig": [("max_window_size", 1, 0), ("max_ngram_size", 2, 0), ("max_verification_set_size", 3, 0)],
        "LoraConfig": [("task_id", 1, 0), ("weights", 2, 2), ("config_json", 3, 2)],
        "MultimodalInput": [("image_data", 1, 2)],
        "OutputConfig": [("logprobs", 1, 0), ("prompt_logprobs", 2, 0), ("return_context_logits", 3, 0), ("return_generation_logits", 4, 0), ("exclude_input_from_output", 5, 0), ("return_encoder_output", 6, 0), ("return_perf_metrics", 7, 0)],
        "PerfMetrics": [("arrival_time", 1, 1), ("first_scheduled_time", 2, 1), ("first_token_time", 3, 1), ("last_token_time", 4, 1), ("kv_cache_transfer_start", 5, 1), ("kv_cache_transfer_end", 6, 1), ("kv_cache_size", 7, 0)],
        "PromptTuningConfig": [("embedding_table", 1, 2)],
        "SamplingConfig": [("beam_width", 1, 0), ("num_return_sequences", 2, 0), ("top_k", 3, 0), ("top_p", 4, 5), ("top_p_min", 5, 5), ("top_p_reset_ids", 6, 0), ("top_p_decay", 7, 5), ("seed", 8, 0), ("temperature", 9, 5), ("min_tokens", 10, 0), ("beam_search_diversity_rate", 11, 5), ("repetition_penalty", 12, 5), ("presence_penalty", 13, 5), ("frequency_penalty", 14, 5), ("prompt_ignore_length", 15, 0), ("length_penalty", 16, 5), ("early_stopping", 17, 0), ("no_repeat_ngram_size", 18, 0), ("min_p", 19, 5), ("beam_width_array", 20, 0)],
        "TokenLogprob": [("token_id", 1, 0), ("logprob", 2, 5), ("top_logprobs", 3, 2)],
        "TokenizedInput": [("original_text", 1, 2), ("input_token_ids", 2, 0), ("query_token_ids", 3, 0)],
        "TopLogprob": [("token_id", 1, 0), ("logprob", 2, 5)],
    },
    "mlx_engine_pb2": {
        "AbortRequest": [("request_ids", 1, 2)],
        "AbortResponse": [],
        "GenerateComplete": [("output_ids", 1, 0), ("finish_reason", 2, 2), ("prompt_tokens", 3, 0), ("completion_tokens", 4, 0), ("cached_tokens", 5, 0), ("output_logprobs", 6, 2), ("index", 7, 0), ("matched_stop_token_id", 8, 0)],
        "GenerateRequest": [("request_id", 1, 2), ("tokenized", 2, 2), ("text", 3, 2)],
        "GenerateResponse": [("chunk", 1, 2), ("complete", 2, 2)],
        "GenerateStreamChunk": [("token_ids", 1, 0), ("prompt_tokens", 2, 0), ("completion_tokens", 3, 0), ("cached_tokens", 4, 0), ("output_logprobs", 5, 2), ("index", 6, 0)],
        "GetModelInfoRequest": [],
        "GetModelInfoResponse": [("model_path", 1, 2), ("is_generation", 2, 0), ("max_context_length", 3, 0), ("vocab_size", 4, 0), ("served_model_name", 5, 2), ("model_type", 6, 2), ("architectures", 7, 2), ("eos_token_ids", 8, 0), ("pad_token_id", 9, 0), ("bos_token_id", 10, 0), ("max_req_input_len", 11, 0)],
        "GetServerInfoRequest": [],
        "GetServerInfoResponse": [("server_type", 1, 2), ("active_requests", 2, 0), ("uptime_seconds", 3, 1)],
        "HealthCheckRequest": [],
        "HealthCheckResponse": [("healthy", 1, 0), ("message", 2, 2)],
        "OutputLogProbs": [("token_logprobs", 1, 5), ("token_ids", 2, 0), ("top_logprobs", 3, 2)],
        "SamplingParams": [("temperature", 1, 5), ("top_p", 2, 5), ("top_k", 3, 0), ("min_p", 4, 5), ("repetition_penalty", 5, 5), ("frequency_penalty", 6, 5), ("presence_penalty", 7, 5), ("max_tokens", 9, 0), ("stop_token_ids", 10, 0), ("ignore_eos", 11, 0), ("logprobs", 12, 0), ("seed", 13, 0)],
        "TokenizedInput": [("original_text", 1, 2), ("input_ids", 2, 0)],
        "TopLogProbs": [("values", 1, 5), ("token_ids", 2, 0)],
    },
}


def _modules() -> dict[str, object]:
    modules = {name: getattr(smg_grpc_proto, name) for name in DEPLOYED if name != "common_pb2"}
    # `common.proto` has no public export; load it from the generated package.
    if "common_pb2" in DEPLOYED:
        from smg_grpc_proto.generated import common_pb2

        modules["common_pb2"] = common_pb2
    return modules


_MODULE_IDS = sorted(DEPLOYED)


@pytest.fixture(scope="module")
def modules() -> dict[str, object]:
    return _modules()


@pytest.mark.parametrize("module_name", _MODULE_IDS)
def test_every_deployed_message_keeps_its_name(modules: dict[str, object], module_name: str) -> None:
    descriptor = modules[module_name].DESCRIPTOR
    for message_name in DEPLOYED[module_name]:
        assert message_name in descriptor.message_types_by_name, (
            f"{module_name}: {message_name} was removed from the proto"
        )


@pytest.mark.parametrize("module_name", _MODULE_IDS)
def test_every_deployed_field_keeps_its_number_name_and_wire_type(
    modules: dict[str, object], module_name: str
) -> None:
    descriptor = modules[module_name].DESCRIPTOR
    for message_name, fields in DEPLOYED[module_name].items():
        message = descriptor.message_types_by_name[message_name]
        by_number = {field.number: field for field in message.fields}
        by_name = {field.name: field for field in message.fields}
        for name, number, wire in fields:
            assert name in by_name, f"{module_name}: {message_name}.{name} was removed"
            field = by_name[name]
            assert field.number == number, (
                f"{module_name}: {message_name}.{name} moved from field "
                f"{number} to {field.number}"
            )
            assert field.number in by_number
            assert _WIRE[field.type] == wire, (
                f"{module_name}: {message_name}.{name} moved from wire type "
                f"{wire} to {_WIRE[field.type]}"
            )


@pytest.mark.parametrize("module_name", _MODULE_IDS)
def test_no_new_field_reuses_a_deployed_field_number(
    modules: dict[str, object], module_name: str
) -> None:
    # A new field must take a free number. A reuse would decode the deployed
    # peer's bytes into the new field, which is worse than a removal.
    descriptor = modules[module_name].DESCRIPTOR
    for message_name, fields in DEPLOYED[module_name].items():
        message = descriptor.message_types_by_name[message_name]
        known = {name for name, _number, _wire in fields}
        owners = {number: name for name, number, _wire in fields}
        for field in message.fields:
            if field.name in known:
                continue
            owner = owners.get(field.number)
            assert owner is None or owner == field.name, (
                f"{module_name}: {message_name}.{field.name} reuses field "
                f"{field.number} of deployed {owner}"
            )


def test_data_parallel_rank_keeps_field_16_and_an_absent_rank_stays_absent(
    modules: dict[str, object],
) -> None:
    # Tensor-parallel serving routes by data-parallel rank. The deployed
    # servicer sent it as an implicit int32 on field 16; this tree marks it
    # optional, so an unset rank must still stay off the wire.
    sglang = modules["sglang_scheduler_pb2"]
    field = sglang.GenerateRequest.DESCRIPTOR.fields_by_name["data_parallel_rank"]
    assert field.number == 16
    assert _WIRE[field.type] == 0
    encoded = sglang.GenerateRequest().SerializeToString()
    decoded = sglang.GenerateRequest()
    decoded.ParseFromString(encoded)
    assert not decoded.HasField("data_parallel_rank")


def test_kv_cache_cleared_gains_ownership_without_moving_the_deployed_shape(
    modules: dict[str, object],
) -> None:
    # 0.4.14 sent `KvCacheCleared` with no fields. The tree adds the optional
    # `ownership` string at field 1, so an empty message from a deployed
    # servicer still decodes and a new one decodes on this tree.
    common = modules["common_pb2"]
    assert list(common.KvCacheCleared.DESCRIPTOR.fields_by_name) == ["ownership"]
    empty = common.KvCacheCleared()
    round_trip = common.KvCacheCleared()
    round_trip.ParseFromString(empty.SerializeToString())
    assert round_trip.SerializeToString() == b""
    owned = common.KvCacheCleared(ownership="residency-agent")
    decoded = common.KvCacheCleared()
    decoded.ParseFromString(owned.SerializeToString())
    assert decoded.ownership == "residency-agent"
