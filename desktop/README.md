# llmos 任务管理器桌面壳(Tauri 2,W32-A 只读半 + W32-B 写入半 + W32-D 权限/预算可见 + W32-E 资源监控 + W32-F 应用表面呈现)

可信桌面 Task Manager 外壳:TypeScript 前端 + Rust 后端命令层。后端每条
inspect/控制命令都经 **ADR-0011 challenge-response 认证入口**
(`nlos-system-control` 的 `dispatch_over_authenticated_socket`)连接真实
SystemControl IPC 服务,前端渲染 SABI Receipt 数据。W32-B 加入授权控制
动作(ack/resume/pause/cancel/kill/throttle/reclaim)的 GUI 派发与
Receipt 展示;W32-D 加入可信权限 UI 最小版(B5-5 前半):授权/预算/成本
可见——`InspectResource` 有界成本事实经真实 `ResourceAuthorityInspector`
组装,与权威及 CLI parity 一致。W32-E 加入 Resource Monitor 最小版
(B5-5 后半):消费既有 OpenMetrics 指标面(三恢复域只读导出命令)并
结构化展示,带刷新,无任何新控制路径。W32-F 加入应用表面呈现(B2-2 /
ROAD-B-002 第四能力维度):Application 声明的 UI Surface 经本地应用
权威读回并以窗口/面板卡呈现(声明 → 呈现最小链,非窗口管理系统)。
**parity 钉死是 W32-C**——本壳
只带读路径自检 + 一条 pause-operation 写路径探针 + W32-D 成本事实自检。

目录独立:本目录自带 `package.json` 与 `src-tauri/Cargo.toml`(后者含空
`[workspace]` 表,是独立 workspace 根),不改动仓库根 `Cargo.toml` 的
members,对 `crates/` 的依赖只以相对 path dep 出现在 `src-tauri/Cargo.toml`。

## 结构

```text
desktop/
├── index.html / vite.config.ts / tsconfig.json / package.json   # 前端壳
├── src/                    # TypeScript 前端(views:恢复/语义/资源/任务/进程/资源查询/指标/资源监控/应用表面/控制动作/权限预算/一致性自检/配置)
└── src-tauri/
    ├── Cargo.toml          # 独立 workspace 根;path deps → ../../crates/*
    ├── tauri.conf.json     # bundle.active=false(打包/签名是后续波次)
    ├── src/
    │   ├── lib.rs          # 命令注册
    │   ├── ipc.rs          # 认证 IPC 客户端接线 + 全部 #[tauri::command](读 + W32-B 写)
    │   ├── dto.rs          # ControlReceipt → JSON DTO 单一投影点(含 receipt_hex;穷尽匹配无通配臂)
    │   ├── error.rs        # 类型化 DesktopError { code, message },零 unwrap
    │   ├── surfaces.rs     # W32-F:UI Surface 呈现核心(本地应用权威直读投影)
    │   └── devfixture.rs   # feature `dev-fixture`:双入口开发夹具服务(+W32-D 结清资源链)
    ├── examples/dev_server.rs        # 开发夹具服务器(认证 + plain 双入口)
    └── tests/
        ├── authenticated_read_side.rs       # W32-A:认证读侧集成测试
        ├── authenticated_write_side.rs      # W32-B:认证写侧集成测试
        ├── authenticated_permission_side.rs # W32-D:权限/预算/成本集成测试
        ├── resource_monitor_metrics_side.rs # W32-E:三域指标消费 + parity 集成测试
        └── surface_presentation_side.rs     # W32-F:声明→呈现最小链集成测试
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
| `LLMOS_DESKTOP_RESOURCE_ROOT` | (W32-D,仅权限/预算视图)本地资源权威根目录(`resource-authority.db` 所在目录);未设置时成本查询保持未接线形态 |
| `LLMOS_DESKTOP_APPLICATION_ROOT` | (W32-F,仅应用表面视图)本地应用权威根目录(`application-authority.db` 所在目录);未设置时表面呈现为类型化 CONFIG 拒绝(无未接线回退形态) |

私钥在每次派发时从密钥文件读取,进程内不缓存、不入仓库、不写日志。

## 本地演示(开发夹具)

`dev-fixture` feature 提供一个与仓库集成测试同形态的真实服务夹具:真实
`IdentityAuthority`(Ed25519 验签)、真实 `AuthorityClock`(系统墙钟 +
SQLite 持久化)、真实 `SqliteTaskAuthority`(含一条 escalated 恢复计划)
与真实 `ResourceAuthority`(W32-D:一条已结清预留,driver→account→quote→
reserve→activate→consume×2→finalize 全链,上界 100/高水位 70/2 次消费),
同时开两个入口——认证入口(GUI 用)与 plain 入口(CLI 比对用)。

```sh
# 1. 构建真实 CLI(plain 入口客户端,一致性比对用)
cargo build -p nlos-system-control

