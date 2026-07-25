# exp1-trap-layer：陷入层原型

> 验证假设 1/5/4（议题 6/8/11）：**凭证托管+拦截能透明插入**、**max_tokens 硬顶物理截断透支**、**WAL 批量流水 5k/s 不拖垮热路径**
> 状态：✅ 通过（2026-07-25）

## 结论先行

三个假设全部实证通过：

1. **透明性**：ReAct harness 不改代码、只改 `OPENAI_BASE_URL` 即接入代理跑通完整任务（tool_calls → stop 全链路）
2. **透支物理截断**：余额不足时生成被 `finish_reason=length` 物理截断，账单 ≤ 余额——max_tokens 硬顶把透支从账本问题变为不可能事件
3. **耗尽语义**：余额 ≤20% 响应带 `X-Budget-Warning: true` 预警头；耗尽返回 429 `budget_exhausted` 挂起；`/admin/recharge` 充值后恢复
4. **流水吞吐**：批量组提交在定速 5,000 扣减/s 下跑满，p99 延迟增量仅 6.38µs（占延迟预算 3.19%，阈值 <10%）；比每笔同步落盘快 3.3×（p99）

## 架构（议题 6/8/11 定案的原型化）

```
agent（只持代理签发 key）
  ↓ POST /v1/chat/completions（OpenAI 完全兼容，流式/非流式）
trap_layer 代理
  ├─ 认证：agent key → 预算账户（config/accounts.yaml）
  ├─ max_tokens 硬顶：剩余 credits ÷ 单价 → token 上限注入请求
  ├─ 转发：provider 抽象层（真实 key 只在此处，凭证托管）
  ├─ 结算：usage × 定价表（config/pricing.yaml）扣减 credits
  ├─ 语义：≤20% 预警头 / ≤0 429 挂起 / 充值恢复
  └─ WAL：内存队列 + 批量组提交落盘（results/wal/*.jsonl）
```

## 判定标准对照

| 判定标准 | 实证 | 结论 |
|---|---|---|
| 透明性：harness 不改代码只改 base_url 跑通 | `harness.react_agent normal`：calculator 工具调用 → 最终答案 101.0，全链路通过代理 | ✅ |
| 透支物理截断：余额 100 发起预估 150 的调用 → 生成截断，账单 ≤ 余额 | `truncated` 场景：`finish_reason=length`，余额未被击穿 | ✅ |
| 耗尽语义：80% 预警、100% 挂起、充值恢复 | `exhausted` 场景：429 `budget_exhausted` → recharge 200 → `finish_reason=stop` 恢复；预警头有专项测试（test_exhaustion.py） | ✅ |
| 流水吞吐：≥5k 扣减/s 且热路径延迟增幅 <10% | 定速 5,000/s 跑满；批量 p99 增量 6.38µs（3.19% 预算）；极限吞吐 230,809 扣减/s | ✅ |

## 基准数据（results/benchmark.md）

| 变体 | p50 (µs) | p99 (µs) | 定速吞吐 | 极限吞吐 |
|---|---|---|---|---|
| baseline（无 WAL） | 1.46 | 3.33 | 5,000/s | 1,859,489/s |
| sync（每笔落盘） | 8.17 | 32.17 | 5,000/s | 226,599/s |
| **batch（批量组提交）** | **3.04** | **9.71** | **5,000/s** | **230,809/s** |

## 复现

```bash
cd experiments/exp1-trap-layer
uv sync
uv run pytest -q                              # 26 个测试
uv run python -m trap_layer.main &            # 起代理（mock provider，端口 8400）
uv run python -m harness.react_agent normal     # 场景 1：透明跑通
uv run python -m harness.react_agent truncated  # 场景 2：透支截断
uv run python -m harness.react_agent exhausted  # 场景 3：耗尽挂起+充值恢复
uv run python scripts/benchmark_wal.py          # WAL 基准
```

真实 provider：复制 `.env.example` 为 `.env`，填 `LLM_BASE_URL` / `LLM_API_KEY` / `LLM_MODEL`（OpenAI 兼容转发服务），`.env` 已 gitignore。

## 测试（26 passed）

test_format（格式兼容）/ test_budget（扣减正确性）/ test_max_tokens（硬顶注入）/ test_exhaustion（预警头+429+充值）/ test_streaming（流式计量）/ test_wal（批量落盘）/ test_admin（管理端点）

## 文件结构

```
trap_layer/
  proxy.py            # FastAPI 入口：认证/硬顶/转发/结算/语义
  budget.py           # 预算账户：扣减/预警/挂起/充值
  wal.py              # 批量组提交流水
  config.py           # 配置加载（accounts/pricing/.env）
  providers/
    base.py           # provider 抽象
    mock.py           # 确定性假 LLM（离线测试基准）
    openai_compat.py  # 真实 OpenAI 兼容 provider
harness/react_agent.py  # ReAct demo（透明性证据）
scripts/benchmark_wal.py
config/{accounts,pricing}.yaml
results/{benchmark.md, wal/}
```

## 遗留与下一步

- 凭证托管的"不可绕过"在本原型中由"agent 只持代理 key"演示；网络隔离（工具只能经内核网关到达）属部署层，未在原型范围
- exp2（看门狗）复用本 harness；exp3（KV 计费）复用本代理
- 工程化方向：异步并发压力下的账户锁竞争、多 agent key 规模化的账户存储
