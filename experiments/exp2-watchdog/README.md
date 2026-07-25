# exp2-watchdog：看门狗（无进展检测）

> 验证假设 2（议题 9）：**进展信号 + 相似度阈值能以可接受误报率检测 agent 空转（livelock）**
> 状态：✅ 通过（2026-07-25）

## 结论先行

两级制第一级（内核看门狗）可用性实证成立：

- **误报率 0.0%**（10 个正常任务，判定线 <10%）
- **检出率 100%**（5 个空转任务，检出步数 [4, 4, 4, 6, 7]，均在阈值内）
- **边界任务零误杀**（3 个"慢但有进展"任务全部正常完成）

阈值敏感性扫描（4×4 全格子）显示默认阈值 `(maxStepsWithoutProgress=4, maxRepeatSimilarity=0.85)` 位于安全区，并给出了失效边界（见下）——这组数据直接支撑"阈值声明进 ELF"的设计。

## 设计（议题 9 定案的原型化）

| 议题 9 设计 | 本实验实现 |
|---|---|
| 进展信号（"喂狗"） | 新工具调用 / 新产物（工具返回新信息）/ harness 心跳——全是结构信号非语义判断（`watchdog/signals.py`） |
| 相似度检测 | 连续步内容 n-gram Jaccard 相似度（`watchdog/similarity.py`，离线可算，无外部依赖） |
| 阈值（ELF 声明） | `config/watchdog.yaml`：maxStepsWithoutProgress=4、maxRepeatSimilarity=0.85（连续 3 步超限触发） |
| 触发动作 | 挂起 + 标记（监督事件 JSON，模拟"转第二级语义监督"；本实验只到标记） |
| 挂接方式 | 陷阱侧中间件（与内核边界一致：机制在陷阱侧，策略配置化） |
| mock 模式 | `providers/scripted.py`：diverse（正常推进）/ stuck（空转复读）/ mixed（中途卡死）脚本化 provider |

## 判定标准对照

| 判定标准 | 实证 | 结论 |
|---|---|---|
| 正常任务集（≥10）误报率 < 10% | 10 任务，误报率 0.0% | ✅ |
| 空转任务检出率 100%（阈值步数内） | 5/5 检出，步数 [4,4,4,6,7]（阈值 maxSteps=4 触发 + 确认窗） | ✅ |
| 边界任务（慢但有进展）不误杀 | 3 任务全部 completed=true | ✅ |

## 阈值敏感性扫描（results/threshold_scan.md）

maxStepsWithoutProgress ∈ {3,4,5,6} × maxRepeatSimilarity ∈ {0.80,0.85,0.90,0.95}，本任务集上 16 格全部 0% 误报、100% 检出。但判读给出了真实的失效边界：

- **maxSim 过低（0.80）开始误杀慢任务**——相似度阈值越松，把"同领域正常推进"误判为复读的风险越大
- **maxSteps 越小对慢任务越苛刻**——progress_interval=3 的边界任务在 maxSteps=3 时触线
- 默认 (4, 0.85) 位于安全区中间；**ELF 声明阈值时应按任务类型从安全区取值，而非抄默认值**

## 复现

```bash
cd experiments/exp2-watchdog
uv sync
uv run pytest -q                        # 14 个测试（信号/相似度/阈值/触发）
uv run python -m scripts.run_experiment # 判定实验（正常 10 + 空转 5 + 边界 3）
uv run python -m scripts.threshold_scan # 阈值敏感性扫描
```

真实 LLM API 下的误报率验证：待 `.env`（本实验全部 mock 可跑；mock 的 stuck 模式是理想化复读，真实空转形态更隐蔽，真实误报率可能高于 0%——这是本实验的已知边界）。

## 文件结构

```
watchdog/{core,signals,similarity,config}.py  # 看门狗内核机制
harness/react_loop.py       # 复用 exp1 harness 循环（只读引用）
providers/scripted.py       # 脚本化 mock（diverse/stuck/mixed）
tasks.py                    # 任务集定义（正常/空转/边界）
runner.py, report.py        # 实验运行与报告生成
scripts/{run_experiment,threshold_scan}.py
tests/                      # 14 个测试
config/watchdog.yaml        # 阈值（ELF 声明原型）
results/{report,threshold_scan}.{json,md}
```

## 遗留与边界

- mock 空转是理想化形态（高相似复读）；真实空转包括"话题漂移但无进展"等语义形态——那正是两级制第二级（用户态语义监督 agent）存在的理由，本实验验证了第一级"便宜过滤器"不误伤正常流量
- 阈值与任务类型强相关 → 佐证议题 9 决策"阈值声明进 ELF"而非内核硬编码
- 子任务中途超时停滞，实验运行与 README 由 Sisyphus 接管完成
