# NLOS Package SDK 开发者指南（`nlos-package` CLI）

> 状态：`DESIGN+IMPLEMENTED`（W33-A / B1-4；证据见 [B-ARTIFACT-007](../evidence/stage-b/b-artifact-007-package-sdk.md)；`install` 子命令 W35-P2 / 移交#2 前片，见 [B-APPLICATION-007 §7](../evidence/stage-b/b-application-007-third-party-sample.md)）
>
> 适用版本：`nlos/package-file/v1` · 依赖 `nlos-artifact` 签名验证面（B-ARTIFACT-003 / W28-B 模板段）

面向第三方 Application 开发者的打包工具链：从一个普通开发者目录（manifest + 载荷文件 + 可选 tasks 模板）构建出**一个自包含、已签名、可复现**的包文件，并用与内核同一条 `verify_package` 权威路径验签。本 CLI 不触任何内核内部 API。

## 1. 命令

```text
nlos-package keygen --seed <HEX64> [--out <KEYFILE>]        # 默认 KEYFILE = nlos-package.devkey
nlos-package build <DIR> --key <KEYFILE> [--out <PKGFILE>]  # 默认 PKGFILE = <package_id_hex>.v<version>.nlospkg
nlos-package verify <PKGFILE> [--store <DIR>] [--identity <DIR>] [--at-ms <U64>]
nlos-package conformance <PKGFILE>                          # W33-C 一致性检查器，见 package-conformance.md
nlos-package install <PKGFILE> --root <DIR>                 # W35-P2：verify → 安装权威（见 §6.1）
```

二进制位于 `crates/nlos-package/src/main.rs`（W35-P2 起独立 crate——install 需链接 `nlos-application` 安装权威，而后者依赖 `nlos-artifact`，原址会成依赖环；W33-A 时期根 Cargo.toml 禁改才落在 nlos-artifact 的 src/bin 下），`cargo build -p nlos-package` 后在 `target/debug/nlos-package`（或 `target/release/`）。

## 2. 开发者目录布局

```text
my-app/
├── package.manifest        # 必需：包声明（见 §3）
├── hello.bin               # entry 载荷：任意字节
├── assets/config.bin
└── tasks/                  # 可选：task 模板段引用的 digest 体文件
    ├── binding.txt
    ├── inputs.txt
    ├── outputs.txt
    ├── policy.txt
    └── ceiling.txt
```

规则：所有路径都是相对 `my-app/` 的普通相对路径——拒绝绝对路径、`..`/`.` 段、含空格（路径按空白分词）与 NUL。entry 名是包内唯一键（≤255 字节、非空、无 NUL、无重复）。

## 3. `package.manifest` 字段参考

行式格式：`#` 开头为注释，空行忽略；标量行 `key = value`；记录行 `entry = …` / `task = …`（可重复）。

| 行 | 形式 | 说明 |
|---|---|---|
| `package-id` | 32 位十六进制（16 字节） | 包身份，全局自选；建议 `openssl rand -hex 16` |
| `version` | `u64` 十进制 **或** `major.minor.patch` | 点分三元组按 `nlos_application::pack_package_version` 同一公式打包（major<<32 \| minor<<16 \| patch），与安装/更新兼容窗判定一致 |
| `entry` | `entry = <name> <role> <path>` | role ∈ `executable` \| `background-service` \| `data`；path 为载荷文件，内容摘要进签名 |
| `task`（可选） | `task = <node-key-HEX32> <kind> <binding> <inputs> <outputs> <policy> <ceiling> [deps=<HEX32,…>]` | ADR-0016 决定 1 的 manifest `tasks` 模板段（W28-B 面）；kind ∈ `agent-role` \| `executable` |

`task` 行的五个文件字段各自取**文件内容**作为对应声明摘要：`binding_digest` / `input_selectors_digest` / `output_contract_digest` / `policy_digest` / `resource_ceiling_digest`。`node-key` 是段内唯一声明身份（16 字节）；`deps` 引用同段内已声明的 key（无自依赖；环检测由 plan authority 独占，`[PLAN-OVERRIDE-001]`）。段非空才启用模板签名面——无 `task` 行的包走 legacy 签名面，两者域分隔、互不可移植。

