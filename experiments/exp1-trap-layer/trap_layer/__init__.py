"""exp1-trap-layer：llmos 陷入层原型。

验证三个危险假设：
1. 凭证托管 + LLM 调用拦截可透明插入（OpenAI 兼容代理）
2. max_tokens 硬顶可物理截断预算透支
3. WAL 批量组提交在 5k 扣减/s 下不拖垮热路径
"""

__version__ = "0.1.0"
