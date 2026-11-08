# W82-C 受管 Worker 环境变量契约(托管约定 C)设计

- 车道:`W82-C-MANAGED-WORKER-ENV`(stage-e-progress.md §E.6.1.8 Phase 1 三轨并行之一)
- 写集:`crates/nlos-up/**`、`crates/nlos-package/**`、`crates/nlos-process/**`
- 状态:设计定稿(2026-11-08),维护者已批准按推荐默认决策执行(「按推荐的来」,2026-11-08)
- 验收门(规格原文):「按包清单拉起外部多进程组件并注入 `LLMOS_AUTH_ENDPOINT` / `LLMOS_PROCESS_IDENTITY_SEED` / `LLMOS_ROOT_CAPABILITY_SLOT`,主进程退出或吊销时子进程树干净回收」

## 1. 问题与现状断层(勘察实证)

外部多语言应用(如 qq-agent 的 `adapter` + `brain` 双进程 Python Agent)今天必须手写
`ManagedLlmosDaemon` 与三进程 supervisor。仓内断层:

| 断层 | 实证锚点 |
|---|---|
| 三个 `LLMOS_*` 变量全仓零出现 | `grep -rn LLMOS_AUTH_ENDPOINT|LLMOS_PROCESS_IDENTITY_SEED|LLMOS_ROOT_CAPABILITY_SLOT` 零命中;规格中变量名即全部规格(stage-e-progress.md:485、495) |
| 既有 env 注入是叠加继承,非白名单 | nlos-up `main.rs:530-534` `tokio::process::Command::new(npm).envs(child_envs)` 继承父环境全部变量 |
| 无清单拉起路径 | package.manifest(nlos-package `main.rs:636-765`)无解释器/argv/env/cwd 字段;其 `run` 是进程内载荷执行道,明示 no process spawn(`main.rs:1553-1554`) |
| Unix 无树回收原语 | nlos-process `platform_kill.rs:229-248` 仅单 pid SIGTERM,孙进程存活;全 crate 零 `killpg/setsid/process_group` 使用;工作区级 `unsafe_code = "forbid"`(根 Cargo.toml `[workspace.lints.rust]`) |
| `ProcessSupervisor` 生产代码零消费者 | nlos-up/nlos-package 均不依赖 nlos-process |

## 2. 已冻结决策(维护者 2026-11-08 批准,不再复议)

1. **受管 worker 清单用独立新清单文件**(本设计定名 `workers.manifest`),由
   nlos-package 解析、nlos-up 消费;**不扩展 package.manifest**——后者触
   nlos-artifact 规范类型与封存 N-1 包兼容,越出本车道写集。
2. **`LLMOS_ROOT_CAPABILITY_SLOT` 值为字节透传**:注入管道不硬编码语义,值来自
   清单单元的 `slot` 字段(缺省空串);格式契约留 TODO 对表 `w82-b-design`。
3. **令牌吊销→kill 接线留 W82-INT**(kill 执行器 ProcessAuthorityKillExecutor 在
   nlos-system-control = A/INT 写集);本车道只做回收原语与 kill-scope 事实记录,
   不自建吊销语义,避免与 ADR-0022 能力体系分叉。

## 3. 方案比较(选项与推荐)

### 3.1 清单格式

| 选项 | 结论 |
|---|---|
| (a) 沿用仓内行式 `key = value` 格式(`numbered_lines`/`scalar_line` 纪律,`#` 注释、空行忽略) | **采纳**。全工作区零 serde(根 Cargo.toml workspace.dependencies 无 serde 族),行式格式是 package.manifest / `*.devkey` 的既有纪律,零新依赖 |
| (b) TOML/JSON + serde | 否决:serde 将是全工作区首例,破坏格式纪律一致性 |
| (c) 新造结构化格式 | 否决:无先例、解析面更大 |

### 3.2 Unix 树回收原语(任务书要求列明选项并推荐;此处需 maintainer 表态项已随安全解消失,仅存档)

