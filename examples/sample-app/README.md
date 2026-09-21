# sample-app — 第三方样板应用（W33-B / B1-5）

一个完整的第三方应用样例，证明不触任何内核内部 API 的开发者可以只骑
公共面（`nlos-package` CLI + 各权威 crate 的公共 API + `nlos-slice-k`
装配）走完整个生命周期：

```text
开发（manifest + 载荷 + tasks 模板）
  → keygen → build → verify（签名 + 权威验签 receipt）
  → install（receipt digest-binding → ApplicationAuthority）
  → run（Task/Attempt/Process → fiber driver operation → Artifact + 回执）
  → update（同 major 新 revision → W29-E 迁移 runner 原子切换）
  → uninstall（W27-D 活动门 → W30-D teardown 链 → 过门卸载）
```

证据文件：`docs/evidence/stage-b/b-application-007-third-party-sample.md`
（ROAD-B-001 的 constructive exit 证据）。开发者打包指南：
`docs/developers/packaging.md`。

## 一键演练

```sh
sh examples/sample-app/lifecycle.sh
```

脚本自带退出码与输出断言（任何一步失败即非零退出）：构建
`nlos-package` 与 `sample-app-driver`（若缺）、然后依次执行上面九步，
并在最后做两个负门断言——平台 kill 后 Os 服务替身进程必须真的消失；
卸载之后 `run` 必须 typed 拒绝。

机械化的无头全链测试（CI 可跑，同一链条的进程内孪生）：
`cargo test -p nlos-slice-k --test third_party_sample_lifecycle`。

## 目录

```text
sample-app/
├── package/            # 开发者目录 v1.0.0（manifest + 载荷 + tasks/ 模板体）
├── package-v1.1.0/     # 同一目录在版本演进后的快照（同 package-id，版本 1.1.0）
├── src/main.rs         # sample-app-driver：生命周期驱动（只 import 公共 API）
├── lifecycle.sh        # 全生命周期演练脚本（真实 CLI + 真实子进程 kill 链）
└── Cargo.toml          # 独立 workspace 根（desktop/ 先例；不入仓库 workspace）
```

`package.manifest` 的 `tasks` 段演练 W28-B 模板面：两个模板（`agent-role`
主节点 + 依赖它的 `executable` 服务节点），五个 digest 体文件在
`tasks/` 下；构建走 with-tasks 域分隔签名面，验证走
`verify_package_with_tasks`。

## 分步命令

（`<repo>` 为仓库根；`STATE` 为任一空目录，权威们会在其下落库。）

```sh
# 0. 工具链
cargo build -p nlos-package                                     # → target/debug/nlos-package
cargo build                                            # 本目录 → target/debug/sample-app-driver

# 1. 密钥（熵由开发者供给；样例种子见 lifecycle.sh，勿用于生产）
openssl rand -hex 32
nlos-package keygen --seed <HEX64> --out sample-app.devkey

# 2-3. 构建 + 权威验签（--store/--identity 指向 STATE 下的权威目录，
#      与 sample-app-driver 打开的 SliceKRuntime 同一落点）
nlos-package build package --key sample-app.devkey --out app-v1.nlospkg
nlos-package verify app-v1.nlospkg --store $STATE/artifacts --identity $STATE/identity
# → VERIFIED <receipt-id-HEX32>

# 4. 安装（驱动只拿 receipt id；内部走 install_application 的
#    verify-then-commit 七项 digest 绑定）
sample-app-driver install $STATE <receipt-id>

# 5. 运行（Task/Attempt 行携带 application 关联；delegated Process；
#    CommitPermit；fiber 的 durable driver operation + 应用自有 artifact
#    的 stage/plan/converge；注册 background task 与 process binding；
#    Unix 下以真实子进程充当 background-service 的 Os 替身）
sample-app-driver run $STATE <package-id>
# → 输出 service_os_pid=<pid>（卸载阶段经真实 POSIX kill 链杀死它）

# 6-7. 更新（同 major 1.1.0：verify → 迁移 runner begin → 2 步记录 →
#    健康检查（probe 恰一次）→ 单事务原子切换，代际 1→2）
nlos-package build package-v1.1.0 --key sample-app.devkey --out app-v2.nlospkg
nlos-package verify app-v2.nlospkg --store $STATE/artifacts --identity $STATE/identity
sample-app-driver update $STATE <package-id> <receipt-id-v2>

# 8. 卸载（先见证 W27-D 活动门对未收敛后台 Task 的 typed 拒绝，再走
#    W30-D teardown 链：platform kill → crash terminal → linkage →
#    cancel_task → 过门卸载）
sample-app-driver uninstall $STATE <package-id> <service_os_pid>
```

## 诚实范围

- 载荷执行面（把 `executable` entry 的字节真正跑起来）是后续车道：
  样例的 run 阶段驱动的是公共任务/操作/纤维面，载荷以 artifact 形态
  可消费（verify 时已物化进 artifact store）。
- `background-service` 的 Os 侧替身是 `sleep 600` 真实子进程（slice-k
  测试同款纪律），经 supervisor registry 注册、被真实 SIGTERM 杀死；
  它不是内核托管的服务进程。
- 信任边界沿用 `nlos-package` 的声明：verify 证明「签名与密钥一致」，
  不是信任决策；生产密钥托管/trust root 是部署侧关注点。
- 密钥/时间戳纪律：slice-k 复用助手按 seed 带；驱动自有键走
  `llmos/sample-app/driver-key/v1` 域分隔派生（见 `src/main.rs` 注释）。