# 2. 启动夹具(打印全部所需环境变量与密钥文件路径)
cd desktop && npm run dev:server

# 3. 另一个终端,按夹具输出 export 环境变量后启动 GUI
#    (写路径自检探针执行真实 mutation,需显式 opt-in 宿主保护门)
export LLMOS_DESKTOP_ALLOW_WRITE_PARITY=1
npm run tauri dev
```

夹具演示数据:恢复总览应显示 worker `BackingOff`、`durable_escalated=1`、
一条 escalated 告警;「任务查询」输入夹具打印的 `plan_id`;「一致性自检」
选 `inspect-health` 运行应显示 `matched`;「控制动作」页在 artifact 域
「巡检读取告警」后对夹具告警点「确认告警(ack)」应得到 acknowledged
回执(真实 TaskAuthority CAS mutation),回「恢复总览」刷新可见
`durable_unacknowledged_escalated=0`;在 semantic/resource 域对同一
plan_id 下发 ack/resume 应得到类型化 `NOT_FOUND`(域路由不串);
「操作控制」下发任意动作(如 pause)在夹具上得到类型化
`NOT_FOUND`(executor 未接线)——kill 需两步确认。「一致性自检」页
「写路径自检」需先 `export LLMOS_DESKTOP_ALLOW_WRITE_PARITY=1`
(探针执行真实 pause-operation mutation 的宿主保护门;未设置时命令
以类型化 CONFIG 错误拒绝),选默认参数运行应显示
`matched`(两侧同为确定性失败回执)。
「权限/预算」页:授权事实卡显示会话 principal 与控制能力句柄
(slot=9 generation=1),「验证控制面授权」应得到真实巡检回执(权限通过);
「查询成本」输入夹具打印的 `reservation_id` 应显示
`upper_bound=100`、`usage_high_water=70`、`consumption_count=2`(真实
`ResourceCostReceipt` 投影,结余派生行 = 30);「成本事实自检」对同一
reservation 运行应显示 `matched`(结清事实不可变)。「一致性自检」下拉
已补 `inspect-resource-health`/`export-resource-metrics`(W32-B 遗留小项)。
「资源监控」页点「刷新(三域,经认证 IPC)」应显示三域结构化指标表:
artifact 域含 worker 生命周期(`backing_off=1`)、
`nlos_artifact_recovery_cycles_total 4`、plans_inspected=3、
plans_finalized=2、retry_delay=250ms、durable_escalated=1;semantic/
resource 域目录逐族在场(夹具上全 0);每域卡片页脚有 receipt hex,
`<details>` 可展开原始 OpenMetrics 文本,与 CLI
`system-control-cli <plain_socket> export-metrics` 输出的 `RECEIPT`
hex 一致。

## 授权控制动作(W32-B 写入半)

- **动作面**:恢复告警 `ack`/`resume`(artifact/semantic/resource 三域)
  与操作控制 `pause`/`resume`/`cancel`/`kill`/`throttle`/`reclaim`,
  全部经后端 `submit_control` 单一入口:动作 → 真实 `ControlCommand`
  (§25.3 命令身份在派发时由 /dev/urandom 新生成)→ ADR-0011 认证入口。
  CAS 预期取自 inspect 状态(告警行的 `total_failures`;操作目标的
  generation/revision,进程可先经「进程查询」读取)。
- **Receipt 展示**:每个动作按类型渲染(acknowledged/resumed/
  operation_* 各自标题 + `receipt_reference`),回执页脚恒显
  `control_command_id`/`correlation_id`/`receipt_hex`;类型化失败
  (SabiFailure)与成功形态视觉区分(红/绿边框),无原始错误倾倒。
- **kill 两步确认**:kill-operation 首次点击只武装(5 秒窗口,按钮变红
  并提示),再次点击才真正下发;纯应用内,无新控制路径。
- **边界**:操作执行 seam(`OperationCommandExecutor`)的宿主接线是后续
  波次——夹具/未接线宿主上 pause 族回执为类型化 `NOT_FOUND`(executor
  未接线),派发与回执本身真实完整;恢复告警 ack/resume 在夹具上是真实
  mutation。

## 权限/预算视图(W32-D 可信权限 UI 最小版,B5-5 前半)

- **授权事实(客户端路径事实,非 inspect 数据)**:会话 principal
  (ADR-0011 认证身份)、资源权威接线状态、SystemControl 服务名与固定
  控制能力句柄(`CONTROL_CAPABILITY_SLOT=9`/`GENERATION=1`,每条派发
  信封携带,服务端授权检查拒绝时回执为类型化 `RIGHTS` 失败);
  「验证控制面授权」按钮经真实 dispatch 取回执佐证。
- **预算/成本可见性**:`inspect_resource_cost` 命令经认证入口派发
  `InspectResource`;会话配置 `resource_root`
  (`LLMOS_DESKTOP_RESOURCE_ROOT` 或「连接配置」页)指向本地资源权威
  根目录时,后端以真实 `ResourceAuthorityInspector`(每次派发即时打开,
  WAL 多进程读安全,不缓存句柄)组装五个有界成本事实
  (reservation_id/account_id/upper_bound/usage_high_water/
  consumption_count,来自权威 `inspect_cost_receipt` 只对**已结清**
  预留开放的不可变投影)+ 一行标注「派生」的结余
  (upper_bound − usage_high_water)。未配置时 inspector 传 `None`,
  回执为诚实的类型化 `NOT_FOUND`(未接线),不伪造数据。
- **一致性纪律**:`parity_check`(GUI↔CLI 字节比对)与全部既有命令
  恒走未接线 `dispatch_control`,不受 resource_root 影响(有集成测试
  钉死);「成本事实自检」(`cost_fact_check`)把同一 reservation 两次
  独立经认证入口派发,逐字段比较渲染事实与直接复检并比对 receipt hex
  ——结清事实不可变,必须 `matched`。
- **缺口登记(诚实边界)**:能力签发/衰减/撤销账本
  (`nlos-capability`)、call_limit 剩余与消耗回执、资源报价明细
  (demand_capacity/pricing_version/valid_until)、预留状态机与多维需求、
  账户余额、结清明细回执(refund_credit/逐条 ConsumptionReceipt)——
  均无 IPC inspect 面,视图不渲染,以静态缺口表列出(证据
  `b-gui-001` §W32-D 逐条登记,供后续车道补面)。

## 资源监控视图(W32-E Resource Monitor 最小版,B5-5 后半)

- **消费机制(既有指标面,零新面)**:OpenMetrics 文本**已经**经 SABI
  IPC 可达——`ExportMetrics` / `ExportSemanticMetrics` /
  `ExportResourceMetrics` 是三条既有只读 `ControlCommand`(W27-A/G8
  目录),回执 outcome 即 `MetricsExported { openmetrics_text }`
  (`OpenMetricsRenderer::render` 的确定性文本)。视图每次刷新对三域各
  经认证入口派发一条导出命令(后端 `export_metrics` /
  `export_semantic_metrics` / `export_resource_metrics` 三个薄命令,
  其中 resource 域为 W32-E 补接线——W32-A 只接了前两域),**无任何新
  控制路径、无 in-process 回退**。
- **结构化展示**:前端 `src/openmetrics.ts` 对确定性文本做严格解析
  (`# TYPE` 族头 + 无标签/带标签十进制样本行),按域分组渲染计数/
  gauge 表(指标族/类型/标签/值);计数器值为 u64 十进制文本,按原样
  显示不做数值换算。无法识别的行原样显示,不静默丢弃;解析出的族若
  前缀不属于该域,单独分组如实呈现。
