# ecosystem-drivers — W80-E6B 第三方用户态 Driver 参考样板

Stage E `E6`(`ADR-0027` §2.4)的解耦第三方参考生态样板之一:两个独立"发布者"的用户态
Driver 适配器,均以**版本化扩展**封装(`DRV-PROTO-001`),零内核私有依赖,零新增第三方
依赖(仅 path 依赖仓库内公共 crate 与 `ecosystem-driver-kit`)。

| Crate | 形态 | 规格覆盖 |
|---|---|---|
| `ecosystem-driver-kit` | 共享 kit:有界 JSON codec、四类 `EffectDescriptor` 声明解析、崩溃 `UNCERTAIN` 同幂等键 reconcile 围栏、版本化扩展协商 | `DRV-LIFE-001` / `KABI-UNCERTAIN-001` / `DRV-NEGO-001` |
| `mcp-bridge-driver` | MCP 兼容桥:JSON-RPC 2.0 + 换行分隔 stdio 帧、`initialize` 握手版本协商、`tools/list` / `tools/call`、四类效果工具目录、崩溃对账 | `DRV-PROTO-001`(MCP)/ `DRV-NEGO-001` / `DRV-LIFE-001` |
| `openai-compat-driver` | OpenAI-compatible 网关:`/v1/chat/completions` 请求/响应 codec、SSE 流式分片聚合、真实 `ResourceAuthority` 五维 Token 预算计量 | `DRV-PROTO-001`(OpenAI 兼容)/ 五维预算 |

包树:`<crate>/package`(v1.0.0)与 `<crate>/package-v1.1.0`(SameMajor 升级树);
`<crate>/sealed/` 存放冻结签名 `.nlos-pkg` 与封存记录(由
`examples/ecosystem-apps/seal-n1-baseline.sh` 一条命令复现)。

## 验证

```bash
cd examples/ecosystem-drivers
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```
