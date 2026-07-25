"""脚本化 mock provider：扩展自 exp1-trap-layer/trap_layer/providers/mock.py（注明出处）。

与 exp1 mock 的差异：exp1 mock 只有单一固定行为（有工具就调 calculator）；
本 provider 面向看门狗实验扩展为四种空转/推进模式，全部确定性、可种子复现：

- diverse：每步内容有真实变化（新词、新工具参数），模拟正常推进
- stuck：输出重复/同义反复（同一工具同一参数、内容近相同），模拟空转
- mixed：前 stuck_after 步正常，之后空转，模拟中途卡死
- slow：慢但有进展（每 progress_interval 步才有一次新工具调用，内容中度重叠），
  用于边界任务，验证看门狗不误杀

接口为进程内同步调用（实验不需要 HTTP；挂接形态是独立包装层拦截每步请求/响应）。
"""

from __future__ import annotations

import json
import random
from dataclasses import dataclass
from typing import Any, Literal

ProviderMode = Literal["diverse", "stuck", "mixed", "slow"]
Flavor = Literal["calc", "query", "reason"]

#: 每个 flavor 的词汇池：diverse 模式每步从中采不重叠样本，保证步间低相似度
_VOCAB: dict[str, tuple[str, ...]] = {
    "calc": (
        "compute sum product ratio percent growth rate total average median ",
        "divide multiply subtract add square root factor prime digit value ",
        "ledger invoice tax discount interest principal balance debit credit ",
    ),
    "query": (
        "lookup record entry catalog index archive registry dataset schema row ",
        "column field key value store cache shard replica query filter sort ",
        "atlas beacon cipher delta ember fjord garnet harbor ionic jasper kelp ",
    ),
    "reason": (
        "hypothesis premise inference deduction induction analogy constraint ",
        "evidence counterexample lemma corollary theorem axiom proof refute ",
        "context motive cause effect sequence pattern anomaly outlier trend ",
    ),
}

_STUCK_CONTENT = (
    "reconsidering the same approach again carefully reviewing the identical "
    "previous step once more without change in strategy or direction"
)

_SLOW_SCAFFOLD = (
    "continuing the systematic investigation of the problem with the established "
    "method and keeping the current direction"
)


def _pool(flavor: Flavor) -> list[str]:
    return " ".join(_VOCAB[flavor]).split()


def _rng(seed: int, step: int) -> random.Random:
    return random.Random((seed << 16) ^ (step * 2654435761))


@dataclass(frozen=True, slots=True)
class ScriptedProvider:
    """确定性脚本 provider。步号从消息历史推导（无内部状态，可重放）。"""

    mode: ProviderMode
    seed: int
    flavor: Flavor = "calc"
    planned_steps: int = 4  # diverse/slow：第 planned_steps-1 步产出最终答案
    stuck_after: int = 2  # mixed：从该步开始空转
    progress_interval: int = 3  # slow：每隔几步喂一次狗（新工具调用）

    def chat(self, messages: list[dict[str, Any]]) -> dict[str, Any]:
        step = sum(1 for m in messages if m.get("role") == "assistant")
        content, tool_call, finish = self._plan(step)
        message: dict[str, Any] = {"role": "assistant", "content": content}
        if tool_call is not None:
            message["tool_calls"] = [tool_call]
        return {
            "choices": [
                {
                    "message": message,
                    "finish_reason": finish,
                }
            ]
        }

    # ---- 内部：各模式的步计划 ----

    def _plan(self, step: int) -> tuple[str, dict[str, Any] | None, str]:
        match self.mode:
            case "diverse":
                return self._plan_diverse(step)
            case "stuck":
                return self._plan_stuck(step)
            case "mixed":
                if step < self.stuck_after:
                    return self._plan_diverse(step)
                return self._plan_stuck(step)
            case "slow":
                return self._plan_slow(step)
            case _:
                raise ValueError(f"未知模式: {self.mode}")

    def _plan_diverse(self, step: int) -> tuple[str, dict[str, Any] | None, str]:
        if step >= self.planned_steps - 1:
            return self._final_answer(step), None, "stop"
        words = _rng(self.seed, step).sample(_pool(self.flavor), k=10)
        content = f"step {step} analysis: " + " ".join(words)
        return content, self._tool_call(step, novel=True), "tool_calls"

    def _plan_stuck(self, step: int) -> tuple[str, dict[str, Any] | None, str]:
        return _STUCK_CONTENT, self._tool_call(0, novel=False), "tool_calls"

    def _plan_slow(self, step: int) -> tuple[str, dict[str, Any] | None, str]:
        if step >= self.planned_steps - 1:
            return self._final_answer(step), None, "stop"
        tail = _rng(self.seed, step).sample(_pool(self.flavor), k=6)
        content = f"{_SLOW_SCAFFOLD} " + " ".join(tail)
        # 只在间隔步换参数（新工具调用=喂狗）；其余步重复上一步的参数
        novel = step % self.progress_interval == 0
        arg_step = step if novel else step - (step % self.progress_interval)
        return content, self._tool_call(arg_step, novel=novel), "tool_calls"

    def _final_answer(self, step: int) -> str:
        words = _rng(self.seed, step).sample(_pool(self.flavor), k=8)
        return f"FINAL answer after {step + 1} steps: " + " ".join(words)

    def _tool_call(self, variant: int, novel: bool) -> dict[str, Any]:
        """工具调用按 flavor 生成；novel=False 时固定 variant=0（参数恒定）。"""
        rng = _rng(self.seed, variant)
        match self.flavor:
            case "calc":
                name = "calculator"
                a, b = rng.randint(2, 97), rng.randint(2, 97)
                args = {"expression": f"({a}*{variant + 2})+{b}"}
            case "query":
                name = "lookup_notes"
                args = {"query": f"{self.flavor}-topic-{variant}-{rng.randint(100, 999)}"}
            case "reason":
                name = "calculator" if variant % 2 == 0 else "lookup_notes"
                args = (
                    {"expression": f"{rng.randint(2, 50)}+{variant}"}
                    if name == "calculator"
                    else {"query": f"reason-{variant}-{rng.randint(100, 999)}"}
                )
            case _:
                raise ValueError(f"未知 flavor: {self.flavor}")
        return {
            "id": f"call_scripted_{self.seed}_{variant}_{'n' if novel else 'r'}",
            "type": "function",
            "function": {"name": name, "arguments": json.dumps(args)},
        }