- **刷新**:手动「刷新(三域,经认证 IPC)」+ 可选 5 秒自动刷新;每次
  刷新都是三条完整的真实认证 dispatch(拉模型)。每域卡片页脚恒显
  `control_command_id`/`correlation_id`/`receipt_hex`(与 CLI
  `RECEIPT` 行同一等价契约);原始 OpenMetrics 文本折叠在
  `<details>` 内可展开比对。
- **集成测试**(`tests/resource_monitor_metrics_side.rs`):(1) 三域
  导出经认证入口回执携带 OpenMetrics 文本,artifact/semantic/resource
  目录逐族在场且取值 == 夹具权威健康事实(不发明指标);(2) 同批命令
  经 plain 入口(CLI 同路)派发,receipt 字节一致。
- **缺口登记(诚实边界)**:无 scrape/流式指标端点(B-TASK-006M 未竟
  项,刷新即重新拉取);宿主级资源用量指标(进程 CPU/内存/IO、预留
  实时用量)无任何既有导出面——指标目录只覆盖恢复目录(三域 26 族 +
  worker 生命周期),视图不发明;以上在视图内以静态缺口卡列出(证据
  `b-gui-001` §W32-E)。

## 应用表面视图(W32-F UI Surface 呈现,B2-2 / ROAD-B-002 第四能力维度)

