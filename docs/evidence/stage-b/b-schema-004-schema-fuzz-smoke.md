# B-SCHEMA-004：Protobuf / deterministic CBOR fuzz smoke 证据

> 状态：PARTIAL PASS
>
> 日期：2026-08-02
>
> 对应：`COMPAT-VER-001`、`COMPAT-VER-002`、`SEM-CANON-001`、`RESCROW-CANON-001`

## 1. 实现范围

新增独立 `fuzz/` cargo-fuzz package，不进入普通 workspace 和生产依赖图。它使用 `cargo-fuzz 0.13.2`、`libfuzzer-sys 0.4.13`、固定 CI nightly `2026-08-01` 与 libFuzzer AddressSanitizer，提供三个目标：

| target | 输入上限 | 成功路径不变量 |
|---|---:|---|
| `protobuf_envelope` | raw 1 MiB + 1；hex seed 输入 2,097,154 bytes | parser 先执行 1 MiB frame gate；成功后 `wire_bytes` 与 `into_wire_bytes` 必须逐字节等于输入，unknown field 不得因 decode/re-encode 丢失 |
| `canonical_body` | raw 4,097；hex seed 输入 8,194 bytes | 分别以空 critical registry 和支持 ID 7 解码；任何成功结果都满足 `encode(decoded) == input`，且 critical extension 必须属于显式支持集 |
| `signing_preimage` | raw 4,201；hex seed 输入 8,402 bytes | 固定 expected domain；任何成功结果重新构造完整 domain + length + body preimage 后必须逐字节等于输入 |

`hex:` 只用于让 checked-in seed 保持文本可审查；标记丢失或变异后的输入按 raw bytes 处理。超出 harness 上限的输入在进入 parser 前返回，parser 自身的 1 MiB/4 KiB 上限仍独立生效。

## 2. Checked-in corpus

`fuzz/seeds/` 复用当前 Protobuf/CBOR/preimage golden 内容，并增加下列恶意类别：

- Protobuf：malformed length、unterminated varint、unknown field forwarding；
- CBOR：duplicate key、out-of-order key、non-shortest integer、indefinite map、tag、float、malformed length、unknown major、higher minor、unknown critical extension；
- signing preimage：wrong domain、domain length tamper、body length tamper、trailing byte、truncated prefix。

运行脚本只把 seed 复制到 `mktemp` corpus；libFuzzer 生成的新 corpus 不会修改 canonical seed。crash/timeout/OOM artifact 路由到已忽略的 `fuzz/artifacts/`。

## 3. 本地 sanitizer 结果

平台：macOS arm64；`rustc 1.99.0-nightly (ad3d0bc14 2026-07-31)`；每个 target 设置 `-max_total_time=10 -timeout=5 -rss_limit_mb=2048`。

| target | 实际时长 | executions | 结束时 mutation limit | 峰值 RSS | crash/timeout/OOM/断言反例 |
|---|---:|---:|---:|---:|---:|
| `protobuf_envelope` | 11 s | 3,853,870 | 27,927 bytes | 707 MiB | 0 |
| `canonical_body` | 11 s | 5,122,064 | 8,194 bytes | 638 MiB | 0 |
| `signing_preimage` | 11 s | 6,523,926 | 8,402 bytes | 554 MiB | 0 |

合计 15,499,860 次执行、33 秒；没有生成 counterexample artifact。`mutation limit` 是该次 libFuzzer 结束时已推进到的变异长度，不等于生产允许的 frame 大小；生产 parser 上限仍由代码中的 1 MiB/4 KiB gate 决定。

默认 CI smoke profile 每个 target 2,000 次、共 6,000 次，使用相同长度/timeout/RSS 上限。[GitHub Actions fuzz run 30717749638](https://github.com/cty12356541/llmos/actions/runs/30717749638) 在 Linux 上成功，job 总计 1m17s，其中固定 nightly/cargo-fuzz 安装后，三个 target 的编译与执行步骤为 34s。

[GitHub Actions cross-platform run 30717749643](https://github.com/cty12356541/llmos/actions/runs/30717749643) 同时完成原有 schema generation/conformance、workspace test 和 Clippy：Ubuntu 51s、macOS 1m0s、Windows 1m56s，Ubuntu 额外执行 rustfmt。实现提交前的本地 workspace/fuzz Clippy、root/fuzz rustfmt、三语言 conformance 和 sanitizer smoke 也全部通过。

## 4. 复现

```sh
cargo install cargo-fuzz --version 0.13.2 --locked
rustup toolchain install nightly-2026-08-01 --profile minimal
NLOS_FUZZ_TOOLCHAIN=nightly-2026-08-01 scripts/run-fuzz-smoke.sh
NLOS_FUZZ_SECONDS=10 scripts/run-fuzz-smoke.sh
```

## 5. 当前不能证明什么

- 33 秒本地 fuzz 和每目标 2,000 次 CI 只是可持续 smoke gate，不是数小时/数天的长期 fuzz、覆盖率收敛或 production parser 证明；
- 当前只使用 AddressSanitizer/libFuzzer；尚未加入 memory/thread sanitizer、AFL/honggfuzz、Miri、结构感知 arbitrary generator 或 corpus minimization 基线；
- Protobuf 成功路径证明原始 frame 可保留，不证明所有语言 runtime 的 unknown-field round-trip 行为一致；
- CBOR fuzz 只覆盖当前 `DigestEnvelope` profile，不代表完整 Receipt/Event/TrustPolicy/Resource Escrow schema；
- RSS 包含 libFuzzer、sanitizer、coverage 和 corpus 开销，不能当作生产 parser 的内存 benchmark。

因此该 Evidence 只把“可重复 sanitizer fuzz smoke 门禁”记为 `PARTIAL PASS`。ADR-0003 继续保持 `POC`；长期 fuzz 仍是公共 ABI 冻结和 production claim 前的必做项。