## 4. keygen 与签名密钥

```text
$ openssl rand -hex 32                      # 开发者自行提供熵
$ nlos-package keygen --seed <HEX64>
KEYGEN nlos-package.devkey
principal ca53c1cc7ddd50baaea1460b67787c47
public_key 4e086e9cb9d6ae11fcd0571ca1cd335877bc4e87960500a8599b4bd0cf739ac6
```

密钥文件（unix 下 0600）只含种子与可公开的 bootstrap 描述子；principal id 不落文件——每次由 `nlos-identity` 权威从描述子确定性派生（单一派生权威）。**`--seed` 必填**：工具链刻意不内置随机源（零新增依赖），熵由开发者供给。

**保管边界（如实声明）**：`keygen` 产出的是开发便利密钥。`verify` 会用包文件内嵌的公钥描述子 bootstrap 签名者并验签——这证明「签名与该密钥一致」，**不是**信任决策。生产签名密钥的托管、trust root、签名链与多签策略是部署侧关注点（`[B-IDENTITY-003]` custody 线），本 CLI 一概不承担。

## 5. build：可复现构建

```text
$ nlos-package build my-app --key nlos-package.devkey --out sample.nlospkg
BUILT sample.nlospkg
package 0f1e2d3c4b5a69788796a5b4c3d2e1f0 version 4295229442
manifest_digest a3db0ba7…
signer ca53c1cc…
entries 2 tasks 2
```

构建无任何墙钟输入：entry 的 `artifact_id` 由 `SHA-256("llmos/package-file/artifact-id/v1" ‖ package_id ‖ version ‖ name)` 前 16 字节确定性派生，载荷原样内嵌。同一目录 + 同一密钥 ⇒ **逐字节相同**的包文件与相同 manifest 摘要（有测试钉死）。

包文件是规范长度前缀帧（u64 BE），布局：`magic "nlos/package-file/v1"` → 签名者公钥描述子（公钥/两个策略摘要/bootstrap 幂等键/有效期窗）→ package_id/version → entry（名/角色/artifact_id/digest/载荷）→ task 模板段 → 64 字节 Ed25519 签名。签名消息即 `nlos-artifact` 的域分隔 manifest 摘要（legacy 面或 with-tasks 面，由是否存在 task 行决定），两面域分隔使剥段/注段篡改必然验签失败。

## 6. verify：与内核同一条验签路径

```text
$ nlos-package verify sample.nlospkg --store store.d --identity identity.d
VERIFIED a7ea5992540089137d455035715a495e
manifest_digest a3db0ba7…
package 0f1e… version 4295229442
signer ca53… key 2056…
```

流程：解析包文件 → 把每个 entry 物化为真实 `ArtifactStore` 内容（create + put revision，均幂等）→ 在 `IdentityAuthority` 里 bootstrap（或 replay）签名者 → 调 `ArtifactStore::verify_package` / `verify_package_with_tasks`：manifest 形状 → 幂等 replay → 当前 key binding 验签 → 逐条内容绑定 → 落 immutable receipt。`--store`/`--identity` 缺省用一次性临时目录（验完即删）；显式给目录则持久化，且验签幂等键从 manifest 摘要确定性派生——同包重验输出 `REPLAYED` 与首个 receipt 逐字节相同（durable receipt 是权威，密钥事后吊销不影响 replay）。

**后续安装链**：receipt id（`VERIFIED` 行的 32 位十六进制）即 `nlos_application::ApplicationAuthority::install_application` 的 `package_verification_receipt_id` 入参——验签与安装由 receipt digest-binding 衔接（全周期演示见 W33-B 样板应用车道；W35-P2 起 `install` 子命令直接走通，见 §6.1）。

### 6.1. install：从 CLI 走通安装权威（W35-P2）

```text
$ nlos-package install sample.nlospkg --root state.d
VERIFIED a7ea5992540089137d455035715a495e
manifest_digest a3db0ba7…
package 0f1e… version 4295229442
signer ca53… key 2056…
INSTALL 185f9e2228c1f5bead58a0d7840132b3
decision installed
application f15f… package 0f1e… generation 1 version 4295229442 entries 2 installer ca53…
executables hello
```

