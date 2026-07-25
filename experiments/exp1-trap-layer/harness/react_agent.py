"""ReAct demo harness：只改 base_url 即接入陷入层代理（透明性判定的证据）。

agent 侧只持有代理签发的 key，通过 OPENAI_BASE_URL 指向代理；
不感知预算、不感知凭证托管 —— 所有控制都在代理侧物理发生。

用法（先启动代理 uv run python -m trap_layer.main）：
    uv run python -m harness.react_agent normal      # 正常跑完任务
    uv run python -m harness.react_agent truncated   # 余额不足被 max_tokens 截断
    uv run python -m harness.react_agent exhausted   # 耗尽挂起 → 充值恢复
"""

from __future__ import annotations

import ast
import json
import os
import sys
import urllib.request
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any

from openai import APIError, OpenAI

PROXY_BASE_URL = os.environ.get("OPENAI_BASE_URL", "http://127.0.0.1:8400/v1")
MODEL = os.environ.get("AGENT_MODEL", "mock-model")
MAX_STEPS = 6

TOOLS: list[dict[str, Any]] = [
    {
        "type": "function",
        "function": {
            "name": "calculator",
            "description": "计算一个算术表达式",
            "parameters": {
                "type": "object",
                "properties": {"expression": {"type": "string"}},
                "required": ["expression"],
            },
        },
    },
    {
        "type": "function",
        "function": {
            "name": "get_time",
            "description": "获取当前 UTC 时间",
            "parameters": {"type": "object", "properties": {}},
        },
    },
]


def _safe_eval(expression: str) -> float:
    """只允许算术 AST 节点的计算器。"""
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


def run_tool(name: str, arguments: str) -> str:
    if name == "calculator":
        expr = json.loads(arguments).get("expression", "")
        try:
            return str(_safe_eval(expr))
        except (ValueError, ZeroDivisionError, SyntaxError) as exc:
            return f"计算错误: {exc}"
    if name == "get_time":
        return datetime.now(UTC).isoformat(timespec="seconds")
    return f"未知工具: {name}"


@dataclass(slots=True)
class ReActAgent:
    """think → tool_call → observe 循环。对代理零感知。"""

    client: OpenAI
    messages: list[dict[str, Any]] = field(default_factory=list)

    def run(self, task: str, max_tokens: int | None = None) -> tuple[str, bool]:
        """返回 (最终回答, 是否被截断)。"""
        self.messages.append({"role": "user", "content": task})
        truncated = False
        for step in range(1, MAX_STEPS + 1):
            kwargs: dict[str, Any] = {"model": MODEL, "messages": self.messages, "tools": TOOLS}
            if max_tokens is not None:
                kwargs["max_tokens"] = max_tokens
            resp = self.client.chat.completions.create(**kwargs)
            choice = resp.choices[0]
            msg = choice.message
            print(f"[step {step}] finish_reason={choice.finish_reason}")
            self.messages.append(msg.model_dump(exclude_none=True))
            if choice.finish_reason == "tool_calls" and msg.tool_calls:
                for call in msg.tool_calls:
                    result = run_tool(call.function.name, call.function.arguments)
                    print(f"  tool {call.function.name}({call.function.arguments}) -> {result}")
                    self.messages.append(
                        {"role": "tool", "tool_call_id": call.id, "content": result}
                    )
                continue
            if choice.finish_reason == "length":
                truncated = True
            return msg.content or "", truncated
        return "超出最大步数", truncated


def _client(agent_key: str) -> OpenAI:
    # 透明性的全部接入成本：一个 base_url + 一个代理签发的 key
    return OpenAI(base_url=PROXY_BASE_URL, api_key=agent_key)


def _recharge(agent_key: str, credits: float) -> None:
    body = json.dumps({"agent_key": agent_key, "credits": credits}).encode()
    url = f"{PROXY_BASE_URL.removesuffix('/v1')}/admin/recharge"
    req = urllib.request.Request(url, data=body, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req) as resp:
        print(f"  充值结果: {resp.read().decode()}")


def scenario_normal() -> None:
    agent = ReActAgent(_client("sk-agent-demo-rich"))
    answer, truncated = agent.run("帮我算一下 (12*8)+5 等于多少，算完告诉我结果。")
    print(f"正常场景完成: truncated={truncated}\n最终回答: {answer[:200]}")


def scenario_truncated() -> None:
    agent = ReActAgent(_client("sk-agent-demo-poor"))
    # 客户端想要 150 token 的长回答；账户余额 120 credits 物理上付不起 → 被硬顶截断
    answer, truncated = agent.run("请尽量详细地回答：为什么陷入层是 agent OS 内核的物理位置？", max_tokens=150)
    assert truncated, "预期被 max_tokens 硬顶截断"
    print(f"截断场景验证通过: 生成被物理截断，账单 ≤ 余额\n截断的回答: {answer[:120]}...")


def scenario_exhausted() -> None:
    key = "sk-agent-demo-broke"
    agent = ReActAgent(_client(key))
    # 余额 40：第 1 次调用后余额跌入 ≤20% 预警区，第 2 次后耗尽
    for attempt in range(1, 5):
        try:
            answer, truncated = agent.run(f"第 {attempt} 次提问：随便聊两句。", max_tokens=150)
            print(f"  第 {attempt} 次调用成功(truncated={truncated}): {answer[:60]}")
        except APIError as exc:
            print(f"  第 {attempt} 次调用被拒: {exc.status_code} {exc.body}")
            if exc.status_code == 429:
                print("耗尽挂起验证通过，执行充值恢复...")
                _recharge(key, 200)
                answer, _ = agent.run("充值后恢复提问：随便聊两句。", max_tokens=150)
                print(f"充值恢复验证通过: {answer[:60]}")
                return
            raise
    raise AssertionError("预期在 4 次调用内触发 429 budget_exhausted")


SCENARIOS = {"normal": scenario_normal, "truncated": scenario_truncated, "exhausted": scenario_exhausted}


def main() -> None:
    if len(sys.argv) != 2 or sys.argv[1] not in SCENARIOS:
        print(f"用法: python -m harness.react_agent [{'|'.join(SCENARIOS)}]")
        sys.exit(2)
    SCENARIOS[sys.argv[1]]()


if __name__ == "__main__":
    main()