| 选项 | 结论 |
|---|---|
| (a) `std::os::unix::process::CommandExt::process_group(0)`(safe、stable 1.64,MSRV 1.97 无碍)+ `nix::sys::signal::killpg`(nix 0.31.3 既有 `signal` feature 已含) | **采纳**。零 unsafe、零新依赖、零外部 helper;pgid == 组长 pid == 注册 os_pid,代际栅栏语义不变 |
| (b) `pre_exec` setsid / PDEATHSIG | 否决:`pre_exec` 需 unsafe closure,撞工作区 `unsafe_code = "forbid"` |
| (c) 外部 helper 二进制做树杀 | 否决:新增移动部件与供应链面,收益为零 |
| (d) Windows JobObject 等价物 | 不采纳:Windows 侧 `taskkill /F /T` 已是树杀且被 LiveProcessTree 测试覆盖(`platform_kill.rs:1092-1434`);JobObject 绑定需 unsafe 或新增 windows-sys 依赖 |

结论:**不需要 unsafe 白名单,也不需要外部 helper**——选项 (a) 全 safe API 达成
「setsid+killpg 等价」语义。 supervisor 主进程被 SIGKILL 的瞬时孤儿窗口(无
PDEATHSIG 的固有限制)如实登记为已知边界(§8)。

### 3.3 解析器安放

| 选项 | 结论 |
|---|---|
| (a) nlos-package 新增 `[lib]`(src/lib.rs + managed_workers 模块),bin 不动 | **采纳**。任务拓扑「nlos-package 的清单解析」;bin-only 现状(scout 实证)使跨 crate 消费必须开 lib;main.rs 零改动,写集卫生 |
| (b) 解析器放 nlos-process | 否决:与冻结拓扑不符 |
| (c) nlos-up 自行解析 | 否决:解析无唯一权威家,双份格式定义会漂移 |

### 3.4 kill-scope 事实记录

| 选项 | 结论 |
|---|---|
| (a) `SupervisorPidEntry` 增加 `kill_scope: KillScope` 字段(`Process`/`ProcessGroup`);**`RegisterSupervisorPidRequest` 字段集冻结不动**,scope 经新增 `register_at_scope(request, kill_scope)` 参数传入(既有 `register(request)` 委托为 `KillScope::Process`,行为逐字节不变) | **采纳**(2026-11-08 实现期修订:勘察实证 12 处越集 `RegisterSupervisorPidRequest` 字面量——nlos-slice-k 生产 5 处 + 两 crate 测试 7 处,其中 nlos-system-control 为 W82-A 活跃写集;请求结构体加字段必然破坏越集编译,参数式注入零波及。越集 `SupervisorPidEntry` 字面量为零,entry 加字段安全)。scope 是注册期事实,单一真源;INT 后续接吊销→kill 时从同一栅栏取 scope |
| (b) supervisor 侧表记录 scope | 否决:与 registry 并行的第二真源,unregister/supersede 漂移面 |
| (c) kill 时隐式假设全部组杀 | 否决:破坏既有单进程 kill 语义与 88 个既有测试契约 |
| (d) 直接给 `SpawnSupervisedRequest`/`RegisterSupervisorPidRequest` 加字段 | **被写集约束否决**:`nlos-runtime-tokio/tests/kill_receipt.rs:524` 与上述 12 处越集字面量依赖两结构体字段集 |

**兼容性实证锚点**(scout + grep):`RegisterSupervisorPidRequest` 与 `SpawnSupervisedRequest`
的全部字面量构造点在 nlos-process 之外共 13 处(runtime-tokio 1 + slice-k/system-control
12),两结构体字段集冻结即零波及;`SupervisorPidEntry` 越集字面量为零,slice-k 仅对
`SupervisorPidDecision` 变体做绑定解构(加字段不破坏);`generation_pid_map()` 快照形状
不变(system-control/src/daemon.rs 消费面零漂移)。`ProcessSupervisor::kill` 在
`kill_scope == Process` 时走既有逐字节相同代码路径;`ProcessGroup` 才进入新分派。

## 4. 契约定义

### 4.1 `workers.manifest` 格式

行式格式,与 package.manifest 同纪律(`#` 注释与空行忽略;`key = value` 两侧
trim;首个 `=` 切分):

```
worker = <name> <interpreter> <argv...>
cwd  = <name> <dir>      # 可选;缺省 = 清单所在目录
slot = <name> <token>    # 可选;缺省 = 空串;字节透传,单 token(无空白);TODO 对表 w82-b-design
```