流程：解析包文件 → 在 `--root`（必填）上打开 slice-k 装配的权威集（identity/process/artifacts/applications/tasks/clock/operations，子路径布局与 `sample-app-driver` 同一落点）→ bootstrap 签名者 → 逐 entry 物化 → 权威 verify 管线（`VERIFIED`/`REPLAYED`，与 §6 同一条路径）→ W22-001 install-scoped 孤儿 GC → `install_application`（receipt digest-binding、单事务 CAS 推进代际）。

幂等纪律：verify 幂等键由 manifest digest 派生（同 §6），GC/安装幂等键与时钟键由 verification receipt id 域分隔派生——**同包重装逐回执 replay**（`decision replayed`，代际不推进），**不同包装进同一 root 互不冲突**（各自的 receipt 派生各自的键）。安装后 `--root` 即可被 slice-k 运行时/载荷执行车道重新打开（`SliceKRuntime::open`）。

### 退出码

| 码 | 含义 |
|---|---|
| 0 | 成功（verify 输出 VERIFIED/REPLAYED；conformance 输出 CONFORMANT；install 输出 INSTALL + decision） |
| 1 | 用法错误 |
| 2 | 输入畸形：manifest/密钥文件/包文件解析或形状（重复 entry 名、悬空依赖、截断包…）；conformance 亦用于文件不可读 |
| 3 | 验签/身份失败：签名不符、principal 未知、密钥吊销、幂等冲突 |
| 4 | 内容绑定失败：载荷与声明摘要不符（篡改载荷） |
| 5 | 内部 I/O 或存储失败 |
| 6 | conformance 发现违规（`PKG-CONF-###`，规则表见 [package-conformance.md](package-conformance.md)） |
| 7 | 安装权威拒绝（`install`：receipt 未知/终态冲突/幂等冲突/时序倒置） |

### 篡改语义（负门）

- 改**载荷**字节 ⇒ 物化后的 head digest ≠ 声明 ⇒ `PackageTampered`（exit 4）；
- 改**任何被签名字段**（version、entry 名/摘要、task 段…）⇒ 签名失效 ⇒ exit 3；
- 剥/注 task 段 ⇒ 两面域分隔 ⇒ exit 3（W28-B G6 纪律）。

## 7. 已知限制

- 单签名者、无 trust root/签名链/多签；`KeyPurpose` 沿用 `SemanticSigning`（B-ARTIFACT-003 已登记的 identity 侧后续切片）。
- `verify` 的自包含物化是**开发者路径**：内核生产摄取走系统侧 store/identity 部署，本 CLI 不覆盖。`install` 同理把 `--root` 当作单节点状态根（dev 形态），不做多 principal 审批。
- manifest 是 §23.2 最小子集 + tasks 段；applications/imports/exports/resources/lifecycle/security 等字段未建模。
- 版本点分三元组的打包公式镜像自 `nlos-application`（两 crate 依赖方向所限无法复用函数）；`nlos-application` 侧公式演进时此文档与 CLI 需同步。
- `install` 只做安装：运行/更新/卸载的消费端 CLI（`run`/`update`/`uninstall` 子命令）仍是后续车道；已安装应用 executable 载荷的执行经 `nlos-slice-k::execute_application_payload` 内核车道（W35-P2 前片）。

## 8. 相关

- [B-ARTIFACT-003](../evidence/stage-b/b-artifact-003-package-signature.md)：签名验证最小前缀与失败语义
- [B-PLAN-002](../evidence/stage-b/b-plan-002-manifest-template.md)：tasks 模板段与 G6/`[PLAN-OVERRIDE-001]`
- [B-ARTIFACT-007](../evidence/stage-b/b-artifact-007-package-sdk.md)：本 CLI 的证据文件（§6 为 W33-C conformance kit）
- [package-conformance.md](package-conformance.md)：W33-C 包一致性检查器与 `PKG-CONF-###` 规则表
- [B-APPLICATION-001](../evidence/stage-b/b-application-001-installation-authority.md)：receipt → 安装权威
