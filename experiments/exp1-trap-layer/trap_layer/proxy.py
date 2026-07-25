"""陷入层代理：OpenAI 兼容入口 + 凭证托管 + 预算扣减 + max_tokens 硬顶 + WAL。

请求生命周期（非流式与流式同构）：
1. 认证：Bearer <代理签发的 agent key> → 预算账户（凭证托管：agent 永远接触不到真实 provider key）
2. 挂起检查：余额 ≤0 → 429 budget_exhausted
3. 硬顶注入：剩余 credits ÷ 单价 → max_tokens 上限，覆盖客户端更大的值（透支物理截断）
4. 转发 provider（mock 或真实兼容端点）
5. 结算：按 usage × 定价表扣减（钳制在余额内），追加 WAL 流水（批量组提交）
6. 预警：余额 ≤20% 累计额度 → X-Budget-Warning: true
"""

from __future__ import annotations

import json
import uuid
from dataclasses import asdict
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from typing import Any

from fastapi import Depends, FastAPI, Header, HTTPException, Request
from fastapi.responses import JSONResponse, StreamingResponse

from .budget import AccountState, BudgetManager, estimate_prompt_tokens
from .config import Settings
from .providers.base import ChatProvider, ProviderError
from .wal import WalWriter

JsonDict = dict[str, Any]


def _error_response(status: int, message: str, err_type: str) -> JSONResponse:
    """OpenAI 风格错误体。"""
    return JSONResponse(
        status_code=status,
        content={"error": {"message": message, "type": err_type, "code": err_type}},
    )


def _budget_headers(account: AccountState) -> dict[str, str]:
    headers = {"X-Budget-Remaining": f"{account.balance:.4f}"}
    if account.warning:
        headers["X-Budget-Warning"] = "true"
    return headers