「Application 声明 surface → 窗口呈现」最小链(声明 → 呈现,非窗口管理系统):

- **机制(W32-D resource_root 同款本地权威直读)**:会话配置
  `application_root`(`LLMOS_DESKTOP_APPLICATION_ROOT` 或「连接配置」页)
  指向本地应用权威根目录时,`present_surfaces` 命令按包身份(32 hex)
  经真实 `ApplicationAuthority` 读回该应用的 durable 表面登记
  (`inspect_surfaces`,nlos-application schema v8),投影为呈现 DTO。
  每次呈现即时打开权威(WAL 多进程读安全),不缓存句柄。
- **窗口形呈现**:当前安装代际的每个已登记表面以窗口/面板卡呈现——
  标题栏(kind 徽标 + 声明 title)+ 元数据行(surface_id/kind/title/
  entry_name 声明内容引用/登记幂等键/时间)+ 明示的内容占位(「呈现
  声明元数据;entry 载荷渲染属后续车道」)。DTO 不发明任何字段:呈现
  的每一行都是 durable 声明事实的逐位投影。
- **stale 代际不呈现**(DUI-WINDOW-001 最小版):内容更新推进代际后,
  旧代际的登记仍是 durable 事实(`inspect_surfaces` 可读)但不再进入
  可呈现集;gen 2 重声明后恢复呈现。非 `installed` 状态(disabled/
  uninstalled)如实投影为空集 + 状态行,不报错。从未安装的包是类型化
  `NOT_FOUND`(事实读回);未配置 `application_root` 是类型化 `CONFIG`
  拒绝——本视图没有未接线回退形态,不配置即不呈现。
- **声明侧(`crates/nlos-application`)**:manifest additive `surfaces`
  段——`PackageSurfaceDeclaration { surface_id, kind(window|panel),
  title, entry_name? }` + 共享 typed 校验
  (`validate_surface_declarations`:非空/唯一 id/文本界),登记面
  `register_surfaces` 把声明段绑定到安装代际与 manifest digest(声明
  陈旧内容 → 类型化拒绝,不静默改绑),幂等 replay 逐位相等。应用侧
  (样板驱动)经公共 API 登记;桌面只消费 inspect 面。