- `worker` 行:`name` 唯一、非空、无 NUL、`len <= 128` 字节(nlos-artifact
  `MAX_ENTRY_NAME_BYTES` 同量级,lib 内自定常量);`interpreter` 为绝对路径或
  裸名(裸名由 nlos-up 在父进程侧按父 PATH 解析为绝对路径后注入——子进程环境
  无 PATH,见 4.2);`argv` 为 `interpreter` 之后全部空白切分 token,至少
  1 个,原样透传(路径由被拉起解释器自行解析,不做 traversal 守卫——与
  package.manifest 的 entry 路径守卫不同,argv 是解释器参数而非启动器打开的路径)。
- `cwd` 行:`dir` 必须相对路径且过 `safe_join` 同型守卫(拒绝对路径、空、
  `..`/`.` 组件、NUL、超长),相对清单所在目录解析;引用未声明 worker 名 →
  报错;同一 worker 重复 `cwd` → 报错。
- `slot` 行:`token` 单 token;引用未声明 worker 名 → 报错;同一 worker 重复
  `slot` → 报错。
- 至少一个 `worker`;未知 key 报错;标量去重纪律沿用(parse_dev_manifest 同型)。
- 解析错误为一等错误类型(带行号),nlos-package lib 公开
  `parse_managed_worker_manifest(&str)` / `read_managed_worker_manifest(&Path)`
  与 `ManagedWorkerManifest`/`ManagedWorkerUnit` 类型。

qq-agent 双进程实例化示例(随测试夹具落盘):

```
# qq-agent 受管双进程示例
worker = adapter python3 adapter/main.py --port 7901
slot = adapter root-cap-placeholder
worker = brain python3 brain/main.py --mode resident
slot = brain root-cap-placeholder
```

### 4.2 三个 `LLMOS_*` 变量的语义与派生

子进程环境 = `env_clear()` 后**恰好**以下三个变量(顺序固定):

| 变量 | 值派生(nlos-up 装配) | 格式 |
|---|---|---|
| `LLMOS_AUTH_ENDPOINT` | 活守护进程 `auth_socket_path`(与既有 `LLMOS_DESKTOP_SOCKET` 同约定:Unix socket 路径字符串 / Windows 命名管道路径字符串) | UTF-8 路径串 |
| `LLMOS_PROCESS_IDENTITY_SEED` | 本会话 operator Ed25519 种子文件路径(`receipt.key_file`,即守护进程自身 `with_identity_key_file` 所用同一文件) | UTF-8 路径串(沿「传路径不传值」既有约定;传原值的替代案已评估,见 §8) |
| `LLMOS_ROOT_CAPABILITY_SLOT` | 清单单元 `slot` 字段,缺省空串 | 字节透传;格式契约 TODO 对表 w82-b-design |

推论与边界:

- **子进程无 `PATH`**。裸解释器名由 nlos-up 在父侧解析为绝对路径后传入
  `Command::new`(父侧 `which` 逻辑,手写 ~30 行,零新依赖);绝对路径原样使用。
  解释器解析失败 = 拉起失败,fail-closed。
- **不注入其他任何变量**(无 HOME/PATH/SYSTEM_ROOT/LLMOS_DESKTOP_*);既有
  desktop 链路(`.envs()` 叠加 + `augment_desktop_child_env`)完全不动,
  `bootstrap_smoke.rs:389-402` 的 dry-run env 断言保持绿。
- Windows 上若实机验证发现 Python 需 `SYSTEMROOT` 等,如实登记平台差异 receipt,
  不静默放宽白名单(本设计按严格三变量契约实现)。

### 4.3 拉起与生命周期协议(nlos-up `workers` 子命令)

```
nlos-up workers [--root <DIR>] --workers-manifest <PATH> [--grace-ms <N>]
```

- `--workers-manifest` 必填(文件路径;显式优于隐式);`--grace-ms` 缺省 3000。
- 流程:`bootstrap_trial_root` → 组装内嵌 SystemControlDaemon(复用 desktop 道)
  → 健康探测 → 双端点起服 → 解析 workers.manifest(nlos-package lib)→ 逐单元
  派生 `ManagedWorkerEnv`(三变量,单元间仅 SLOT 不同)→ `ProcessSupervisor`
  组拉起(Unix `process_group(0)`;kill_scope=ProcessGroup)→ 等待关停信号
  (Ctrl-C/SIGTERM)或全部单元退出 → 逐单元升级式树回收(4.4)→ 关守护进程。
- 机读输出行(沿 desktop 的 `READY/STOPPED` 协议):

