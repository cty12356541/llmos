"""实验报告渲染：误报/检出数据 + 判定标准逐条结论。"""

from __future__ import annotations

from runner import ExperimentResult, TaskOutcome

FP_RATE_LIMIT = 0.10


def _row(o: TaskOutcome) -> str:
    trigger = f"step {o.trigger_step} ({o.reason})" if o.trigger_step else "—"
    return f"| {o.task_id} | {o.kind} | {o.outcome} | {o.steps} | {trigger} |"


def render_markdown(result: ExperimentResult) -> str:
    m = result.to_dict()["metrics"]
    fp_ok = result.fp_rate < FP_RATE_LIMIT
    det_ok = result.detection_rate == 1.0
    boundary_ok = not result.boundary_failures
    lines = [
        "# exp2 看门狗实验报告（mock 全量）",
        "",
        "## 配置（模拟 ELF 阈值声明）",
        "",
        f"- maxStepsWithoutProgress = {result.config.max_steps_without_progress}",
        f"- maxRepeatSimilarity = {result.config.max_repeat_similarity}",
        f"- repeatWindow = {result.config.repeat_window}",
        f"- similarityBackend = {result.config.similarity_backend}",
        "",
        "## 逐任务结果",
        "",
        "| task | kind | outcome | steps | 触发 |",
        "|---|---|---|---|---|",
        *(_row(o) for o in result.outcomes),
        "",
        "## 度量汇总",
        "",
        f"- 正常任务：{m['normal_tasks']} 个，误报 {m['false_positives']} 个，"
        f"误报率 **{m['fp_rate']:.1%}**（判定线 < {FP_RATE_LIMIT:.0%}）",
        f"- 空转任务：{m['livelock_tasks']} 个，检出 {m['detections']} 个，"
        f"检出率 **{m['detection_rate']:.1%}**，检出步数 {m['detection_steps']}",
        f"- 边界任务：{m['boundary_tasks']} 个，误杀/未收敛 "
        f"{len(result.boundary_failures)} 个 {m['boundary_failures'] or ''}",
        "",
        "## 判定标准逐条结论",
        "",
        "| 判定标准 | 实证 | 结论 |",
        "|---|---|---|",
        f"| 正常任务集（≥10）误报率 < 10% | {m['normal_tasks']} 任务，误报率 {m['fp_rate']:.1%} | {'✅' if fp_ok else '❌'} |",
        f"| 空转任务检出率 100%（阈值步数内） | 检出率 {m['detection_rate']:.1%}，检出步数 {m['detection_steps']} | {'✅' if det_ok else '❌'} |",
        f"| 边界任务（慢但有进展）不误杀 | {m['boundary_tasks']} 任务全部 completed：{boundary_ok} | {'✅' if boundary_ok else '❌'} |",
        "",
        "> 真实 LLM API 下的误报率验证：待 .env（本实验全部 mock 可跑）。",
        "",
    ]
    return "\n".join(lines)