- **集成测试**(`tests/surface_presentation_side.rs`):小夹具样例包
  (带 surfaces 声明段)→ 真实验签 → install → 登记 → **第二个独立权威
  句柄上的桌面呈现**(读者进程形态)逐字段断言;stale 代际空集、重声明
  恢复、未知包 NOT_FOUND、未配置 CONFIG 拒绝均钉死。
- **表面开合终态(W39-D / §25)**:`window_lifecycle::SurfaceLifecycle`
  落地规范链 `REGISTERED → CREATED → PRESENTED ↔ HIDDEN → CLOSED` 上的
  open/close 动作,open 后 close 到达终态 `CLOSED`(无窗口管理器/合成器)。
- **呈现边界登记(诚实边界)**:entry 载荷内容渲染(artifact 字节→表
  面内容)、焦点/输入路由、窗口几何/多窗口编排、Surface 域 SABI
  ControlCommand IPC 面——均不在本最小链内,视图以静态缺口卡声明缺席
  (证据 b-gui-001 §W32-F)。

## 一致性自检(parity approach)

- **运行时自检·读路径(已实现)**:GUI 的 `parity_check` 把同一只读命令派发两次——
  GUI 经认证入口,真实 `system-control-cli` 二进制经 plain 入口——比对两侧
  `ControlReceipt::to_bytes` 的 hex(CLI stdout 首行 `RECEIPT <hex>`)。
  这与 B-TASK-006L 已固化的「in-process / plain-IPC / authenticated 三入口
  receipt 字节级一致」契约同源,桌面侧每次交换后也在回执页显示
  `receipt_hex` 供人工比对。
- **运行时自检·写路径探针(W32-B)**:`parity_check_write` 把同一方法扩展到
  一条写路径命令(pause-operation):每次运行生成新 §25.3 命令 id,GUI 与
  CLI 以字节同一的命令各派发一次,比对 receipt hex。开发夹具(未接线
  executor)上两侧同为确定性类型化 `NOT_FOUND` 失败回执,应显示 matched;
  已接线执行器的宿主上第一次派发可能真实暂停目标、第二次按 idempotency/CAS
  纪律回 `CONFLICT`——mismatch 如实显示,这正是纪律在工作。集成测试
  `operation_commands_dispatch_real_submits_with_typed_unwired_failures`
  已在测试内钉住「同字节 pause 命令,认证入口与 plain 入口失败回执逐字节
  相等」。
- **CI 钉死(W32-C)**:把 parity 比对逻辑下沉为仓库级测试——同一
  夹具同时服务三入口,断言 GUI(认证)、CLI 子进程(plain)与 in-process
  三份 receipt hex 逐字节相等;矩阵覆盖成功读、typed NotFound、typed Rights
  拒绝三形态(对齐 `control_ipc_auth.rs` 的既有 parity 测试),并把写路径
  扩展为成功/失败多形态。本目录的
  `tests/authenticated_read_side.rs::authenticated_inspect_health_matches_plain_entry_bytes`
  与上述写侧断言是它的最小前驱(认证 vs plain 字节一致)。

## 边界与后续波次

- **process/resource inspector 未接线(默认路径)**:`InspectProcess`/
  `InspectResource` 在 `parity_check` 与既有命令路径上客户端侧 inspector
  仍传 `None`,失败面为类型化 `NOT_FOUND`(「backend is not wired」)——
  与 CLI 行为一致(parity 不破坏)。W32-D 的预算/成本查询是**可选本地
  增强**:仅 `inspect_resource_cost`/`cost_fact_check` 两条命令在会话
  配置了 `resource_root` 时经真实 `ResourceAuthorityInspector` 组装事实,
  该路径的回执与 CLI(未接线)字节不同属预期——它是本地权威直读视图,
  不是 CLI parity 面;「与 authority 一致」由集成测试
  (`resource_cost_inspect_matches_authority_facts`)对权威
  `inspect_cost_receipt` 直接读数钉死。接入宿主
  ProcessAuthority/ResourceAuthority 属后续接线;操作控制(pause/kill 族)
  的 `OperationCommandExecutor` 宿主接线同理,未接线时的回执形态与
  inspector 一致(类型化 NOT_FOUND,不伪造成功)。
