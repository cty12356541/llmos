# W82-B4 签名 Blob 引用服务库验证

证据 ID `w82-b4-blob-service-20261116` · W82-B4 / Attempt 20261116-2 · 2026-11-16 · `H4`

Windows 实施验证及独立审查通过。下列实测为隔离候选的对应结果。主树复验和三平台验证见[汇总门](../../management/stage-e-progress.md#2026-11-16-三服务库验证)。

- 任务 `W82-B4 / Attempt 20261116-2`。窗口 2026-11-16，候选分支 `codex/w82-b4-blob`。原派发基线 `4d097056e3ebd57e130d9303e02e23433fc9d1ba`，实施基线与候选 HEAD 均为已合流前置 `cdb0d10d0a68c7d23b04081bc0031942cbb2b893`，本轮结果在该 HEAD 的隔离工作树中。代码与测试补丁 SHA-256 为 `9bd91692fa0ddcc0cbcfadbc88a70aba2a97c599b75edd72f5fbaaad491af1dc`。该摘要对应初版候选，接受的两个 B4 补丁（实现 Fix 2 和 WAL 补验）另列于归档。[补丁回执](artifacts/w82-services-20261116/integration-inputs.json) · [集成源码清单](artifacts/w82-services-20261116/services-source-manifest.json) · [制品摘要](artifacts/w82-services-20261116/sha256-manifest.json)
- 共享调用接口 Git blob 为 `72d8053faabf6f465e5f042f2f75c135d7fc4ade`，根锁 Git blob 为 `c3928b59dfb83b2eff4365983c36dfdf0a2c587d`。已预锁 `cap-std`、`cap-fs-ext`、`cap-primitives` 4.0.3，包含手工解析器修复。接口与依赖依据见[前置证据](w82-service-prerequisites-20261116.md)和[Blob 补充裁定](../../management/w82-b-design-draft.md#55-2026-11-16-补充裁定blob-可实施契约)。

## 双主体与调用合同

- `BlobReferenceService` 暴露 `mint_reference`、`read_reference`、`release_reference`、`handle_for_ipc` 与有界 `maintain`。每次进入共享授权边界前均做 bounded payload decode、通用上下文字段比对、语义摘要重算与目标绑定。缺失签名、语义摘要不配合、未受信 Task binding 和时钟失败使用类型化拒绝。
- `SignedMintCommand` 和 `SignedReleaseCommand` 为进程内 sidecar。签名复用 Capability command v1 域的 kind 4 和 5，覆盖 principal、application、process 代际及完整流程字段，包含作用域内原始幂等键与签发时刻。会话和截止时间由当前受信调用核验。IdentityAuthority 按当前 key 验证 Ed25519 与用途，接收者 principal 经同一 authority 解析并校验当前有效期与撤销状态。
- 真实 authority 测试由 A 签给 B，B 的独立服务能力及当前身份获准逐块恢复 WAV。A 与第三主体 C 持同一引用读仍被拒。夹具中的策略明确允许的主体、方法及 namespace，并回调同一 CapabilityAuthority 核验当前记录。实际生产权位、用途策略、受信 workspace resolver、slot 映射和签名输送待 INT 接线。
- release 允许当前 issuer 或 holder 在获准且签名正确时操作。首次成功固化单条撤销与回执，同 actor、application、key 与摘要可在当前授权下重放。其他已授权操作者见到已释放状态。签名与授权结果重放仍重验；读重放在有效期和读配额内重读原句柄并核对块摘要，回执无块数据。

## 根目录相对打开

- 接收受管本地 workspace 的 `cap_std::fs::Dir`。斜杠分隔的相对路径限制在 1,024 UTF-8 字节、32 段、每段 255 字节。拒绝空段、路径穿越、Windows 设备名、ADS、尾点和尾空格。逐段 `open_dir_nofollow`，末级 read-only + no-follow，Unix 加非阻塞。Windows 额外检查已打开目录与文件的 reparse 属性。末级从实际打开的句柄校验普通文件、单硬链接和长度，随后把同一对象交给流式全文及逐块 SHA-256 和钉住句柄表。
- Windows 11 Pro build 28000 的 NTFS 测试对首次打开各做 2,048 次访问。换入 junction 父目录 130 次，内部读取 814 次、安全拒绝 1,234 次、外部或意外内容 0 次。换入指向外侧夹具的硬链接末级 512 次，内部读取 489 次、安全拒绝 1,559 次、外部或意外内容 0 次。首次打开与 mint 后换靶分别测试。末级已钉后对原件改名并用外侧硬链接占原路径，读取仍来自原对象。原对象的原地改写、截断与扩长均拒绝异常块。
- 支持范围限定为由受信 resolver 管控根、祖先和挂载的本地文件系统。授权锚点是抓取时的目录对象，Unix 移动目录后仍可定位该对象。实现未施加 `NO_XDEV`，文件链接数也不能证明来源。远程、虚拟和用户控制挂载的文件系统 profile 需要独立强制边界。Windows 第三方终端安全 hook 兼容性待专用环境验证。
- Windows 末级符号链接用例显式尝试时，夹具创建返回系统错误 1314。它由允许创建符号链接的 runner 单独执行。Linux 内核 `openat2` 和手工 fallback、macOS 目录句柄、Unix FIFO 和 socket 负例均待 native 实测。实际设备节点未纳入本机夹具。主树初次 Windows helper 的普通 opener 套件 4 通过、1 忽略，显式运行受权限约束的 symlink 用例退出 101（仍为 1314）。[初次集成专项](artifacts/w82-services-20261116/initial-native-helper-results.jsonl)

## 持久线性化与资源

- SQLite WAL/FULL 同一 CapabilityAuthority 数据库上 additive v3 只新增 Blob ledger。STRICT 和 trigger 保护 token/manifest、幂等读取回执和撤销记录。有效读数由 append-only ordinal 衍生。独占的相邻锁文件持有操作系统排他锁，durable epoch 单调增加。只有当前 epoch 与实际钉住句柄可返回字节。重启丢弃所有钉住句柄，旧 mint key 可复用历史回执，且不会调用路径 resolver。
- read 在 per-token fd 锁内取时、授权、核验来源与接收者、读一块并核验 SHA-256；I/O 后重新取时、鉴权，在和来源能力吊销共享的数据库事务中核验代际、来源祖先、释放与 ordinal，随后提交回执。已有 durable admission 的块在并发撤销下可交付，后续新块拒绝。每个唯一读键最多占一次读额度。重放仍受时效、吊销和基于句柄的内容复验；读尽后回收 fd，后续重放按读尽状态拒绝。
- 固定块长 1–65,408，位置按 mint 块长对齐，接收长度须容纳整块。末块可短，大小为 0 的文件可 mint，EOF 返回类型化错误且不新增 read receipt。响应使用 canonical SchemaIdentity 和实际 bounded encode，全 protobuf ≤65,536 字节。
- 默认文件上限 64 MiB，manifest 最多 4,096 块及 128 KiB；候选 64 MiB 文件在一次隔离压力探针使用完整 4,096 块 manifest，并以另外四个引用并发返回 65,408 字节块。Windows 对该测试进程采样 303 次，内核峰值工作集 9,224,192 字节，采样峰值私有内存 2,785,280 字节。该量值包含进程、测试夹具与 SQLite 开销，采样仅覆盖该负载。
- 活令牌/文件句柄默认每主体 16 个、全局 256 个，发行者和接收者都计入。并发 read 默认每主体 4、全局 16。mint 每 60 秒每主体 16、全局 64，重连共享配额。默认 TTL 供调用方选择 300 秒，上限 3,600 秒，且受授予有效期约束。单 token 可承诺 1–4,096 次读。TTL、release 与读尽回收 active permit。受信维护调度可在空闲时关闭到期句柄。
- 审计 admission 按 512 MiB 或 2,000,000 行的 75% 水位截住新 mint，加计每个已发放 token 的最坏剩余 read 和 release。估算同时核对实际 DB+WAL 字节；read 在近水位时为在期 release 留位。审计永久增长，物理 SQLite/WAL 及其他能力活动能影响可用空间。该检查给出有界 admission，真正的磁盘字节配额需宿主提供。空间和 I/O 失败返回错误，无失败帧附带块。同步本地系统调用以调用间的截止时间检查约束，已提交操作遇时间失败仍保留幂等回执。
- 新 TCB 包含 Blob 服务、v3 ledger、已预锁 rooted-open 后端、受信时钟/授权器/目录 resolver 和一项 Identity 查询。查询用现有 key loader 表达当前 principal binding，不衍生权限。重复 principal key head 的坏状态复现了 `query_row` 只取第一行，改用 `query_one` 后查询和 Capability 命令验签均产生既有 `CorruptRecord`。

## 本机验证与剩余门

- `cargo test --locked -p nlos-capability` 退出 0，75 项单元和集成测试通过，1 项 compile-fail 文档测试通过。默认忽略 2 项，其中隔离内存探针另跑退出 0，Windows 末级符号链接的权限缺口单列。
- `cargo test --locked -p nlos-identity` 退出 0，34 项通过。其中新增 principal lookup 覆盖未知、bootstrap、轮换、撤销与重复 key head。原权限、ledger、custody、fault 与共享调用合同测试通过。
- WAL 故障试验在迁移、epoch 增量、mint、read count/receipt、release/revoke 各注入 pre-commit IOERR 或 SQLITE_FULL，确认未追加半条记录，重开与原键重试收敛。另测试 v3 DDL 原子回滚、旧历史、两进程写者互斥、原始行替换/修改/删除、manifest 损坏及 range/EOF、配额和拒绝重放。Blob 的受控撕裂 WAL 补验见末节。真实断电仍未验证。
- `cargo clippy --locked -p nlos-capability -p nlos-identity --all-targets -- -D warnings`、`cargo fmt -p nlos-capability -p nlos-identity -- --check`、`cargo +1.97 check --locked -p nlos-capability -p nlos-identity --all-targets`、`git diff --check` 和含新文件的补丁反向检查均退出 0。短路径整仓格式、独立 workspace 锁门、供应链和 ROAD-E-001 在合流时验收。
- 复现 Windows 末级 symlink 使用 `cargo test --locked -p nlos-capability --test blob_open repeated_first_open_symlink_leaf_swaps_do_not_escape -- --ignored --nocapture`。首次打开计数使用 `cargo test --locked -p nlos-capability --test blob_open -- --nocapture`。
- [Windows 根句柄测试](artifacts/w82-services-20261116/b4-win-opener.txt) · [本机 symlink 权限缺口](artifacts/w82-services-20261116/b4-win-symlink-privilege.txt)。该证据表明库路径可逐块恢复 WAV；qq-agent spill 路径退役、daemon/SDK 签名传输、适用平台产品化与远端 CI 各有后续验收门。

## Fix 1 独立审查修订

- Fix 1 的代码与测试补丁 SHA-256 为 `5bb0db1438d32ee922e21fda0141d730e9fbfb089e78e5326fd9afc32fdf3235`，仍基于 `cdb0d10d0a68c7d23b04081bc0031942cbb2b893`。上面的原候选测量和摘要保留作历史对照。Fix 1 的审计预留与 deadline 复审通过。[审查](artifacts/w82-services-20261116/b4-storage-review.txt) · [预留反例](artifacts/w82-services-20261116/b4-reserve-red.txt) · [等待反例](artifacts/w82-services-20261116/b4-deadline-red.txt) · [截止时间复验](artifacts/w82-services-20261116/b4-deadline-green.txt) · [内存样本](artifacts/w82-services-20261116/b4-memory-sample.json)
- 审查红例在 5,000,000 字节的配置下成功 mint 4,096 次读，第 2,454 次读和有效签名 release 均返回 `AuditFull`。Fix 1 在 4 KiB 页下预先拒绝该 mint，resolver 调用次数为 0。默认容量实测全部 4,096 次读和持久 release 成功，结束时 DB+WAL 为 1,651,968 字节。1 KiB 页、3,000,000 字节配置下，一个令牌完成 70 次读及持久 release，结束为 109,320 字节。该令牌存活时，另一 issuer 申请相同预留被提前拒绝。
- 审计预留按 SQLite 实际页大小计算，先计已观察 DB 作为保守基线，再为历史行、所有在期未撤销令牌的剩余 reads/release 和新 mint 的完整承诺留空间。结构页上界为每行 13 页，覆盖 rowid 表的叶和内部页、最多五棵索引的键及单页溢出、记录链舍入。记录溢出负载另以两倍已估算逻辑字节计入。初次 split 保留的 freelist 高水位和下一次回执的突发分配分别留界。
- 每笔变更额外核对实际 DB 文件、逻辑 `page_count` 和 WAL。瞬态页预算取自锁定 SQLite 的 20 层 btree 和 `balance_nonroot` 的五个输出 sibling 上界，覆盖受影响索引、父页、split/freelist、overflow、计数页与 WAL sector padding。Blob 事务临时关闭 cache spill，超过 64 frame 窗口时在事务前尝试 TRUNCATE checkpoint。busy 按请求剩余时间重试，checkpoint I/O 错误中止副作用，审计行永久保留。既有 capability 调用的 busy 设置从 Blob 连接锁释放前恢复。物理模型用于页大小 512–65,536 的默认非 auto-vacuum SQLite 受管本地库，独立的非 Blob writer 可改变实际容量。
- 两个 20 ms 回归在修复前分别等待 fd 252.1018 ms 和 SQLite 265.9488 ms 后才拒绝。Fix 1 的最终实测为 20.2811 ms 和 20.083 ms，均返回 `DeadlineExpired` 且无新增回执。持有读快照阻塞 checkpoint 的用例也在截止后拒绝，读者退出后原键可重试。外部时钟、策略和目录 resolver 继续在 authority DB 锁外；本库控制的 fd、connection 和 SQLite writer 等待服从请求限额。进入内核后的同步 I/O 仍按前后检查收口。
- mint 在事务前短暂登记 pending pin，数据库提交决定引用的可见性。登记失败、写失败与超时回收 pin 和 permit。并发 release 在 mint 响应返回前撤销可读权与回收 active quota。新增测试独立覆盖每主体与全局读并发、跨 issuer mint 限速、同 recipient 跨 issuer 的 pin 配额、不同主体原始幂等 key 隔离；旧 boot 读取仍拒绝。
- Fix 1 的 Windows 终验为 `cargo test --locked -p nlos-capability -- --nocapture` 退出 0，87 项单元和集成测试、1 项 compile-fail doc test。Identity 34 项、Clippy、定向 fmt、Rust 1.97.1 全 target 编译检查均退出 0。隔离 memory probe 另跑退出 0，321 个样本，内核峰值工作集 9,969,664 字节，采样私有内存峰值 3,051,520 字节。Windows 最终 junction 153 次换入、591 次内读、1,457 次安全拒绝、0 外侧；硬链接末级 512 次换入、546 次内读、1,502 次安全拒绝、0 外侧。
- Unix FIFO、socket、`/dev/null` 字符设备具名用例均已入树，Linux/macOS native 待验。字符设备用例只验证拒绝打开，夹具不读字节。Windows symlink 1314、Linux fallback、第三方安全 hook、真实断电和 INT 生产接线仍待各自验收。受控撕裂 WAL 已另补测。

## Fix 2 并发取时

- Fix 2 在同一独立时钟锁内完成 host clock 采样、wall/monotonic 两域校验及成对发布。回调错误或任一域真实回退返回错误，失败样本完全丢弃。时钟回调与策略在 authority DB 锁外，继续要求回调不重入同一服务。
- 已建立调用 `Timing` 后，取时锁排队使用原请求截止时间。入口首样本取得前尚无法解释 host deadline，使用 100 ms 本地候样上限，超时返回 `TimeUnavailable`，不以旧样本决定授权。取得首样本后检查请求原截止时间。空闲维护使用 5 秒本地候样上限。受信同步回调需有界返回。
- 暂停较早的正常时钟样本并让另一调用并发取时。Fix 1 的较晚样本首先发布，较早调用收到 `TimeUnavailable`。Fix 2 两调用均返回成功，发布顺序和采样顺序一致。分别注入真实 wall 回退、monotonic 回退及回调错误，各错误后的较低合法非回退样本仍接受。首次候样争用实测 100.075 ms 返回 `TimeUnavailable`；已有 80 ms 截止时间的读在 80.477 ms 返回 `DeadlineExpired`，未新增读回执。
- 调整 mint/release 夹具以服从有序取时。mint 已 durable 且暂停在响应取时回调时发起 release，确认它仍在途后放行取时；并发 release 完成后原引用不可读，下一次 mint 取得回收的 pin 容量。
- Windows `cargo test --locked -p nlos-capability --test blob_reference -- --nocapture` 退出 0，40 通过、1 忽略。定向 clock 用例 5/5，Capability Clippy 与定向 fmt 退出 0。隔离 memory、完整 Identity 和跨平台门未在此轮复跑。Fix 1 的 F1/F2 独立核查为已处理，Fix 2 的 F3 局部复审通过，定向 fmt 指针已补实际重验。原代码补丁 SHA-256 `26790c9819215e90aee0dfce134143deb5f0d97012188a43ecba8a642588ba49`。[并发取时反例](artifacts/w82-services-20261116/b4-clock-red.txt) · [修复后 5/5](artifacts/w82-services-20261116/b4-clock-green.txt) · [Blob 套件](artifacts/w82-services-20261116/b4-blob-suite.txt) · [局部复审](artifacts/w82-services-20261116/b4-clock-review.txt) · [fmt 重验](artifacts/w82-services-20261116/b4-fmt.txt)

## SQLite WAL 提交尾部与恢复补验

`blob_wal_recovery` 使用真实签名 Blob、SQLite WAL/FULL 与故障 VFS。在 COMMIT 第 4 次 `xWrite` 注入 `SQLITE_IOERR`（扩展码 10），此时有一帧完整未提交、下一帧只有头，调用返回错误且无成功回执。再用未确认帧的校验和破坏镜像作独立对照，已确认前缀保持原字节。故障点移至不可达位置时用例预期失败，证明返回成功会触发断言。恢复原测试后，Windows 具名目标通过 1/1。

原库、撕裂 WAL 镜像及未提交帧损坏镜像均恢复 2 个令牌、1 条撤销、1 条已确认读和原审计快照，`integrity_check=ok`。故障读无回执或新增计数。重启 epoch 1→2，旧 mint/release 原键可返回原回执，旧读被拒，旧工作区文件已移除且未调用 resolver。此门覆盖真实提交写错误和受控镜像恢复。单独的 `xSync` 失败、硬断电与设备缓存保证仍无实测。测试增量和精确失败边界通过独立审核。[红色对照](artifacts/w82-services-20261116/b4-wal-red.txt) · [具名补验](artifacts/w82-services-20261116/b4-wal-green.txt) · [独立审核](artifacts/w82-services-20261116/b4-wal-review.txt)
