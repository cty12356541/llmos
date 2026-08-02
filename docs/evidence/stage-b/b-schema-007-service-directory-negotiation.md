# B-SCHEMA-007：ServiceDirectory 协议与确定性协商内核初始证据

> 状态：PARTIAL PASS（三平台 CI 待本提交推送后复验）
>
> 日期：2026-08-02
>
> 对应：`SABI-DISC-001`、`COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`

## 1. 本切片解决的问题

B-SCHEMA-005/006 的 IPC client 仍由调用者直接传入 socket/pipe endpoint。本切片开始实现 v0.5 的 `ServiceDirectory + version/feature negotiation`，但有意先把协议和确定性选择内核做成独立可验收单元：

```text
trusted Namespace/bootstrap handle
  → resolve(service)
      → transport-neutral candidates（没有 endpoint address）
  → negotiate(schema major/minor, required features, supported transports)
      → one compatible ServiceBinding + LocalEndpoint
```

新增：

- `service_directory.proto`：`resolve`/`negotiate` request/response、candidate、binding、local transport 和 typed error；
- `nlos.sabi.ServiceDirectory` v1.0 registry identity 与独立 64 KiB payload bound；
- Rust/TypeScript/Python checked-in generated types；
- 三语言共同的 `ResolveServiceRequest` golden vector；
- `nlos-service-directory`：只读 `SnapshotDirectory`，在候选进入目录前验证 binding ID、generation、名称、版本、feature、transport 和 endpoint 上界；
- typed negotiation failure：invalid request、not found、schema/version/required feature/transport unsupported。

ServiceDirectory 自身的 bootstrap endpoint/handle 必须由受信 Namespace 或 Process birth binding 提供。本切片没有定义全局固定地址，也没有按 Rust/TypeScript/Python 实现语言发现服务。

## 2. 协商与有界规则

- `binding_id` 固定 16 bytes，generation 必须非零；
- service/schema name 各不超过 255 bytes，禁止空值和 NUL；endpoint 不超过 4096 bytes；
- feature IDs 必须非零、严格升序、唯一，最多 128 个；
- transport kind 必须已知、非 `UNSPECIFIED`、严格升序且有界；
- snapshot 最多 256 个 registration，构造时逐服务验证完整 resolve response 不超过 64 KiB；
- resolve 只返回 candidate descriptor 和可用 transport kind，不返回 OS endpoint address；
- negotiate 逐层区分 service、schema、version、required feature 和 transport 失败；
- 多个兼容 binding 的确定性选择顺序为：更高 minor → 更高 generation → 更小 binding ID；注册输入顺序不影响结果；
- 错误不回显 endpoint、注册清单或超界/畸形 service 输入。

`resolve` candidate 的稳定排序使用 service/schema、major/minor、generation 和 binding ID。当前 feature ID 只是已排序的 opaque numeric registry key，不代表 feature registry 已冻结。

## 3. 测试与结果

本地 macOS arm64 已通过：

```sh
npm run schema:lint
npx --no-install buf breaking --against '.git#branch=origin/main,subdir=schema'
npm run schema:generate
npm run schema:typecheck
npm run schema:test:typescript
python tests/conformance/schema/envelope.py
cargo test -p nlos-schema -p nlos-service-directory
cargo clippy -p nlos-schema -p nlos-service-directory --all-targets -- -D warnings
cargo fmt --all -- --check
```

Rust 新增 1 项 schema/registry 测试和 5 项目录行为测试，覆盖：

1. 三语言逐字节读取同一 ServiceDirectory golden；
2. payload identity、unknown major、缺失 response result 与 64 KiB bound fail-closed；
3. malformed/duplicate registration 在进入 snapshot 前被拒绝；
4. resolve 输出与注册顺序无关；
5. negotiate 选择更高 minor/generation，并验证 required feature/transport；
6. 五类 compatibility failure 返回稳定 typed error，畸形输入不被反射。

## 4. 当前能证明什么

- ServiceDirectory 已有独立 schema identity、跨语言生成物、golden 和大小上限，不再只是架构文档中的方法名；
- endpoint address 只在 negotiate 成功结果出现，resolve 阶段不能被直接当作可连接 binding；
- Rust snapshot 对相同注册集合和相同请求产生确定结果，不依赖 arrival order；
- schema/version/feature/transport 失败不会被压缩成同一个字符串错误；
- 添加第二个 proto 后，protobuf-es 跨文件 import 由生成配置固定为 `.js` specifier，可在 NodeNext 下从 TypeScript source 正确解析。

## 5. 当前不能证明什么

- 尚无通过 IPC 提供 `resolve/negotiate` 的真实 ServiceDirectory server，也没有 TypeScript/Python resolver 调用目录后自动连接业务服务；
- `watch`、`describe_error`、动态注册、撤销、lease/TTL、generation change notification 和 stale binding retry 尚未实现；
- bootstrap handle 的 Namespace 继承、目录 peer authentication、Capability 和进程 incarnation binding 尚未实现；
- feature ID registry、服务 schema registry、负载均衡、健康检查和多 endpoint policy 尚未冻结；
- common SABI 的 Principal/Application/Process、deadline/cancel、IdempotencyKey、Operation/Receipt、partial/uncertain error 尚未进入 envelope；
- 当前 snapshot 是进程内候选，不是 durable authority，也不构成 production service manager。

因此本 Evidence 只把 `ServiceDirectory schema + Rust resolve/negotiate core` 记为 `PARTIAL PASS`。下一验收门是让 Rust 目录 server 与 TypeScript/Python resolver 通过真实 Unix socket/Windows named pipe 完成“bootstrap → negotiate → 连接业务服务”组合，然后再推进 common SABI。