- **平台**:认证入口(`nlos-system-control::auth`)目前仅 Unix;Windows
  named-pipe 认证接线、bundle/图标/签名、多窗口为后续波次。
- **tauri.conf.json** `bundle.active=false`:`cargo build` 可全量编译链接,
  完整 `tauri build` 打包(bundler/签名)未纳入本波次验证。


## 阶段 D 完整自然语言工业工作站扩展(`W69–W73` 已落地,`PARTIAL_PASS`)

在阶段 B/C 可信 Task Manager 底座之上,`W69–W73` 落地了符合 [ADR-0025](../docs/management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md) 与 [Stage D 桌面视觉与 UX 设计规格](../docs/superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md) 的中性工业工作站桌面架构(共注册 **108 项** `#[tauri::command]` 句柄):

1. **`D-TOKENS-KIT` (`W69`, [D-TOKENS-001](../docs/evidence/stage-d/d-tokens-001-workstation-tokens-and-sota-kit.md))**:
   - `src/styles/{tokens,surfaces-and-borders}.css` 与 `src/ui/`:`Graphite Workstation` / `Paper Workstation` 100% 实心不透明双主题(零 `backdrop-filter`、零霓虹发光、零 Emoji、`0px/2px/4px/6px` 工业圆角)、`32×20` 凹陷单色 Gerono 示波防伪印章、Nomad 三水位微条、Temporal 五段耗时瀑布条、Headlamp 非模态抽屉与面包屑、Terraform `+ / ~ / -` 爆炸半径差异表。
2. **`D-IPC-ENUM` (`W69`, [D-IPC-001](../docs/evidence/stage-d/d-ipc-001-twelve-domain-enum-and-symmetry.md))**:
   - `IPC-D1..D8` 12 域有界游标分页枚举(`limit <= 64`)、`inspect_search` / `inspect_notification` 句柄与 `disable/uninstall-application` 动作补齐、显式类型化 `ControlScope`、`PreviewControlBlastRadius`、`QueryControlCommandStatus` 及五大法定不可篡改禁区(`tests/ipc_enum_symmetry_side.rs`)。
3. **`D-COMPOSITOR-WM` (`W70`, [D-COMPOSITOR-001](../docs/evidence/stage-d/d-compositor-001-five-plane-tiling-wm-and-session.md))**:
   - `src/{compositor,wm,dock,store}/` 与 `src-tauri/src/{compositor,session,input_router,window_lifecycle}.rs`:五平面 `Z0–Z4` 工作站合成器、`4px` 间隙五模式平铺优先窗口管理器、`30px` 可信窗框、`36px` 底部贴边工作站任务栏(`L0` 环境感知与 `[DUI-LAUNCH-001]` 启动器)、`L0→L1→L2→L3` `<16ms` 渐进式披露 Store 与五输入源代际防伪路由(`tests/compositor_wm_side.rs`)。
4. **`D-OMNIBAR-MULTIMODAL` (`W71`, [D-OMNIBAR-001](../docs/evidence/stage-d/d-omnibar-001-command-intent-palette-and-multimodal.md))**:
   - `src/views/omnibar/`、`src/multimodal/` 与 `src-tauri/src/omnibar.rs`:`Z3` 按需唤起 `Ctrl+Space` `580px` 实心命令/意图面板(平时隐藏零霸屏)、`L1/L2` 零行话与 `L3` `TypedIntent` 预览、`[NL-AMBIG-001]` 高风险歧义澄清门、`InputSource::Voice` 语音流与内容寻址附件标签(`tests/omnibar_multimodal_side.rs`)。
