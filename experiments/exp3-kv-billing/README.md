# exp3-kv-billing：KV 计费驱动对接

验证危险假设 3：**provider 能回报含 prefix cache 命中的实际成本**——议题 11 拍板
"KV 共享页按驱动回报实际成本记账，不做二次分摊"的落地可行性实验。

## 实验问题

1. 真实 provider 的 usage 里缓存命中字段长什么样？（字段探测）
2. usage 各字段 → credits 的折算规则怎么写？（计费映射）
3. 若字段存在，exp1 代理的预算扣减应如何改为按"实际成本"？（集成方案，文档级）

## 目录结构

```
kv_billing/
  usage_probe.py          # usage 字段探测：原始 JSON → 类型化 UsageProbe（三情形降级）
  pricing.py              # 定价表：ModelPrice 扩展 cached_prompt_per_1k（可缺省）
  billing.py              # 计费映射：usage → credits 折算器
  measure.py              # 测量流程：同前缀连续调用 + 对比表（脚本与测试共用）
  providers/
    base.py               # 复制自 exp1（注明出处）
    openai_compat.py      # 复制自 exp1（注明出处）
    mock.py               # 复制并扩展自 exp1 mock：缓存命中感知，三种回报风格可配
config/pricing.yaml       # 定价表（含 cached_prompt_per_1k 扩展示例）
scripts/measure_mock.py   # 离线测量：三种字段风格全跑，产物 results/measurement_mock.json
scripts/measure_real.py   # 真实 API 实测（待用户提供凭证后运行）
tests/                    # 26 个 pytest：探测/折算/降级/折扣/mock 命中模拟
results/report.md         # 实验报告（判定标准逐条结论）
```

## 快速开始

```bash
uv sync
uv run pytest -q                          # 全部离线可跑
uv run python scripts/measure_mock.py     # 离线测量 + 对比表

# 真实 API 实测（待用户提供转发服务凭证后运行）：
cp .env.example .env   # 填 LLM_BASE_URL / LLM_API_KEY / LLM_MODEL
uv run python scripts/measure_real.py    # 原始 usage 落盘 results/real/（已 gitignore）
```

## 字段探测结论（usage 兼容性清单）

| 情形 | 字段 | 语义 | 探测结果 |
|---|---|---|---|
| DeepSeek 风格 | `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` | hit + miss = prompt_tokens | `CacheFieldKind.DEEPSEEK`，cached = hit |
| OpenAI 风格 | `prompt_tokens_details.cached_tokens` | cached 是 prompt_tokens 的子集 | `CacheFieldKind.OPENAI`，cached = cached_tokens |
| 无缓存字段 | 仅 prompt/completion/total | provider 无缓存感知 | `CacheFieldKind.NONE`，cached = 0（优雅降级，不报错） |

统一语义：**cached_tokens 是 prompt_tokens 中被命中的子集，uncached = prompt_tokens - cached**。
两种风格都归入此语义；命中量被钳制在 `[0, prompt_tokens]` 防御矛盾回报；
非法缓存字段值被忽略并降级为 NONE；但 prompt_tokens/completion_tokens 缺失即报错
（基础字段是协议约定的事实来源，不可降级）。

## 计费映射规则（usage → credits）

```
uncached_prompt_cost = (prompt_tokens - cached_tokens) × prompt_per_1k / 1000   # 全价
cached_prompt_cost   = cached_tokens × cached_prompt_per_1k / 1000              # 折扣价
completion_cost      = completion_tokens × completion_per_1k / 1000             # 全价
total_cost           = 三者之和
```

降级规则（两条，方向相反、各自独立）：

1. **provider 无缓存字段**（NONE）→ cached_tokens = 0，全部按全价——与 exp1 `settle()` 口径完全一致；
2. **provider 报了命中但定价表未配折扣价**（`cached_prompt_per_1k` 缺省）→ 命中部分按全价。
   折扣必须是显式配置的商业决定，折算器绝不"擅自打折"。

定价参考量级：DeepSeek 命中价 ≈ 原价 1/10；OpenAI cached_tokens ≈ 原价 1/2
（`config/pricing.yaml` 中 mock-model / openai-style-model 条目即按此配置）。

## 与 exp1 代理的集成建议（文档级，不改 exp1 代码）

exp1 `BudgetManager.settle()` 当前的折算：

```python
cost = prompt_tokens * prompt_per_1k/1000 + completion_tokens * completion_per_1k/1000
```

它等价于本实验折算器 `cached_tokens = 0` 的特例。集成路径：

1. **`ModelPrice` 加字段**：`cached_prompt_per_1k: float | None = None`（exp1 `config.py`
   的 `load_pricing` 读取同名 YAML 键，缺省 None）。定价表 YAML 为支持缓存的模型补该键。
2. **settle 前加探测**：`proxy.py` 结算点拿到的本就是完整 usage dict；调用
   `probe_usage(usage)` 得 `cached_tokens`，传入 `settle()`（签名加一个默认 0 的参数，
   向后兼容）。
3. **settle 内改一行折算**：prompt 部分拆为
   `(prompt_tokens - cached) × 全价 + cached × 折扣价(缺省=全价)`。
   无缓存字段的 provider 行为逐 credit 不变（回归安全）。
4. **WAL/响应头**：`Settlement` 可加 `cached_tokens` 字段便于对账——缓存命中带来的
   成本下降应在管理面可观测，否则"账突然变便宜"会被误认为计费 bug。
5. **硬顶折算（max_completion_tokens_affordable）不改**：预留按全价估 prompt 成本即可——
   缓存命中只会让实际账单 ≤ 预留，硬顶方向保守，无需感知缓存。
6. **不做二次分摊**：命中折扣全额归当次调用的 agent（议题 11 拍板）。共享前缀是
   平台行为，谁触发填充不向他人收费，谁命中谁享受折扣。

## 判定标准

见 `results/report.md` 逐条结论。mock 部分全部实证通过；真实 API 字段形态
（DeepSeek/OpenAI 各自的确切 JSON）**待用户提供转发服务凭证后运行
`scripts/measure_real.py` 验证**。
