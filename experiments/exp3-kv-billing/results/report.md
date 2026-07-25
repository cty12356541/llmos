# exp3-kv-billing 实验报告

日期：2026-07-25　状态：**mock 部分实证通过；真实 API 部分待验（缺转发服务凭证）**

## 1. 测量方法

构造 10,000 字符（≈2,500 token，按 4 字符/token 粗估）的确定性共享系统前缀，
对同一前缀连续发起 3 次调用（同前缀、不同问题、max_tokens=32），
逐次记录完整 usage JSON，按定价表折算 credits 并对比全价基线。

mock 实测输出（`uv run python scripts/measure_mock.py`，产物 `results/measurement_mock.json`）：

```
=== cache_style=deepseek ===
 # | field_kind |  prompt |  cached |  compl |    credits |     全价基线 |     节省
 1 |  deepseek |    2505 |       0 |     32 |   1284.500 |   1284.500 |    0.000
 2 |  deepseek |    2505 |    2500 |     32 |    159.500 |   1284.500 | 1125.000
 3 |  deepseek |    2505 |    2500 |     32 |    159.500 |   1284.500 | 1125.000

=== cache_style=openai ===（同上：第二次起 cached=2500，成本 1284.5 → 159.5）
=== cache_style=none ===（三次均 1284.5，无缓存字段，全价）
```

物理事实：**第二次起同前缀命中缓存（cached=2500 = 前缀估算 token 数），
当次调用成本从 1284.5 降至 159.5 credits（-87.6%）**——议题 11"按驱动回报实际成本记账"
在计费链路上的量级意义即在此：若按 exp1 旧口径（不感知缓存）记账，命中调用被多收 8 倍。

## 2. 判定标准逐条结论

| # | 判定标准 | 结论 | 证据 |
|---|---|---|---|
| 1 | 测量工具能逐次采集完整 usage（含嵌套 details 字段）并输出对比表 | ✅ 实证通过（mock）/ ⏳ 真实 API 待验 | `kv_billing/measure.py` + `scripts/measure_mock.py`；对比表见上 |
| 2 | mock 模拟"第二次起同前缀命中缓存"，三种字段风格可配 | ✅ 实证通过 | `tests/test_mock_cache.py`：deepseek 风格 hit+miss=prompt_tokens 不变式、openai 风格子集语义、none 风格无字段，均有测试锁定 |
| 3 | 计费映射支持折扣价、无字段全价降级、未配折扣价全价降级 | ✅ 实证通过 | `tests/test_billing.py`：`test_no_cache_fields_charges_full_price`、`test_hit_with_unconfigured_discount_falls_back_to_full_price` |
| 4 | 三种字段情形的折算正确性 + 与定价表组合 | ✅ 实证通过 | 26 个 pytest 全绿（0.24s） |
| 5 | 真实 provider 回报含缓存命中的 usage 字段 | ⏳ **待验** | `scripts/measure_real.py` 就绪；当前输出"缺少凭证"并以退出码 2 终止。待用户提供转发服务凭证后运行，原始 usage 将落盘 `results/real/` |

## 3. 字段探测结论（基于公开文档形态 + mock 复现；真实回报待验）

| provider 风格 | 字段 | 语义 | 计费映射处理 |
|---|---|---|---|
| DeepSeek | `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` | hit + miss = prompt_tokens | cached = hit |
| OpenAI | `prompt_tokens_details.cached_tokens` | cached 是 prompt_tokens 子集 | cached = cached_tokens |
| 无缓存感知 | 仅三基础字段 | — | cached = 0，全价（与 exp1 口径一致） |

统一语义 cached ⊆ prompt_tokens 使两种风格共用一条折算路径；
探测层做了三处防御：命中量钳制到 [0, prompt_tokens]、非法字段值降级 NONE、
基础字段缺失报错。以上防御均有测试锁定（`tests/test_usage_probe.py`）。

## 4. 计费映射规则（usage → credits，建议采纳）

```
uncached_prompt_cost = (prompt_tokens − cached_tokens) × prompt_per_1k / 1000
cached_prompt_cost   = cached_tokens × cached_prompt_per_1k / 1000   # 缺省=全价
completion_cost      = completion_tokens × completion_per_1k / 1000
```

- 命中折扣量级参考：DeepSeek ≈ 1/10，OpenAI ≈ 1/2（pricing.yaml 已按此配置示例）。
- 两条降级规则方向相反且独立：provider 不报 → 全价；报了但定价表没配 → 也全价。
- exp1 `settle()` 是本折算器 `cached_tokens=0` 的特例，集成后无缓存 provider 的
  账单逐 credit 不变（回归安全）。集成方案详见 README「与 exp1 代理的集成建议」节。

## 5. 对危险假设 3 的判定

**机制层面成立（mock 实证）**：缓存感知的 usage 回报 → 类型化探测 → 含折扣的折算，
整条链路离线可测、可复现，折算规则在三种字段情形下均正确且降级安全。

**前提层面待验（真实 API）**：假设 3 的剩余风险是真实 provider 的回报形态
（字段名、语义、命中粒度、是否与文档一致、流式 usage chunk 是否同样带 details）。
`scripts/measure_real.py` 已就绪，凭证到位后一键验证，预期产出：
- `results/real/usage_call_*.json`：各次调用原始 usage（字段形态的直接证据）
- 对比表：第二次起 cached 是否 >0、成本差异是否达到定价表宣称的量级

## 6. 已知边界

- mock 的前缀识别按"除最后一条消息外的全部消息"哈希，命中粒度是整个前缀；
  真实 provider 的命中粒度可能更细（前缀的任意前缀）——折算器对此免疫
  （只消费 cached_tokens 数值），但测量解读时需注意。
- 流式响应的 usage chunk 字段形态未在真实 provider 上验证（mock 已支持）；
  exp1 代理默认流式透传，集成时需确认流式 usage 同样携带缓存明细。
