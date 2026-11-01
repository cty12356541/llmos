//! Cross-platform dual-layer Wasm strong sandbox plane for untrusted
//! third-party community packages (`Stage E` `E6`, `ADR-0027`;
//! `[DRV-PROTO-001]`, `[DRV-LIFE-001]`, `[TCB-BYPASS-001]`,
//! `[TCB-EGRESS-001]`, `[TCB-DOWNGRADE-001]`, `[TUI-CONSENT-001]`).
//!
//! `W80-0-SCAFFOLD` 仅注册空骨架:实现由 `W80-E6A-WASM-SANDBOX` 车道在独立
//! worktree 中落地(独立宿主 Process + Wasmtime WASI 白名单 + 内存页/CPU
//! fuel 硬限 + 零宿主环境继承),并跑通路径穿越/网络外连/配额超限逃逸对抗
//! 矩阵,正式闭合 `U-9`(`RISK-E-04`)。

#![forbid(unsafe_code)]

#[cfg(test)]
mod tests {
    #[test]
    fn wasm_sandbox_skeleton_registered_as_workspace_crate_45() {
        // 骨架标记:实际隔离语义与逃逸对抗矩阵由 W80-E6A 车道以 TDD 落地。
    }
}