```
WORKERS manifest=<path> units=<N>
WORKER name=<name> pid=<os_pid> status=spawned
READY service=system_control workers=<N>
KILL name=<name> outcome=<terminated|escalated|already-terminated> group_drained=<bool>
STOPPED service=system_control
```

- `ProcessId` 派生:SHA-256 域分离 `llmos/nlos-up/managed-worker-process-id/v1`
  ‖ 单元名 ‖ 清单路径字节 → 前 16 字节(仓内 `derive_16` 惯例);
  `Generation::INITIAL`。

### 4.4 回收协议(升级式,at-least-once)

每单元 teardown(nlos-process managed 模块提供原语,nlos-up 编排):

1. `ProcessSupervisor::kill(id, gen)`(代际栅栏解析后按 scope 发信号;Unix
   ProcessGroup scope = `killpg(pgid, SIGTERM)`,pgid == 注册 os_pid;Windows =
   既有 `taskkill /F /T`)。
2. 宽限轮询直子 `try_wait`(grace-ms);超时 → 强杀 `killpg(pgid, SIGKILL)`
   (Windows 单次 taskkill 已是强杀,无二级)。
3. 组排空核验:重复 `killpg(pgid, SIGTERM)` 至 ESRCH(对残余成员的重复
   SIGTERM 是幂等且处于拆除语境,无副作用面);ESRCH 即 `group_drained=true`。
4. 回执:每单元一条 `ManagedKillReceipt { process_id, generation, os_pid,
   outcome, escalated, group_drained }`,at-least-once 语义(重放安全,ESRCH 归一
   为 already-terminated)。

kill 链路改动面:`PlatformKillAdapter` 增加默认 fail-closed 的
`signal_platform_kill_group(id, gen, force: bool)` 方法(Stub/Noop 适配器继承
默认拒绝,零改动);Posix(unix)覆写为 `killpg(pgid, SIGTERM|SIGKILL)`,
Windows 覆写为委托既有 `taskkill /F /T`(本就树杀+强杀);
`ProcessSupervisor` 新增 `spawn_supervised_grouped`(unix 侧应用
`process_group(0)`,以 `KillScope::ProcessGroup` 注册)与 `kill_force`(升级
强杀);既有 `kill`/`spawn_supervised`/2 参 `request_platform_kill`/
`request_platform_kill_with_registry` 签名与 fence 语义不动(88 个既有测试与
runtime-tokio 越集测试零漂移)。

## 5. 架构与模块

```
nlos-package (新增 [lib])                nlos-process (新增 src/managed.rs)
  src/lib.rs ──┐                           ManagedWorkerEnv(3 变量名常量+值)
  src/managed_workers.rs                   prepare_managed_command(env_clear+3+cwd+process_group)
    parse/read/类型/错误                    KillScope + registry/adapter scope 分派
    (bin main.rs 零改动)                    terminate_group_with_escalation + ManagedKillReceipt
        │                                            ▲
        ▼                                            │ 依赖
     nlos-up (main.rs run_workers + lib 派生助手) ───┘
       新增依赖:nlos-package(lib)、nlos-process
       CLI 四点:USAGE / Command 枚举 / parse_cli_args / main dispatch
```

依赖边:nlos-up → nlos-package(lib)、nlos-up → nlos-process(两条均新增,
当前零反向依赖,scout 实证无环)。

## 6. 平台矩阵

| 面 | macOS(本机验证) | Windows(声明未实机) | CI |
|---|---|---|---|
| 清单解析 | 纯逻辑测试 | 同 | 全平台 |
| 命令构造(env_clear+3+cwd) | `Command::get_envs()`/`get_current_dir()` 断言 | 同 | 全平台 |
| 真实拉起+三变量断言+父环境不泄漏 | 实测(fixture dump 环境) | 同代码路径,`#[cfg(unix)]` 门控不在 Windows 跑 | macOS/Linux |
| Unix 树杀/孙进程/升级强杀/组排空 | 实测 | N/A(taskkill /T 既有覆盖) | macOS/Linux |
| e2e 主进程退出回收(SIGTERM nlos-up) | 实测 | 同路径未实机,声明 | macOS/Linux |
| Windows 差异 | — | taskkill /F /T 单级强杀、无 SIGTERM 宽限二级、命名管道端点值 | 既有 Windows 测试不回归 |

## 7. 测试计划(四类,先红后绿)

