# llmos 任务管理器桌面壳(Tauri 2,W32-A 只读半)

可信桌面 Task Manager 外壳:TypeScript 前端 + Rust 后端命令层。后端每条
inspect 命令都经 **ADR-0011 challenge-response 认证入口**
(`nlos-system-control` 的 `dispatch_over_authenticated_socket`)连接真实
SystemControl IPC 服务,前端渲染 SABI Receipt 数据。**写入/控制动作是
W32-B;parity 钉死是 W32-C**——本壳不含任何 mutation 派发路径。

目录独立:本目录自带 `package.json` 与 `src-tauri/Cargo.toml`(后者含空
`[workspace]` 表,是独立 workspace 根),不改动仓库根 `Cargo.toml` 的
members,对 `crates/` 的依赖只以相对 path dep 出现在 `src-tauri/Cargo.toml`。

## 结构

```text
desktop/
├── index.html / vite.config.ts / tsconfig.json / package.json   # 前端壳
├── src/                    # TypeScript 前端(views:恢复/语义/任务/进程/资源/指标/一致性自检/配置)
└── src-tauri/
    ├── Cargo.toml          # 独立 workspace 根;path deps → ../../crates/*
    ├── tauri.conf.json     # bundle.active=false(打包/签名是后续波次)
    ├── src/
    │   ├── lib.rs          # 命令注册
    │   ├── ipc.rs          # 认证 IPC 客户端接线 + 全部 #[tauri::command]
    │   ├── dto.rs          # ControlReceipt → JSON DTO 单一投影点(含 receipt_hex)
    │   ├── error.rs        # 类型化 DesktopError { code, message },零 unwrap
    │   └── devfixture.rs   # feature `dev-fixture`:双入口开发夹具服务
    ├── examples/dev_server.rs        # 开发夹具服务器(认证 + plain 双入口)
    └── tests/authenticated_read_side.rs  # 认证入口真实 dispatch 集成测试
```

## 构建与运行(本机 macOS 验证过)

前置:Node ≥ 20、Rust 1.97+(与仓库 rust-toolchain 一致)、Xcode CLT。

```sh
cd desktop
npm install
npm run build          # tsc + vite build → dist/(cargo build 的前置)
```

Rust 侧检查/构建(在 `src-tauri/` 下,cargo 会自动拉取 tauri 2 依赖树):

```sh
cd src-tauri
cargo build            # 或在 desktop/ 下 npm run tauri build(先跑 npm run build)
cargo test --features dev-fixture
cargo clippy --all-targets -- --deny warnings
cargo fmt --check
```

开发态窗口:`npm run tauri dev`(Vite :1420 + Rust 热构建)。

## 连接配置(无明文捷径,无入库密钥)

GUI 三要素 + 自检二要素,来源按优先级:环境变量 → 「连接配置」页会话内
设置(不落盘):

| 环境变量 | 含义 |
| --- | --- |
| `LLMOS_DESKTOP_SOCKET` | SystemControl 认证 Unix socket 路径(**唯一派发入口**) |
| `LLMOS_DESKTOP_PRINCIPAL` | principal id,32 hex 字符 |
| `LLMOS_DESKTOP_KEY_FILE` | Ed25519 种子文件路径(64 hex 字符,建议 0600) |
| `LLMOS_DESKTOP_CLI_SOCKET` | (仅自检)plain 入口 socket,供 CLI 比对 |
| `LLMOS_DESKTOP_CLI` | (仅自检)`system-control-cli` 二进制路径;缺省探测 `../../target/debug/system-control-cli` |

私钥在每次派发时从密钥文件读取,进程内不缓存、不入仓库、不写日志。

## 本地演示(开发夹具)

`dev-fixture` feature 提供一个与仓库集成测试同形态的真实服务夹具:真实
`IdentityAuthority`(Ed25519 验签)、真实 `AuthorityClock`(系统墙钟 +
SQLite 持久化)、真实 `SqliteTaskAuthority`(含一条 escalated 恢复计划),
同时开两个入口——认证入口(GUI 用)与 plain 入口(CLI 比对用)。

```sh
# 1. 构建真实 CLI(plain 入口客户端,一致性比对用)
cargo build -p nlos-system-control

# 2. 启动夹具(打印全部所需环境变量与密钥文件路径)
cd desktop && npm run dev:server

# 3. 另一个终端,按夹具输出 export 环境变量后启动 GUI
npm run tauri dev
```

夹具演示数据:恢复总览应显示 worker `BackingOff`、`durable_escalated=1`、
一条 escalated 告警;「任务查询」输入夹具打印的 `plan_id`;「一致性自检」
选 `inspect-health` 运行应显示 `matched`。

## 一致性自检(parity approach)

- **运行时自检(已实现)**:GUI 的 `parity_check` 把同一只读命令派发两次——
  GUI 经认证入口,真实 `system-control-cli` 二进制经 plain 入口——比对两侧
  `ControlReceipt::to_bytes` 的 hex(CLI stdout 首行 `RECEIPT <hex>`)。
  这与 B-TASK-006L 已固化的「in-process / plain-IPC / authenticated 三入口
  receipt 字节级一致」契约同源,桌面侧每次交换后也在回执页显示
  `receipt_hex` 供人工比对。
- **CI 钉死(W32-C)**:把 `parity_check` 的比对逻辑下沉为仓库级测试——同一
  夹具同时服务三入口,断言 GUI(认证)、CLI 子进程(plain)与 in-process
  三份 receipt hex 逐字节相等;矩阵覆盖成功读、typed NotFound、typed Rights
  拒绝三形态(对齐 `control_ipc_auth.rs` 的既有 parity 测试)。本目录的
  `tests/authenticated_read_side.rs::authenticated_inspect_health_matches_plain_entry_bytes`
  已是它的最小前驱(认证 vs plain 字节一致)。

## 边界与后续波次

- **只读半**:命令面只有 `get` 投影(health/semantic/task/process/resource/
  metrics);ack/resume/pause/cancel 等 mutation 一律不存在于本壳(W32-B)。
- **process/resource inspector 未接线**:`InspectProcess`/`InspectResource`
  的回执仍是完整认证 GET 交换,但客户端侧 inspector 传 `None`,失败面为
  类型化 `NOT_FOUND`(「backend is not wired」)——与 CLI 行为一致(parity
  不破坏)。接入宿主 ProcessAuthority/ResourceAuthority 属后续接线。
- **平台**:认证入口(`nlos-system-control::auth`)目前仅 Unix;Windows
  named-pipe 认证接线、bundle/图标/签名、多窗口为后续波次。
- **tauri.conf.json** `bundle.active=false`:`cargo build` 可全量编译链接,
  完整 `tauri build` 打包(bundler/签名)未纳入本波次验证。