def create_app(
    settings: Settings,
    budget: BudgetManager,
    wal: WalWriter,
    provider: ChatProvider,
) -> FastAPI:
    """应用工厂：测试与生产共用，依赖全部显式注入。"""

    @asynccontextmanager
    async def lifespan(app: FastAPI) -> AsyncIterator[None]:
        await wal.start()  # 幂等：测试若已手动启动则跳过
        yield
        await provider.aclose()
        await wal.close()

    app = FastAPI(title="llmos exp1 trap-layer proxy", lifespan=lifespan)
    app.state.settings = settings
    app.state.budget = budget
    app.state.wal = wal
    app.state.provider = provider

    # ---- 依赖：agent key 认证 ----

    async def require_account(authorization: str = Header(default="")) -> AccountState:
        scheme, _, key = authorization.partition(" ")
        if scheme.lower() != "bearer" or not key:
            raise HTTPException(status_code=401, detail="缺少 Bearer agent key")
        account = budget.account_for_key(key)
        if account is None:
            raise HTTPException(status_code=401, detail="未知 agent key")
        return account

    async def require_admin(x_admin_token: str = Header(default="")) -> None:
        # 原型语义：仅在配置了 ADMIN_TOKEN 时强制校验
        if settings.admin_token and x_admin_token != settings.admin_token:
            raise HTTPException(status_code=403, detail="admin token 无效")

    # ---- 计费主路径 ----

    def apply_hard_cap(body: JsonDict, account: AccountState, model: str) -> int:
        """注入 max_tokens 硬顶，返回最终上限。透支截断的物理保证点。"""
        est_prompt = estimate_prompt_tokens(body)
        cap = budget.max_completion_tokens_affordable(account, model, est_prompt)
        client_max = body.get("max_tokens")
        if not isinstance(client_max, int) or client_max > cap:
            body["max_tokens"] = cap
        return int(body["max_tokens"])

    def settle_and_record(
        account: AccountState,
        model: str,
        usage: JsonDict,
        request_id: str,
        stream: bool,
    ) -> None:
        settlement = budget.settle(
            account,
            model,
            prompt_tokens=int(usage.get("prompt_tokens", 0)),
            completion_tokens=int(usage.get("completion_tokens", 0)),
        )
        wal.append(
            request_id=request_id,
            agent_id=settlement.agent_id,
            key_fingerprint=account.key_fingerprint,
            model=model,
            prompt_tokens=settlement.prompt_tokens,
            completion_tokens=settlement.completion_tokens,
            cost=settlement.cost,
            charged=settlement.charged,
            balance_after=settlement.balance_after,
            stream=stream,
        )

    @app.post("/v1/chat/completions", response_model=None)
    async def chat_completions(
        request: Request,
        account: AccountState = Depends(require_account),
    ) -> JSONResponse | StreamingResponse:
        # 挂起：余额 ≤0 物理拒绝
        if account.exhausted:
            return _error_response(
                429,
                f"预算耗尽，账户 {account.agent_id} 已挂起，请充值",
                "budget_exhausted",
            )

        body: JsonDict = await request.json()
        requested_model = str(body.get("model") or settings.llm_model)
        if not settings.use_mock:
            body["model"] = settings.llm_model  # 真实模型由代理决定，agent 无权指定

        cap = apply_hard_cap(body, account, requested_model)
        is_stream = bool(body.get("stream"))
        request_id = uuid.uuid4().hex
        headers = _budget_headers(account)
        headers["X-Budget-Max-Tokens-Cap"] = str(cap)

        if not is_stream:
            try:
                resp = await provider.chat_completion(body)
            except ProviderError as exc:
                return _error_response(502, f"下游 provider 错误: {exc}", "provider_error")
            usage = resp.get("usage") or {
                "prompt_tokens": estimate_prompt_tokens(body),
                "completion_tokens": 0,
            }
            settle_and_record(account, requested_model, usage, request_id, stream=False)
            headers["X-Budget-Remaining"] = f"{account.balance:.4f}"
            return JSONResponse(content=resp, headers=headers)

        # ---- 流式：chunk 增量计量，结束后统一结算 ----
        try:
            chunk_iter = provider.chat_completion_stream(body)
        except ProviderError as exc:
            return _error_response(502, f"下游 provider 错误: {exc}", "provider_error")

        async def event_stream() -> AsyncIterator[bytes]:
            usage: JsonDict | None = None
            counted_chunks = 0  # 兜底计量：provider 不回报 usage 时按 chunk 数估算
            try:
                async for chunk in chunk_iter:
                    chunk_usage = chunk.get("usage")
                    if isinstance(chunk_usage, dict) and chunk_usage:
                        usage = chunk_usage
                        continue  # usage chunk 不转发给 agent（OpenAI 语义下也一样可转发，这里选择吞掉）
                    delta = (chunk.get("choices") or [{}])[0].get("delta") or {}
                    if delta.get("content"):
                        counted_chunks += 1
                    yield f"data: {json.dumps(chunk, ensure_ascii=False)}\n\n".encode()
                yield b"data: [DONE]\n\n"
            except ProviderError:
                yield b'data: {"error": {"type": "provider_error"}}\n\n'
            finally:
                # 统一结算：优先 provider 回报的 usage，兜底用代理自计数
                final_usage = usage or {
                    "prompt_tokens": estimate_prompt_tokens(body),
                    "completion_tokens": counted_chunks,
                }
                settle_and_record(account, requested_model, final_usage, request_id, stream=True)

        return StreamingResponse(
            event_stream(),
            media_type="text/event-stream",
            headers=headers,
        )

    # ---- 管理面 ----

    @app.post("/admin/recharge", dependencies=[Depends(require_admin)])
    async def recharge(request: Request) -> JSONResponse:
        body = await request.json()
        key = str(body.get("agent_key", ""))
        credits = float(body.get("credits", 0))
        account = budget.recharge(key, credits)
        if account is None:
            return _error_response(400, "未知 agent key 或非法充值额", "invalid_recharge")
        return JSONResponse(
            content={
                "agent_id": account.agent_id,
                "balance": round(account.balance, 6),
                "total_granted": round(account.total_granted, 6),
                "exhausted": account.exhausted,
            }
        )

    @app.get("/admin/accounts", dependencies=[Depends(require_admin)])
    async def accounts() -> JsonDict:
        return {"accounts": budget.snapshot(), "wal": asdict(wal.stats)}

    @app.get("/health")
    async def health() -> JsonDict:
        return {"status": "ok", "mode": "mock" if settings.use_mock else "live"}

    return app