夹具风格沿仓内既有纪律(scout 实证):**自重执行**(`std::env::current_exe()` +
`#[ignore]` 子 worker 测试 + 角色环境变量)与 `sh -c`/`sleep` 受害进程为骨架——
仓内至今零 Rust 测试拉起 python/node,python3 可用性仅为 CI 假设;故核心夹具
用自重执行(可移植、零外部依赖),另加**一条 python3 门字面测试**直接对准
验收门「拉起外部 Python 组件并注入三变量」的原文(unix 门控;python3 缺席时
醒目失败而非静默跳过),Python 夹具按任务书置于本 crate `tests/fixtures/`。

1. **多进程清单拉起**:nlos-package 解析单测/集测(合法、注释、重复名、未知
   key、cwd/slot 引用未声明名、cwd traversal 拒绝、qq-agent 示例);nlos-up
   e2e 双单元拉起(自重执行 worker,ready 文件协议)。
2. **三变量注入断言**:worker 夹具把自身环境全量落盘;nlos-process 断言键集合
   恰为三变量且值匹配(env_clear 语义);nlos-up e2e 断言父环境哨兵变量(如
   `W82C_LEAK_SENTINEL`)不出现在子环境;python3 门字面测试同断言(`os.environ`
   全量恰三键)。
3. **Unix 树杀/孤儿回收**:夹具直子再 spawn 孙进程(`sh -c 'sleep … &'` 型);
   kill 升级链;`killpg` ESRCH 断言组排空;直子先亡孙进程残留场景同样被组杀
   覆盖。
4. **主进程退出触发回收 e2e**:真实拉起 `nlos-up workers` 二进制 + 夹具清单
   (自重执行 worker 含孙进程),就绪后 SIGTERM nlos-up,断言全部夹具进程
   (含孙)退出。

## 8. 已知边界与诚实声明

- **与 nlos-wasm-sandbox env 契约的表面差异**:wasm-sandbox 宿主进程契约是
  `.env_clear()` argv-only(不可信工具执行面),本车道受管 worker 是开发者清单
  声明的第一方组件(信任级同 package.manifest 作者),三变量注入正是规格
  明文 mandate(stage-e-progress.md:485/495);两契约各自封闭,不互通。
- **SIGKILL-of-supervisor 瞬时孤儿窗口**:主进程被 SIGKILL(非 SIGTERM)时,
  Unix 无 PDEATHSIG(unsafe 禁令)即无内核级父亡联动;进程组使幸存成员可被
  外部按稳定 pgid 事后清理(pgid == 注册 os_pid,回收信息不随 supervisor 亡失)。
  如实登记,不冒充覆盖。
- **`LLMOS_PROCESS_IDENTITY_SEED` 传路径不传值**:沿仓内既有身份约定
  (`LLMOS_DESKTOP_KEY_FILE`);传原值(种子字节直接入 env)为已评估替代案,
  交 INT/D 车道定契约时复议——注入管道字节透传,切换零结构改动。
- **令牌吊销→kill 联动留 W82-INT**(冻结决策 3);本车道交付 kill-scope 事实
  + 回收原语 + at-least-once 回执,验收门中「吊销时回收」一项显式标注未覆盖。
- **`slot` 值格式 TODO 对表 w82-b-design**(冻结决策 2),现为单 token 字节透传。
- **Windows 实机未核验**:全部 Windows 断言为声明级(同型代码路径 + 既有
  taskkill 测试);PR 正文按要求显式标注。
- **设计/计划文档落位 `docs/superpowers/{specs,plans}/`**:沿 W37-C 等既有车道
  先例;任务书写集枚举未含此路径,已在回执与 PR 显式标注此偏差供维护者裁定。

## 9. 提交与验收映射

| 验收门子句 | 交付物 |
|---|---|
| 按包清单拉起外部多进程组件 | workers.manifest 解析(nlos-package lib)+ `nlos-up workers` 子命令多单元拉起 |
| 注入且只注入三项 `LLMOS_*` | `ManagedWorkerEnv` + `prepare_managed_command`(env_clear+恰三变量)+ 注入断言测试 |
| 主进程退出时子进程树干净回收 | 进程组原语 + 升级式 teardown + e2e SIGTERM 测试 |
| 令牌吊销时回收(留 INT) | 显式未覆盖标注;kill-scope 事实记录供 INT 接线 |
