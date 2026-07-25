"""进程内 ReAct 步进循环：改编自 exp1-trap-layer/harness/react_agent.py（注明出处）。

与 exp1 harness 的差异：exp1 走 OpenAI client → HTTP 代理；本实验用进程内
ScriptedProvider（离线、可种子复现），看门狗作为独立包装层挂在步进循环上，
在每步请求/响应边界提取 StepObservation —— 与内核边界一致：机制在陷阱侧，
agent 循环对看门狗零感知（循环体不判断任何阈值，只查询挂起标记）。

工具：calculator / get_time 复制自 exp1（注明出处），lookup_notes 为本实验新增的
确定性假检索工具（不需要真实知识库）。
"""

from __future__ import annotations

import ast
import hashlib
import json
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any

from providers.scripted import ScriptedProvider
from watchdog import StepObservation, SuspensionEvent, ToolCallView, Watchdog, WatchdogConfig

#: 跑满上限必须大于 mixed 模式的检出步数，又不至于让 stuck 跑太久
MAX_STEPS = 12


def _safe_eval(expression: str) -> float:
    """只允许算术 AST 节点的计算器（复制自 exp1 harness，注明出处）。"""
    allowed = (
        ast.Expression, ast.BinOp, ast.UnaryOp, ast.Constant,
        ast.Add, ast.Sub, ast.Mult, ast.Div, ast.USub, ast.UAdd, ast.Mod, ast.Pow,
        ast.Load, ast.Call, ast.Name,
    )
    tree = ast.parse(expression, mode="eval")
    for node in ast.walk(tree):
        if not isinstance(node, allowed):
            raise ValueError(f"非法表达式节点: {type(node).__name__}")
        if isinstance(node, ast.Call) and not (isinstance(node.func, ast.Name) and node.func.id == "round"):
            raise ValueError("仅允许 round() 调用")
    return float(eval(compile(tree, "<calc>", "eval"), {"__builtins__": {}, "round": round}))


def _lookup_notes(query: str) -> str:
    """确定性假检索：同一 query 恒得同一笔记，不同 query 几乎必得不同笔记。"""
    digest = hashlib.blake2b(query.encode("utf-8"), digest_size=6).hexdigest()
    return f"note[{digest}]: facts about {query} retrieved from archive segment {digest[:4]}"


def run_tool(name: str, arguments: str) -> str:
    if name == "calculator":
        expr = json.loads(arguments).get("expression", "")
        try:
            return str(_safe_eval(expr))
        except (ValueError, ZeroDivisionError, SyntaxError) as exc:
            return f"计算错误: {exc}"
    if name == "get_time":
        return datetime.now(UTC).isoformat(timespec="seconds")
    if name == "lookup_notes":
        return _lookup_notes(json.loads(arguments).get("query", ""))
    return f"未知工具: {name}"


@dataclass(frozen=True, slots=True)
class StepTrace:
    """每步的可观测记录（供报告与测试断言）。"""

    step_index: int
    finish_reason: str
    progress: bool
    similarity: float


@dataclass(slots=True)
class RunResult:
    task_id: str
    outcome: str  # "completed" | "suspended" | "max_steps"
    steps: int
    final_answer: str
    event: SuspensionEvent | None
    traces: list[StepTrace] = field(default_factory=list)


def run_episode(
    task_id: str,
    prompt: str,
    provider: ScriptedProvider,
    config: WatchdogConfig,
    max_steps: int = MAX_STEPS,
) -> RunResult:
    """跑一个任务 episode；看门狗在每步边界观察，触发即挂起并中断循环。"""
    watchdog = Watchdog(config, task_id)
    messages: list[dict[str, Any]] = [{"role": "user", "content": prompt}]
    traces: list[StepTrace] = []

    for step in range(max_steps):
        response = provider.chat(messages)
        choice = response["choices"][0]
        msg = choice["message"]
        finish = choice["finish_reason"]
        messages.append(msg)

        tool_results: list[str] = []
        for call in msg.get("tool_calls") or []:
            result = run_tool(call["function"]["name"], call["function"]["arguments"])
            tool_results.append(result)
            messages.append(
                {"role": "tool", "tool_call_id": call["id"], "content": result}
            )

        obs = StepObservation(
            step_index=step,
            finish_reason=finish,
            content=msg.get("content") or "",
            tool_calls=tuple(
                ToolCallView.from_raw(c["function"]["name"], c["function"]["arguments"])
                for c in msg.get("tool_calls") or []
            ),
            tool_results=tuple(tool_results),
        )
        event = watchdog.observe(obs)
        traces.append(
            StepTrace(
                step_index=step,
                finish_reason=finish,
                progress=watchdog.last_signals.any_progress,
                similarity=watchdog.last_similarity,
            )
        )
        if event is not None:
            return RunResult(task_id, "suspended", step + 1, msg.get("content") or "", event, traces)
        if finish == "stop":
            return RunResult(task_id, "completed", step + 1, msg.get("content") or "", None, traces)

    return RunResult(task_id, "max_steps", max_steps, messages[-1].get("content") or "", None, traces)
