# exp2 看门狗实验报告（mock 全量）

## 配置（模拟 ELF 阈值声明）

- maxStepsWithoutProgress = 4
- maxRepeatSimilarity = 0.85
- repeatWindow = 3
- similarityBackend = ngram

## 逐任务结果

| task | kind | outcome | steps | 触发 |
|---|---|---|---|---|
| n-calc-01 | normal | completed | 3 | — |
| n-calc-02 | normal | completed | 4 | — |
| n-calc-03 | normal | completed | 5 | — |
| n-query-01 | normal | completed | 3 | — |
| n-query-02 | normal | completed | 4 | — |
| n-query-03 | normal | completed | 5 | — |
| n-reason-01 | normal | completed | 4 | — |
| n-reason-02 | normal | completed | 5 | — |
| n-reason-03 | normal | completed | 6 | — |
| n-calc-04 | normal | completed | 6 | — |
| l-stuck-01 | livelock | suspended | 4 | step 4 (repeat_similarity) |
| l-stuck-02 | livelock | suspended | 4 | step 4 (repeat_similarity) |
| l-stuck-03 | livelock | suspended | 4 | step 4 (repeat_similarity) |
| l-mixed-01 | livelock | suspended | 6 | step 6 (no_progress) |
| l-mixed-02 | livelock | suspended | 7 | step 7 (no_progress) |
| b-slow-01 | boundary | completed | 8 | — |
| b-slow-02 | boundary | completed | 10 | — |
| b-slow-03 | boundary | completed | 9 | — |

## 度量汇总

- 正常任务：10 个，误报 0 个，误报率 **0.0%**（判定线 < 10%）
- 空转任务：5 个，检出 5 个，检出率 **100.0%**，检出步数 [4, 4, 4, 6, 7]
- 边界任务：3 个，误杀/未收敛 0 个 

## 判定标准逐条结论

| 判定标准 | 实证 | 结论 |
|---|---|---|
| 正常任务集（≥10）误报率 < 10% | 10 任务，误报率 0.0% | ✅ |
| 空转任务检出率 100%（阈值步数内） | 检出率 100.0%，检出步数 [4, 4, 4, 6, 7] | ✅ |
| 边界任务（慢但有进展）不误杀 | 3 任务全部 completed：True | ✅ |

> 真实 LLM API 下的误报率验证：待 .env（本实验全部 mock 可跑）。
