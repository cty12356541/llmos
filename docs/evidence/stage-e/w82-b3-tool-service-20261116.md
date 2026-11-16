
# W82-B3 沙箱工具服务与持久 Operation 验证记录

证据 ID `w82-b3-tool-service-20261116` · W82-B3 / Attempt 20261116-2 · 2026-11-16 · `H4`

Windows 初版、取消修复及恢复同步通过独立审查。终审发现跨实例配额问题，修复已通过本机验证、增量独立复审及受影响主树复验，原生三平台门待收。下列实测注明对应版本。主树复验和三平台验证见[汇总门](../../management/stage-e-progress.md#2026-11-16-三服务库验证)。 接单树为 `cdb0d10d0a68c7d23b04081bc0031942cbb2b893`。[补丁回执](artifacts/w82-services-20261116/integration-inputs.json) · [集成源码清单](artifacts/w82-services-20261116/services-source-manifest.json) · [制品摘要](artifacts/w82-services-20261116/sha256-manifest.json)

## 运行边界

- `tool_sandbox` 消费冻结的 `list_tools` 和 `invoke_tool` codec。每个具名入口通过注入的当前受信时钟和共享 `authorize_call` 校验调用。运行时绑定和 wire caller 分开验证，Task 绑定拒绝，注册表给出实际 Wasm 摘要、命名空间、包代际和固定入口。受信子进程在 Wasmtime 装载前核对自己有界读取的同一批字节。缺少当前权利时，服务不返回受保护的重放结果。
- 目录入口为 `__nlos_tool_catalog__`，使用空预打开配置。按实际 Wasm SHA-256 缓存，每个注册项保留一个已解析目录。输入为单个 JSON argv，限制 8,192 UTF-8 字节和深度 64。目录上限 128 项，名称 128 字节、摘要 1,024 字节、单 schema 16 KiB。变更描述必须声明 `reconcile=same-idempotency-key`，补偿类必须提供处理器。目录和执行结果采用 canonical schema 并在完整编码后独立应用 65,536 字节上限。
- 每次执行使用独立宿主进程和现有 14 项 WASI 导入，子进程环境为空且标准输入为空。stdout 流界 256 KiB，guest stderr 流界 60 KiB，内部报告预留 4 KiB。外层帧标签区分 guest 诊断和可信报告。标准库创建的非 inheritable 描述符遵守平台默认，宿主集成仍须避免标记敏感句柄为可继承，并控制注册产物、预打开根和 journal 目录。
- 默认 1,000,000 fuel、单个线性内存 16 页（1 MiB）、30 秒 profile，取请求剩余时间与 profile 上界的较小值。宿主内部 epoch 定时器独立于服务进程。额外进程内 watchdog 覆盖引擎编译和同步宿主 I/O，在其收到的 wall 上界后有 250 ms 退出宽限。初版按实例分配活宿主额度。共享 writer 的配额修复及其默认 16/4 和拒绝语义见末节。
- 文件写入在 WASI 宿主落盘前按调用累计，最大 8 MiB。工具路径拒绝可写描述符 seek 和 truncate，并限制每次 4,096 个创建尝试，控制空文件元数据增长。目录逐组件 no-follow，文件由相对受信目录句柄打开，拒链接和非单链接常规文件。已有工作区总磁盘配额归后续工作。
- 默认 journal 只允许一个 writer。单记录最大 128 KiB，admission 水位为 192 MiB / 196,608 条，硬上限为 256 MiB / 262,144 条，逐未决操作预留记录及最坏终态字节。原始主体、应用、命名空间、注册项和 128 位键合并作查重索引，过程、代际和会话用于控制授权。追加记录串联 SHA-256 校验和与序号，回执发出前完成 `sync_all`。重开只修复合法不完整尾帧，已持久 dispatch 的未决项保留为 `EffectUnknown`。完成与取消按 receipt revision 比较并提交一次。目录观测最多 4,096 项 / 128 MiB，前后一致仍保留 `EffectUnknown`。

新增信任面包括工具注册表、进程管理、持久 journal、预打开的根解析和 host 可信报告。受信宿主提供当前身份、时钟、授权和包来源。沿用已有 14 项 WASI 导入，宿主磁盘总量控制及系统级进程树联动仍归集成。

## Windows 实跑

Windows 11 Pro，10.0.28000，x86_64-pc-windows-msvc，AMD Ryzen 5 9600X，rustc 1.99.0 `b940084d7`，Cargo 1.99.0，Wasmtime 50.0.1，debug profile。以下命令使用短构建目标目录 `D:/llmos/target/w82-b3`。

- `cargo test --locked -p nlos-wasm-sandbox` 退出 0。44 项单元测试、15 项逃逸矩阵、6 项 journal、1 项双管道和 18 项服务测试通过。后续仅将测试中的链接夹具分开标记，以单独的矩阵命令复验。
- `cargo test --locked -p nlos-wasm-sandbox --test escape_matrix -- --nocapture` 退出 0，16 项由测试框架标为通过。真实 hard-link 和 NTFS junction 子例分别打印 `escape_preopen_leaf=hard_link` 和 `escape_preopen_parent=ntfs_junction`。Windows 的文件 symlink 测试 `writable_grant_refuses_a_symbolic_link_as_file` 在创建夹具时命中系统权限错误 1314，打印 `escape_preopen_leaf=windows_symlink_unavailable os_error=1314` 后结束，待有相应权限的平台实证。
- `cargo clippy --locked -p nlos-wasm-sandbox --all-targets -- -D warnings`、`cargo fmt -p nlos-wasm-sandbox -- --check`、`git diff --check` 均退出 0。隔离树上的 `cargo fmt --all -- --check` 退出 1，Windows 启动 rustfmt 命令触发 OS error 206，全工作区格式检查由 integrator 在短根路径执行。
- 根和 ROAD-E-001 升级门的完整 `cargo metadata --locked --offline --format-version 1 --filter-platform x86_64-pc-windows-msvc` 均退出 0。锁语义比较仅新增沙箱 crate 到既有 `cap-std` / `cap-fs-ext` 4.0.3 的两条边，各 registry 包版本和校验和不变。锁文件供应链 conformance 3/3 通过。[本机初版包测试](artifacts/w82-services-20261116/b3-baseline-suite.txt) · [逃逸夹具结果](artifacts/w82-services-20261116/b3-win-escape.txt) · [双管道反例](artifacts/w82-services-20261116/b3-pipes-red.txt)

### 竞争、资源与崩溃用例

- `cancel_before_durable_dispatch_prevents_host_from_running` 在授权屏障处停住已登记操作，确认 durable cancel 后零 invocation host 执行。 `running_cancel_kills_real_host_and_preserves_uncertain_effect` 等待真实 guest 写入启动见证，争用同一个 key 并取得同一个 operation，要求取消到达和 kill/reap，最终回执保持 `EffectUnknown`。完成后的取消和晚到回调均保留既有终态。
- `orphan_host_terminates_after_service_harness_is_killed` 通过操作系统确认小型服务测试进程及其真实 host PID，硬杀服务测试进程后仍观察到活着的 host，随后在 epoch / watchdog 窗口内终止，重开 journal 得到 HostLost 墓碑。同 key 复验保持原状态。独立的 `real_tool_epoch_interrupts_independently_of_parent_deadline_and_watchdog` 在 300 ms epoch、3,000 ms watchdog 配置下得到 133 与 `epoch-interrupt`，验证 epoch 有独立终止路径。
- `drains_both_child_pipes_while_waiting` 使用真实 stdout/stderr 管道，各自输出约 1.64 MiB，基线在 3.01 秒等待后误触超时，修复后通过。 `independent_stdout_stderr_wire_and_write_boundaries` 证明 256 KiB / 60 KiB / 8 MiB 的独立语义， `complete_result_encoding_obeys_exact_wire_boundary` 实测编码 65,536 字节成功、增加一字节拒绝。 `complete_catalog_encoding_rejects_an_oversize_directory` 和 `framed_guest_stderr_does_not_replace_the_host_report` 分别覆盖完整目录界和报告隔离。
- `subprocess_loss_recovers_at_each_durable_transaction_boundary` 依次硬杀真实 writer harness，覆盖 begin 前、登记后、dispatch 后、cancel 后和完成后。 `torn_tail_can_be_repaired_but_middle_corruption_and_bad_sequences_fail_closed` 组合受控尾部截断、历史内容损坏和序列断裂。 `os_append_failure_prevents_receipt_and_poisons_this_writer` 使用实际 OS 写拒绝触发 fail-closed。单独同步故障的补验见下文。真实电源故障和 Windows 新建目录项的断电持久性仍待检验。
- `actual_load_refuses_artifact_replaced_after_authorization_hash` 在授权摘要采样后、子进程读取前确定性替换文件。程序以 ArtifactChanged 结束，替换版本未运行。 `preopen_parent_replacement_does_not_retarget_the_grant` 在已登记根的父目录换成 junction 后验证 host 拒绝； `writable_grant_refuses_a_hard_link_as_file` 验证已开对象的链接数防线。

## 实测延迟

单独命令 `cargo test --locked -p nlos-wasm-sandbox --test tool_service measured_spawn_and_run -- --exact --nocapture` 记录温目录下的小型 WAT 工具，argv `{}`，输出 `42`。分位涵盖授权、spawn+run 和写入持久结果的完整调用，使用默认 profile，按最近秩取值。

- Windows x86_64 本机 20 样本，p50 22.59 ms，p95 30.81 ms。
- Linux 原生执行待集成 CI。
- macOS 原生执行待集成 CI。
- Windows 主树初次集成 20 样本 p50 23.63 ms、p95 33.13 ms。helper 因 Windows 真实 symlink 未覆盖退出 1，不能沿用测试框架通过计数充当该夹具的 PASS。[初次集成实测](artifacts/w82-services-20261116/initial-native-helper-output.txt)

p50 或 p95 超过 50 ms 时用于评估未来常驻宿主，继续记录实际样本。[Windows 修复后采样](artifacts/w82-services-20261116/b3-win-latency.txt)

## 待接线的工程门

OperationControl、WaitControl、SQLite Operation 适配、生产 daemon 内的孤儿回收、经验证的 Blob 输入 materializer、SDK 和真实外部消费者接线归 INT / D。B3 提供已授权只读预打开输入边界和本地 journal，外部身份、原生平台过程树管理与 workspace 归属仍由可信集成宿主提供。Stage 退出待维护者验收，候选尚未提交或推送。

## 独立审查 P1 修复 / Fixround 1

跨实例共享单一 journal writer 时，执行者在最终 spawn 授权前检查 durable dispatch fence，并在运行期间以 20 ms 间隔观察自身的持久状态。读取时释放 child 锁。另一实例持久化 `CancelRequested` 后，执行者终止并回收真实 host，再按既有 CAS 合并唯一终态。取消调用返回的 `CancelRequested` 是持久受理结果。观察失败会停止 host 并返回存储错误，存储仍可写时保留 `EffectUnknown(HostLost)`。20 ms 是正常读取返回后的观察间隔，实际回收时间受存储查询和系统调度影响。

初版原始候选、独立审查和初版平台实测继续保留。Fixround 1 产品源集合清单 SHA-256 为 `19dacf20d019300ea26cfff1d7eea8a6abac82d820d24a2f67312eb81f4f2758`。

- Windows 上运行 `cargo test --locked -p nlos-wasm-sandbox --test tool_service shared_writer_cancellation_reaches_the_executing_instance -- --exact --nocapture`。修复前退出 101，等待 500 ms 超时，由原执行者清理同一 host。修复后退出 0，一次实测从跨实例取消到回收和持久终态为 13 ms。stale generation 被拒，同键重放保留终态且无第二次启动。
- `cargo test --locked -p nlos-wasm-sandbox --test tool_service --test tool_journal` 退出 0，服务 21/21、journal 6/6。新增确定性完成屏障覆盖取消先提交及完成先提交，另以真实 host 验证存储观察失败返回 `Durability` 并留下 HostLost 墓碑。
- 严格 Clippy、crate 格式和 diff 检查均退出 0。20 个相同配置的 Windows 样本复测 p50 23.99 ms、p95 32.42 ms。修复通过独立复审。[修复前](artifacts/w82-services-20261116/b3-cancel-red.txt) · [修复后](artifacts/w82-services-20261116/b3-cancel-green.txt) · [服务与 journal 测试](artifacts/w82-services-20261116/b3-service-journal.txt) · [独立复审](artifacts/w82-services-20261116/b3-cancellation-review.txt)


## Journal 同步故障验收 / W82-B3-DURABILITY

独立审查已通过的取消修复保持原样。本轮补齐共享前置验收的同步故障证据，产品源集合清单 SHA-256 为 `19775384cd687993461a27e6085cb3208baeb031f0be0963312ee606c67c0ae9`。

- `reopen_requires_sync_before_publishing_retained_completion` 以真实 journal 文件写出完成帧，再由单 writer 的私有测试缝让同步落在无写权限的 Windows 文件句柄上。实际 `File::sync_all` 返回 `PermissionDenied` / OS 5，文件长度增加表明确实已写。原 writer 拒绝成功回执、查询与重试，进入 poisoned 状态。
- 修复前，用同一故障打开带完整终态帧的 journal 可发布 writer，具名用例退出 101。修复后，恢复校验并同步有效前缀之后才发布 writer。新同步失败时返回 `Durability`，具名用例退出 0。随后正常同步成功，恢复同一操作的完整 wire，异摘要冲突，同键不会再次取得 dispatch。
- `lost_completion_after_sync_failure_keeps_dispatch_indeterminate` 通过裁去未得到同步确认的完成帧，模拟只保留先前已同步 dispatch 的结果。重开得到 `EffectUnknown(HostLost)`，保留原操作键与观测摘要，禁止重派。 `checksum_intact_duplicate_journal_frame_is_rejected` 把同一帧的原字节再次写入，重开返回 `Corrupt`。
- Windows 的 `cargo test --locked -p nlos-wasm-sandbox --lib --test tool_journal` 退出 0，46 项单元测试及 7 项 journal 集成测试通过。具名真实服务重开重放回归、严格 Clippy、crate 格式和 diff 检查退出 0。Windows 用实际文件同步错误取证，Unix 夹具使用匿名管道读取句柄触发实际同步错误，Linux 和 macOS 原生结果待 CI。完成帧丢失由测试构造，断电和 Windows 新建目录项的断电持久性仍待验证。

追加同步门通过独立审查。[恢复缺少同步的反例](artifacts/w82-services-20261116/b3-sync-red.txt) · [恢复同步用例](artifacts/w82-services-20261116/b3-sync-green.txt) · [补验测试](artifacts/w82-services-20261116/b3-durability-suite.txt) · [独立审核](artifacts/w82-services-20261116/b3-durability-review.txt)

## 同一 authority 的共享宿主配额

首轮终审用两个服务实例共用真实 journal writer，复现活宿主额度被分别计数。修复的 `ToolOperationStore::host_quota() -> Arc<ToolHostQuota>` 由可信 adapter 提供。同一 authority 的所有适配器必须转发同一个 Arc，默认 journal writer 自身拥有配额。冷目录导出与 invoke 消费同一全局及每主体计数。首个成功构造固定 global/per-principal 限额元组，构造校验失败不绑定。匹配的配置可共享，后续冲突返回 `ConflictingHostLimits`。即使相关服务都已销毁，绑定仍持续到该 writer 生命周期结束。

默认全局 16、每主体 4。额度用完返回 `NoHostPermit`，拒绝时不新增调用的 journal 记录。许可随宿主执行、回收及当前结果处理的作用域释放，归零后移除主体计数。TCB 包括可信运行时与 adapter 的配额转发责任，计数范围限同进程共享 writer 生命周期。跨进程物理配额与崩溃后的孤儿宿主窗口仍各有边界。

两个真实宿主回归修复前共退出 101。全局限额 1 的用例使用不同主体，每主体限额 1 的用例使用同一主体。两个红例均观察到第二个 guest 写出标记，随后回收持续运行的首个宿主。修复后退出 0，两个用例的第二个标记均未出现。冷目录与 invoke 均受额度约束，回收后原键可正常执行，其他主体可继续使用尚有额度。另验证冷目录失败释放、收紧和放宽配置均被拒绝，以及首次配置跟随 writer 存续。[故障复现](artifacts/w82-services-20261116/b3-quota-red.txt) · [修复回归](artifacts/w82-services-20261116/b3-quota-green.txt)

Windows 隔离树 `cargo test --locked -p nlos-wasm-sandbox` 退出 0，共 95 项（46 库单测、16 逃逸矩阵框架通过、7 journal、1 双管道、25 服务）。Windows symlink 权限 1314 未覆盖沿用此前记录。随后仅改动新测试变量名，`cargo test --locked -p nlos-wasm-sandbox --test tool_service` 最终 25/25。all-targets Clippy（`-D warnings`）、crate fmt 及差异检查均退出 0。该记录属于隔离修复版本。独立复审确认原问题已关闭。主树后修复验证见下段。[增量独立复审](artifacts/w82-services-20261116/b3-quota-independent-review.txt)[全包结果](artifacts/w82-services-20261116/b3-quota-crate.txt) · [最终服务套件](artifacts/w82-services-20261116/b3-quota-service.txt) · [Clippy](artifacts/w82-services-20261116/b3-quota-clippy.txt) · [三路径摘要](artifacts/w82-services-20261116/b3-quota-source-delta.json) · [隔离源码清单](artifacts/w82-services-20261116/b3-quota-source-set.txt)

最终 Windows 主树重跑沙箱包，95 项通过，新增的两个配额回归单独重跑 2/2。两个配额场景均无第二 guest 标记。默认 profile 的 20 个样本为 p50 23.81 ms、p95 48.77 ms，覆盖 invoke、spawn、run 和持久结果。严格 Clippy、Rust 1.97.1 all-targets check 及全工作区 fmt 均退出 0。初验的远端原生缺口仍在，包括 Windows symlink 1314 与 Linux/macOS 专项。修复后的 95 项单独绑定最后 45 路径源快照。[主树测试](artifacts/w82-services-20261116/final-main-test-output.txt) · [具名配额回归](artifacts/w82-services-20261116/final-main-quota-output.txt) · [延迟日志](artifacts/w82-services-20261116/final-main-latency-output.txt) · [命令回执](artifacts/w82-services-20261116/final-main-command-receipt.json)
