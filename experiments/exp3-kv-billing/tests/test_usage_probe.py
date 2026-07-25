"""usage 字段探测测试：三种字段情形 + 降级 + 防御钳制。"""

from __future__ import annotations

import pytest

from kv_billing.usage_probe import CacheFieldKind, probe_usage


class TestProbeDeepseekStyle:
    def test_extracts_hit_tokens_when_deepseek_fields_present(self) -> None:
        # Given: DeepSeek 风格 usage（hit + miss = prompt_tokens）
        usage = {
            "prompt_tokens": 3000,
            "completion_tokens": 32,
            "total_tokens": 3032,
            "prompt_cache_hit_tokens": 2500,
            "prompt_cache_miss_tokens": 500,
        }
        # When: 探测
        probe = probe_usage(usage)
        # Then: 命中量与风格被正确识别
        assert probe.field_kind is CacheFieldKind.DEEPSEEK
        assert probe.cached_tokens == 2500
        assert probe.uncached_prompt_tokens == 500

    def test_reports_zero_hit_when_first_call_all_miss(self) -> None:
        # Given: DeepSeek 首次调用（hit=0，字段仍存在）
        usage = {
            "prompt_tokens": 3000,
            "completion_tokens": 32,
            "prompt_cache_hit_tokens": 0,
            "prompt_cache_miss_tokens": 3000,
        }
        # When: 探测
        probe = probe_usage(usage)
        # Then: 风格识别为 deepseek，命中量为 0（而非降级为 NONE）
        assert probe.field_kind is CacheFieldKind.DEEPSEEK
        assert probe.cached_tokens == 0


class TestProbeOpenaiStyle:
    def test_extracts_cached_tokens_when_details_present(self) -> None:
        # Given: OpenAI 风格 usage（cached 是 prompt_tokens 子集）
        usage = {
            "prompt_tokens": 3000,
            "completion_tokens": 32,
            "total_tokens": 3032,
            "prompt_tokens_details": {"cached_tokens": 2560},
        }
        # When: 探测
        probe = probe_usage(usage)
        # Then: 命中量与风格被正确识别
        assert probe.field_kind is CacheFieldKind.OPENAI
        assert probe.cached_tokens == 2560
        assert probe.uncached_prompt_tokens == 440

    def test_degrades_to_none_when_cached_tokens_zero(self) -> None:
        # Given: OpenAI 风格但 cached_tokens=0（首次调用）
        usage = {
            "prompt_tokens": 3000,
            "completion_tokens": 32,
            "prompt_tokens_details": {"cached_tokens": 0},
        }
        # When: 探测
        probe = probe_usage(usage)
        # Then: 字段存在即识别为 openai 风格，命中量为 0
        assert probe.field_kind is CacheFieldKind.OPENAI
        assert probe.cached_tokens == 0


class TestProbeNoCacheFields:
    def test_degrades_to_zero_cached_when_no_cache_fields(self) -> None:
        # Given: 无任何缓存字段的 usage（无缓存感知 provider）
        usage = {"prompt_tokens": 3000, "completion_tokens": 32, "total_tokens": 3032}
        # When: 探测
        probe = probe_usage(usage)
        # Then: 优雅降级为 NONE + cached=0，不报错
        assert probe.field_kind is CacheFieldKind.NONE
        assert probe.cached_tokens == 0
        assert probe.uncached_prompt_tokens == 3000


class TestProbeDefensive:
    def test_clamps_cached_to_prompt_tokens_when_provider_contradicts(self) -> None:
        # Given: 自相矛盾的回报（cached > prompt_tokens）
        usage = {
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 9999},
        }
        # When: 探测
        probe = probe_usage(usage)
        # Then: 命中量被钳制到 prompt_tokens
        assert probe.cached_tokens == 100
        assert probe.uncached_prompt_tokens == 0

    def test_raises_when_prompt_tokens_missing(self) -> None:
        # Given: 缺少 prompt_tokens 的 usage（违反协议的事实来源）
        usage = {"completion_tokens": 32}
        # When/Then: 探测报错（缓存字段可缺，基础字段不可缺）
        with pytest.raises(ValueError, match="prompt_tokens"):
            probe_usage(usage)

    def test_ignores_garbage_cache_fields_and_degrades(self) -> None:
        # Given: 缓存字段是非法值（字符串/负数）
        usage = {
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "prompt_cache_hit_tokens": "many",
            "prompt_tokens_details": {"cached_tokens": -5},
        }
        # When: 探测
        probe = probe_usage(usage)
        # Then: 非法缓存字段被忽略，降级为 NONE
        assert probe.field_kind is CacheFieldKind.NONE
        assert probe.cached_tokens == 0