5. **`D-TRUSTED-UI` (`W71`, [D-TRUSTED-UI-001](../docs/evidence/stage-d/d-trusted-ui-001-z4-consent-chooser-and-taint-clipboard.md))**:
   - `src/trusted-ui/` 与 `src-tauri/src/trusted_ui.rs`:`Z4` UAC/Polkit 实心安全确认屏(`350ms` 防劫持门、四行规格表、`IRREVERSIBLE` 零伪撤销)、`ChooserGrant` 衰减 Capability 选择器(零宿主裸路径泄露)与污点感知跨应用剪贴板/拖拽门控(`tests/trusted_ui_scrim_chooser_clip_side.rs`)。
6. **`D-TASK-SPACE-DAG` (`W72`, [D-TASK-SPACE-001](../docs/evidence/stage-d/d-task-space-001-l1-l3-disclosure-dag-and-attempts.md))**:
   - `src/views/task-space/` 与 `src-tauri/src/plan_editor.rs`:闭合历史 `TASK_SPACE_GAPS` (`1–3`),实现 `L1` 16 态承诺表、`L2` 单行按需非模态归因抽屉、`L3` Dagster 左到右正交 `TaskPlan` DAG 编辑器(`[PLAN-LAZY-001]` `+N 待物化步骤 · 0 进程` 折叠超节点、双路径变更分类器与 `Z4` 重授权 diff)及 Temporal `TaskAttempt` 竞速泳道(`tests/task_space_dag_override_side.rs`)。
7. **`D-NOTIFY-SEARCH-OFFLINE` (`W72`, [D-NOTIFY-SEARCH-001](../docs/evidence/stage-d/d-notify-search-001-action-token-spotlight-and-offline.md))**:
   - `src/views/{notifications,spotlight,offline-cluster}/` 与 `src-tauri/src/notify_search_offline.rs`:右下角实心通知中心(一次性 SHA-256 `ActionToken` 核销)、`Ctrl+K` 实心候选搜索(硬锁 `0x1F` 法定零写禁区与权威回跳核验)及 `[DUI-OFFLINE-001]` 本地断网自治 + `PartitionAutonomyGate` 集群态势托盘(`tests/notify_token_search_offline_side.rs`)。
8. **`D-CONTROL-SETTINGS-A11Y` Slice 1 (`W72`, [D-CONTROL-SETTINGS-001](../docs/evidence/stage-d/d-control-settings-001-twelve-domain-tree-and-settings-cas.md))**:
   - `src/views/{control-center,settings}/` 与 `src-tauri/src/settings_keychain.rs`:12 域 `SystemControlCenter`(System Informer 十层父子归属树、阶段 C `Cell` 三族租约检视与受控进程迁移、Headlamp 动作裁剪与 5+1 法定不可篡改锁、Terraform `+ / ~ / -` 爆炸半径预览与 5 秒两步武装)、四层 `SettingsDomain` CAS + watch、可撤销 `ContentTypeAssociation` 与零明文 `KeychainReference` 凭据库(`tests/control_center_settings_cas_side.rs`)。
9. **`D-CONTROL-SETTINGS-A11Y` Slice 2 & `#3 C-GUI-CAMPAIGN` (`W73`, [D-A11Y-I18N-001](../docs/evidence/stage-d/d-a11y-i18n-001-wcag22-aria-and-bilingual-conformance.md) / [D-GUI-CAMPAIGN-001](../docs/evidence/stage-d/d-gui-campaign-001-cross-platform-workstation-campaign.md))**:
   - `src/{i18n,a11y}/` 与 `src-tauri/src/a11y_i18n.rs`:`WCAG 2.2 AA` 20 组 `Graphite / Paper` 相对亮度对比度审计、`Z0–Z4` ARIA 语义树与双通道 live-region 播报、全键盘焦点环与 `Z4` 模态焦点陷阱、`prefers-reduced-motion` 全局静态闭合帧冻结及版本化 `zh-CN / en-US` 零行话/零 Emoji 双语包(`tests/a11y_i18n_conformance_side.rs`);`e2e/` 与 `scripts/gui-campaign/run_workstation_campaign.py` 闭合跨平台工业工作站五阶段交互战役、CJK 输入法组合态探针与 1,000 轮长稳 `O(visible_rows)` 零泄漏验证。
