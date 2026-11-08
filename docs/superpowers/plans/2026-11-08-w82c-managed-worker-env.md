# W82-C 受管 Worker 环境变量契约 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 按独立 `workers.manifest` 清单拉起外部多进程组件,注入且只注入 `LLMOS_AUTH_ENDPOINT` / `LLMOS_PROCESS_IDENTITY_SEED` / `LLMOS_ROOT_CAPABILITY_SLOT` 三变量,主进程退出时子进程树干净回收(Unix 进程组原语,零 unsafe)。

**Architecture:** 三 crate 接线:nlos-package 新增 lib 承载清单解析; nlos-process 扩展 KillScope 事实记录 + 分组拉起/组杀原语 + 三变量白名单命令构造 + 升级式回收;nlos-up 新增 `workers` 子命令做装配与端到端编排。设计全文见 spec。

**Tech Stack:** Rust(workspace `unsafe_code = "forbid"`、MSRV 1.99.0、零 serde)、std::process + tokio、nix 0.31.3(仅 unix,既有 `signal` feature 已含 `killpg`)。

**Spec:** `docs/superpowers/specs/2026-11-08-w82c-managed-worker-env-design.md`(本计划从 spec 立论,执行者须同读)

## Global Constraints

- 写集:`crates/nlos-up/**`、`crates/nlos-package/**`、`crates/nlos-process/**` + 本计划显式列出的 `docs/superpowers/plans/` 与 `Cargo.lock`(机械再生)。禁碰:nlos-system-control、nlos-artifact、nlos-runtime-tokio 等一切其他 crate、`.github/**`、`docs/management/**`、`claims.yaml`、`.agentdash/**`。
- `SpawnSupervisedRequest` 的字段集**冻结不动**(越集字面量 `crates/nlos-runtime-tokio/tests/kill_receipt.rs:524` 依赖)。
- 子进程环境 = `env_clear()` 后**恰好三个**变量,不多不少;desktop 链路(`.envs()` 叠加)与 `bootstrap_smoke.rs:389-402` dry-run 断言零改动。
- 零新第三方依赖(nix 已有;`process_group` 是 std)。零 `unsafe`。
- 既有 `kill`/`spawn_supervised`/`request_platform_kill*` 签名与语义零漂移(nlos-process 88 个既有测试 + runtime-tokio 测试须全绿)。
- 提交作者 `cty12356541 <171764500+cty12356541@users.noreply.github.com>`;`<type>(<scope>): 中文祈使句 (W82-C)`;精确 `git add`,提交前跑 `cargo fmt` 与本 crate 测试。
- 验证平台:macOS(本机)。Windows 断言为声明级(同型代码路径),PR 显式标注未实机核验。

---

### Task 1: nlos-package lib —— workers.manifest 解析器

**Files:**
- Modify: `crates/nlos-package/Cargo.toml`(加 `[lib]`)
- Create: `crates/nlos-package/src/lib.rs`
- Create: `crates/nlos-package/src/managed_workers.rs`(内联 `#[cfg(test)]` 单测)
- 不动: `crates/nlos-package/src/main.rs`(bin 零改动)

**Interfaces:**
- Produces(后续任务消费的精确签名):
  - `pub const MAX_WORKER_NAME_BYTES: usize = 128;`
  - `pub struct ManagedWorkerUnit { pub name: String, pub interpreter: String, pub argv: Vec<String>, pub cwd: Option<String>, pub root_capability_slot: String }`
  - `pub struct ManagedWorkerManifest { pub units: Vec<ManagedWorkerUnit> }`
  - `pub enum ManagedWorkerManifestError { Io { path: std::path::PathBuf, source: std::io::Error }, Parse { line: usize, detail: String } }`(`std::error::Error + Display`;`line == 0` 表示全文件级错误)
  - `pub fn parse_managed_worker_manifest(text: &str) -> Result<ManagedWorkerManifest, ManagedWorkerManifestError>`
  - `pub fn read_managed_worker_manifest(path: &Path) -> Result<ManagedWorkerManifest, ManagedWorkerManifestError>`

- [ ] **Step 1: 写失败测试(完整 managed_workers.rs 测试面)**

创建 `crates/nlos-package/src/managed_workers.rs`,只含测试模块(实现留空使编译失败):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const QQ_AGENT: &str = "\
# qq-agent 受管双进程示例
worker = adapter python3 adapter/main.py --port 7901
slot = adapter root-cap-placeholder
worker = brain python3 brain/main.py --mode resident
slot = brain root-cap-placeholder
";

    #[test]
    fn parses_full_manifest_with_comments_and_defaults() {
        let manifest = parse_managed_worker_manifest(QQ_AGENT).expect("parse");
        assert_eq!(manifest.units.len(), 2);
        let adapter = &manifest.units[0];
        assert_eq!(adapter.name, "adapter");
        assert_eq!(adapter.interpreter, "python3");
        assert_eq!(
            adapter.argv,
            vec!["adapter/main.py".to_owned(), "--port".to_owned(), "7901".to_owned()]
        );
        assert_eq!(adapter.cwd, None);
        assert_eq!(adapter.root_capability_slot, "root-cap-placeholder");
        assert_eq!(manifest.units[1].name, "brain");
        assert_eq!(manifest.units[1].argv.last().unwrap(), "resident");
    }

    #[test]
    fn cwd_applies_to_declared_worker() {
        let text = "worker = a /bin/sh a.sh\ncwd = a sub/dir\n";
        let manifest = parse_managed_worker_manifest(text).expect("parse");
        assert_eq!(manifest.units[0].cwd.as_deref(), Some("sub/dir"));
    }

    #[test]
    fn rejects_duplicate_worker_names() {
        let text = "worker = a /bin/sh one.sh\nworker = a /bin/sh two.sh\n";
        let err = parse_managed_worker_manifest(text).unwrap_err();
        assert!(format!("{err}").contains("duplicate worker name"), "{err}");
    }

    #[test]
    fn rejects_unknown_key_with_line_number() {
        let text = "worker = a /bin/sh a.sh\npublisher = x\n";
        let err = parse_managed_worker_manifest(text).unwrap_err();
        assert!(format!("{err}").contains("unknown key"), "{err}");
    }

    #[test]
    fn rejects_manifest_without_workers() {
        let err = parse_managed_worker_manifest("# only comments\n").unwrap_err();
        assert!(format!("{err}").contains("at least one worker"), "{err}");
    }

    #[test]
    fn rejects_cwd_referencing_undeclared_worker() {
        let text = "worker = a /bin/sh a.sh\ncwd = b sub\n";
        let err = parse_managed_worker_manifest(text).unwrap_err();
        assert!(format!("{err}").contains("undeclared worker"), "{err}");
    }

    #[test]
    fn rejects_duplicate_cwd_and_slot_for_same_worker() {
        let text = "worker = a /bin/sh a.sh\ncwd = a x\ncwd = a y\n";
        assert!(parse_managed_worker_manifest(text).is_err());
        let text = "worker = a /bin/sh a.sh\nslot = a x\nslot = a y\n";
        assert!(parse_managed_worker_manifest(text).is_err());
    }

    #[test]
    fn rejects_cwd_traversal_absolute_and_dot_components() {
        for bad in ["..", "../up", "/abs", "a/../b", "."] {
            let text = format!("worker = a /bin/sh a.sh\ncwd = a {bad}\n");
            let err = parse_managed_worker_manifest(&text).unwrap_err();
            assert!(format!("{err}").contains("cwd"), "{err} for {bad}");
        }
    }

    #[test]
    fn rejects_relative_interpreter_with_separator() {
        let text = "worker = a tools/run.sh a.sh\n";
        assert!(parse_managed_worker_manifest(text).is_err());
        // 绝对路径与裸名合法
        assert!(parse_managed_worker_manifest("worker = a /bin/sh a.sh\n").is_ok());
        assert!(parse_managed_worker_manifest("worker = a python3 a.py\n").is_ok());
        assert!(parse_managed_worker_manifest("worker = a C:/py/python.exe a.py\n").is_ok());
    }

    #[test]
    fn rejects_worker_line_without_argv() {
        let text = "worker = a python3\n";
        assert!(parse_managed_worker_manifest(text).is_err());
    }

    #[test]
    fn rejects_bad_names() {
        for bad in ["", "x\u{0}y", &"x".repeat(129)] {
            let text = format!("worker = {bad} /bin/sh a.sh\n");
            assert!(parse_managed_worker_manifest(&text).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn read_reports_io_error_with_path() {
        let err = read_managed_worker_manifest(std::path::Path::new("/nonexistent-w82c/workers.manifest"))
            .unwrap_err();
        assert!(format!("{err}").contains("/nonexistent-w82c"), "{err}");
    }
}
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p nlos-package --lib 2>&1 | tail -5`
Expected: 编译失败(unresolved `parse_managed_worker_manifest` 等)。

- [ ] **Step 3: 最小实现**

`crates/nlos-package/Cargo.toml` 在 `[[bin]]` 段之前加:

```toml
[lib]
name = "nlos_package"
path = "src/lib.rs"
```

创建 `crates/nlos-package/src/lib.rs`:

```rust
//! `nlos-package` 库面:受管外部 Worker 清单(`workers.manifest`)解析,
//! 供编排器(nlos-up `workers` 子命令)跨 crate 消费。只做解析,不做拉起;
//! CLI 仍在 bin 目标,本 lib 不引入新依赖。

pub mod managed_workers;

pub use managed_workers::{
    ManagedWorkerManifest, ManagedWorkerManifestError, ManagedWorkerUnit, MAX_WORKER_NAME_BYTES,
    parse_managed_worker_manifest, read_managed_worker_manifest,
};
```

`crates/nlos-package/src/managed_workers.rs` 顶部(tests 模块之前)实现:

```rust
//! `workers.manifest` —— 受管外部 Worker 拉起清单(W82-C 托管约定 C)。
//!
//! 行式格式,与 `package.manifest` 同纪律:`#` 注释与空行忽略;
//! `key = value` 两侧 trim,首个 `=` 切分。三种记录行:
//!
//! ```text
//! worker = <name> <interpreter> <argv...>   # argv 至少 1 个 token,原样透传
//! cwd  = <name> <dir>    # 可选;相对路径,相对清单目录;仅可引用已声明的 worker
//! slot = <name> <token>  # 可选;单 token,字节透传;仅可引用已声明的 worker
//! ```
//!
//! 刻意独立于 `package.manifest`(不扩展包格式);`slot` 值格式契约对表
//! W82-B 设计(见 spec §2 冻结决策 2)。

use std::fmt;
use std::path::{Path, PathBuf};

/// 受管 worker 名字段节数上限(与 nlos-artifact entry-name 常量同量级)。
pub const MAX_WORKER_NAME_BYTES: usize = 128;
const MAX_TOKEN_BYTES: usize = 4096;

/// 一个受管 worker 单元:解释器、透传 argv、可选相对 cwd、字节透传 slot。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedWorkerUnit {
    pub name: String,
    pub interpreter: String,
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    pub root_capability_slot: String,
}

/// 解析完成的受管 worker 清单。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedWorkerManifest {
    pub units: Vec<ManagedWorkerUnit>,
}

/// 清单读取/解析错误。`line == 0` 表示全文件级错误(如零 worker)。
#[derive(Debug)]
pub enum ManagedWorkerManifestError {
    Io { path: PathBuf, source: std::io::Error },
    Parse { line: usize, detail: String },
}

impl fmt::Display for ManagedWorkerManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(formatter, "read managed worker manifest {}: {source}", path.display())
            }
            Self::Parse { line: 0, detail } => {
                write!(formatter, "managed worker manifest: {detail}")
            }
            Self::Parse { line, detail } => {
                write!(formatter, "managed worker manifest line {line}: {detail}")
            }
        }
    }
}

impl std::error::Error for ManagedWorkerManifestError {}

/// 从文件读取并解析 `workers.manifest`。
pub fn read_managed_worker_manifest(
    path: &Path,
) -> Result<ManagedWorkerManifest, ManagedWorkerManifestError> {
    let text = std::fs::read_to_string(path).map_err(|source| ManagedWorkerManifestError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_managed_worker_manifest(&text)
}

/// 解析清单文本。全部验证 fail-closed:任一违规即整体拒绝,零部分产出。
pub fn parse_managed_worker_manifest(
    text: &str,
) -> Result<ManagedWorkerManifest, ManagedWorkerManifestError> {
    let mut units: Vec<ManagedWorkerUnit> = Vec::new();
    let mut index_by_name: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let last_line = text.lines().count();

    for (line_no, line) in numbered_lines(text) {
        let (key, value) =
            scalar_line(&line).map_err(|detail| ManagedWorkerManifestError::Parse {
                line: line_no,
                detail,
            })?;
        match key.as_str() {
            "worker" => {
                let unit = parse_worker_line(&value).map_err(|detail| {
                    ManagedWorkerManifestError::Parse { line: line_no, detail }
                })?;
                if index_by_name.contains_key(&unit.name) {
                    return Err(ManagedWorkerManifestError::Parse {
                        line: line_no,
                        detail: format!("duplicate worker name {:?}", unit.name),
                    });
                }
                index_by_name.insert(unit.name.clone(), units.len());
                units.push(unit);
            }
            "cwd" => {
                let (name, dir) = split_name_value(&value, "cwd").map_err(|detail| {
                    ManagedWorkerManifestError::Parse { line: line_no, detail }
                })?;
                validate_cwd(&dir).map_err(|detail| ManagedWorkerManifestError::Parse {
                    line: line_no,
                    detail,
                })?;
                let index = require_declared(&index_by_name, &name, "cwd")
                    .map_err(|detail| ManagedWorkerManifestError::Parse { line: line_no, detail })?;
                if units[index].cwd.is_some() {
                    return Err(ManagedWorkerManifestError::Parse {
                        line: line_no,
                        detail: format!("duplicate cwd for worker {name:?}"),
                    });
                }
                units[index].cwd = Some(dir);
            }
            "slot" => {
                let (name, token) = split_name_value(&value, "slot").map_err(|detail| {
                    ManagedWorkerManifestError::Parse { line: line_no, detail }
                })?;
                let index = require_declared(&index_by_name, &name, "slot")
                    .map_err(|detail| ManagedWorkerManifestError::Parse { line: line_no, detail })?;
                if !units[index].root_capability_slot.is_empty() {
                    return Err(ManagedWorkerManifestError::Parse {
                        line: line_no,
                        detail: format!("duplicate slot for worker {name:?}"),
                    });
                }
                units[index].root_capability_slot = token;
            }
            other => {
                return Err(ManagedWorkerManifestError::Parse {
                    line: line_no,
                    detail: format!("unknown key {other:?}"),
                });
            }
        }
    }

    if units.is_empty() {
        return Err(ManagedWorkerManifestError::Parse {
            line: last_line,
            detail: "at least one worker is required".to_owned(),
        });
    }
    Ok(ManagedWorkerManifest { units })
}

fn parse_worker_line(value: &str) -> Result<ManagedWorkerUnit, String> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() < 3 {
        return Err("worker = <name> <interpreter> <argv...> (argv needs at least one token)"
            .to_owned());
    }
    let name = tokens[0];
    if name.is_empty() || name.contains('\0') || name.len() > MAX_WORKER_NAME_BYTES {
        return Err(format!(
            "worker name {name:?} is empty, NUL-bearing, or over {MAX_WORKER_NAME_BYTES} bytes"
        ));
    }
    let interpreter = tokens[1];
    if !valid_interpreter(interpreter) {
        return Err(format!(
            "interpreter {interpreter:?} must be a bare name or an absolute path"
        ));
    }
    for token in &tokens[2..] {
        if token.contains('\0') || token.len() > MAX_TOKEN_BYTES {
            return Err(format!("argv token {token:?} is NUL-bearing or oversized"));
        }
    }
    Ok(ManagedWorkerUnit {
        name: name.to_owned(),
        interpreter: interpreter.to_owned(),
        argv: tokens[2..].iter().map(|s| (*s).to_owned()).collect(),
        cwd: None,
        root_capability_slot: String::new(),
    })
}

fn split_name_value(value: &str, key: &str) -> Result<(String, String), String> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() != 2 {
        return Err(format!("{key} = <name> <value> (exactly two tokens)"));
    }
    Ok((tokens[0].to_owned(), tokens[1].to_owned()))
}

fn require_declared(
    index_by_name: &std::collections::HashMap<String, usize>,
    name: &str,
    key: &str,
) -> Result<usize, String> {
    index_by_name
        .get(name)
        .copied()
        .ok_or_else(|| format!("{key} references undeclared worker {name:?}"))
}

fn valid_interpreter(token: &str) -> bool {
    if token.is_empty() || token.contains('\0') || token.len() > MAX_TOKEN_BYTES {
        return false;
    }
    if !token.contains('/') && !token.contains('\\') {
        return true; // 裸名,由编排器在父进程侧按 PATH 解析
    }
    token.starts_with('/') || windows_drive_absolute(token)
}

fn windows_drive_absolute(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn validate_cwd(dir: &str) -> Result<(), String> {
    if dir.is_empty() || dir.contains('\0') || dir.len() > MAX_TOKEN_BYTES {
        return Err(format!("cwd {dir:?} is empty, NUL-bearing, or oversized"));
    }
    let path = Path::new(dir);
    if path.is_absolute() {
        return Err("cwd must be a relative path under the manifest directory".to_owned());
    }
    // 只接受全 Normal 组件:统一拒绝 Prefix/RootDir(Windows 盘符与根相对
    // 路径,后者在 Windows 上 is_absolute() 为 false)与 CurDir/ParentDir。
    for component in path.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(
                "cwd must be a plain relative path (no prefix, root, `..`, or `.` components)"
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// 与 bin 内 `numbered_lines` 同纪律(bin 不动,lib 自持副本)。
fn numbered_lines(text: &str) -> impl Iterator<Item = (usize, String)> {
    text.lines().enumerate().map(|(index, line)| {
        let trimmed = line.trim().to_owned();
        (index + 1, trimmed)
    }).filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
}

fn scalar_line(line: &str) -> Result<(String, String), String> {
    let Some((key, value)) = line.split_once('=') else {
        return Err("expected `key = value`".to_owned());
    };
    let key = key.trim();
    if key.is_empty() {
        return Err("empty key".to_owned());
    }
    Ok((key.to_owned(), value.trim().to_owned()))
}
```

- [ ] **Step 4: 跑测试确认通过 + bin 不回归**

Run: `cargo test -p nlos-package 2>&1 | tail -8`
Expected: lib 13 测试全 PASS + 既有 42 集成 + 5 单测全 PASS。

- [ ] **Step 5: 提交**

```bash
cargo fmt -p nlos-package
git add crates/nlos-package/Cargo.toml crates/nlos-package/src/lib.rs crates/nlos-package/src/managed_workers.rs
git diff --staged --stat   # 恰 3 文件
git -c user.name="cty12356541" -c user.email="171764500+cty12356541@users.noreply.github.com" commit -m "feat(nlos-package): 新增 lib 承载受管 worker 清单解析 (W82-C)

workers.manifest 行式解析器(worker/cwd/slot 三记录行,fail-closed 验证),
独立于 package.manifest;bin 零改动。验证:cargo test -p nlos-package
lib 13 + 既有 47 全绿(macOS)。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: nlos-process —— KillScope 事实记录 + 分组拉起 + 组杀适配

**Files:**
- Modify: `crates/nlos-process/src/supervisor_pid.rs`(`KillScope` + 两结构体字段 + registry 语义 + 新错误变体)
- Modify: `crates/nlos-process/src/platform_kill.rs`(trait 组杀方法 + Posix/Windows 覆写)
- Modify: `crates/nlos-process/src/supervisor.rs`(`spawn_supervised_grouped`/`kill_force` + scope 分派)
- Modify: `crates/nlos-process/src/lib.rs`(re-export)
- Modify: `crates/nlos-process/tests/*.rs`(既有 `RegisterSupervisorPidRequest`/`SupervisorPidEntry` 字面量机械补 `kill_scope: KillScope::Process`)
- Create: `crates/nlos-process/tests/managed_supervision.rs`

**Interfaces:**
- Consumes: 无(首改造点)
- Produces(Task 3/4 依赖):
  - `pub enum KillScope { Process, ProcessGroup }`(`Clone+Copy+Debug+Eq+PartialEq`,定义于 supervisor_pid.rs,lib.rs re-export)
  - `RegisterSupervisorPidRequest`/`SupervisorPidEntry` 新增 `pub kill_scope: KillScope`
  - `pub fn spawn_supervised_grouped(&self, request: SpawnSupervisedRequest, command: &mut Command) -> Result<SupervisedSpawn, SupervisorError>`(unix 侧应用 `process_group(0)`)
  - `pub fn kill_force(&self, process_id: ProcessId, expected_process_generation: Generation) -> Result<SupervisorSignalOutcome, SupervisorError>`(仅 ProcessGroup scope;Process scope fail-closed)
  - trait 新默认方法 `fn signal_platform_kill_group(&self, process_id: ProcessId, process_generation: Generation, force: bool) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError>`(默认 `Err(Platform("process-group kill unsupported by this adapter"))`)
  - `SupervisorPidRegistryError::KillScopeMismatch { process_id: ProcessId, generation: Generation, registered: KillScope, presented: KillScope }`

- [ ] **Step 1: 写失败测试 `crates/nlos-process/tests/managed_supervision.rs`**

```rust
//! W82-C 受管分组监管面:KillScope 事实记录、Unix 进程组拉起与组杀。
//! unix 门控:组原语仅 Unix 有真实语义(Windows taskkill /T 既有覆盖)。
#![cfg(unix)]

use nlos_process::{
    KillScope, PlatformKillAdapter, PosixPlatformKillAdapter, ProcessId, ProcessSupervisor,
    RegisterSupervisorPidRequest, SpawnSupervisedRequest, SupervisorError,
    SupervisorPidRegistry, SupervisorPidRegistryError, SupervisorSignalOutcome,
};
use nlos_types::Generation;
use std::collections::HashMap;
use std::process::Command;

fn spawn_request(process_id: ProcessId) -> SpawnSupervisedRequest {
    SpawnSupervisedRequest {
        process_id,
        process_generation: Generation::INITIAL,
        registered_at_ms: 0,
    }
}

fn sh_sleep_with_grandchild(marker: &str) -> Command {
    // 孙进程:sleep 300(其 pid 写入 marker.grandpid);直子 sh 存活等待。
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(format!(
        "sleep 300 & echo $! > {marker}.grandpid; echo $$ > {marker}.pid; wait"
    ));
    command
}

#[test]
fn registry_records_kill_scope_and_replay_mismatch_fails_closed() {
    let registry = SupervisorPidRegistry::new();
    let process_id = ProcessId::from_bytes([7; 16]);
    let request = |kill_scope| RegisterSupervisorPidRequest {
        process_id,
        process_generation: Generation::INITIAL,
        os_pid: 4242,
        registered_at_ms: 0,
        kill_scope,
    };
    registry.register(request(KillScope::Process)).expect("register");
    let entry = registry.lookup(process_id).expect("lookup");
    assert_eq!(entry.kill_scope, KillScope::Process);
    // 同 (id, gen, pid) 但 scope 不同 → fail-closed
    let err = registry.register(request(KillScope::ProcessGroup)).unwrap_err();
    assert!(matches!(err, SupervisorPidRegistryError::KillScopeMismatch { .. }), "{err}");
    // 完全一致的 replay 仍通过
    registry.register(request(KillScope::Process)).expect("replay");
}

#[test]
fn posix_adapter_group_signal_resolves_generation_fence() {
    let process_id = ProcessId::from_bytes([9; 16]);
    let map = HashMap::from([((process_id, Generation::INITIAL), 999_999u32)]);
    let adapter = PosixPlatformKillAdapter::with_generation_pid_map(map);
    // 代际不匹配 → fail closed(不触组)
    let err = adapter
        .signal_platform_kill_group(process_id, Generation::INITIAL.checked_next().unwrap(), false)
        .unwrap_err();
    let _ = err;
    // 未知 (id,gen) 之外的已映射但不存在 pid → killpg ESRCH → AlreadyTerminated
    let outcome = adapter
        .signal_platform_kill_group(process_id, Generation::INITIAL, true)
        .expect("probe");
    assert_eq!(outcome, nlos_process::PlatformKillAdapterOutcome::AlreadyTerminated);
}

#[test]
fn grouped_spawn_makes_child_its_own_group_leader() {
    let dir = std::env::temp_dir().join(format!(
        "w82c-leader-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let marker = dir.join("m");
    // kill -0 -$$ 成功 ⇔ 本进程是组长(pgid == pid)
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(format!(
        "if kill -0 -$$ 2>/dev/null; then echo leader > {}; else echo follower > {}; fi; sleep 30",
        marker.display(),
        marker.display()
    ));
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([1; 16]);
    let mut spawned = supervisor
        .spawn_supervised_grouped(spawn_request(process_id), &mut command)
        .expect("grouped spawn");
    assert_eq!(
        supervisor.registry().lookup(process_id).expect("entry").kill_scope,
        KillScope::ProcessGroup
    );
    let leader = poll_file(&marker, 10).expect("leader marker");
    assert_eq!(leader.trim(), "leader");
    supervisor.kill(process_id, Generation::INITIAL).expect("kill");
    let _ = spawned.child().wait();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn group_kill_terminates_direct_child_and_grandchild() {
    let dir = std::env::temp_dir().join(format!(
        "w82c-tree-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let marker = dir.join("m");
    let mut command = sh_sleep_with_grandchild(&marker.display().to_string());
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([2; 16]);
    let mut spawned = supervisor
        .spawn_supervised_grouped(spawn_request(process_id), &mut command)
        .expect("grouped spawn");
    let grandpid: u32 = poll_file(&marker.with_extension("grandpid"), 10)
        .expect("grandpid")
        .trim()
        .parse()
        .expect("pid number");
    let outcome = supervisor.kill(process_id, Generation::INITIAL).expect("kill");
    assert_eq!(outcome, SupervisorSignalOutcome::Applied);
    let _ = spawned.child().wait();
    assert!(wait_pid_gone(grandpid, 50), "grandchild {grandpid} survived group kill");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kill_force_requires_process_group_scope() {
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([3; 16]);
    let mut command = Command::new("/bin/sleep");
    command.arg("30");
    let mut spawned = supervisor
        .spawn_supervised(spawn_request(process_id), &mut command)
        .expect("plain spawn");
    let err = supervisor.kill_force(process_id, Generation::INITIAL).unwrap_err();
    assert!(matches!(err, SupervisorError::PlatformKill(_)), "{err}");
    supervisor.kill(process_id, Generation::INITIAL).expect("cleanup kill");
    let _ = spawned.child().wait();
}

fn poll_file(path: &std::path::Path, seconds: u32) -> Option<String> {
    for _ in 0..(seconds * 20) {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    None
}

fn wait_pid_gone(pid: u32, seconds: u32) -> bool {
    for _ in 0..(seconds * 20) {
        let status = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status();
        match status {
            Ok(stat) if stat.success() => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => return true,
        }
    }
    false
}
```

同时新建 `crates/nlos-process/tests/fixtures/worker_env_dump.sh`(Task 3 复用,此处先落盘):

```sh
#!/bin/sh
# W82-C 受管 worker 环境白名单夹具:$1 = 输出基名(无空白路径)
env > "$1.env"
echo $$ > "$1.pid"
sleep 300 &
echo $! > "$1.grandpid"
wait
```

Run: `cargo test -p nlos-process --test managed_supervision 2>&1 | tail -5`
Expected: 编译失败(`KillScope` 等未定义)。

- [ ] **Step 2: 实现 supervisor_pid.rs 的 KillScope**

在 `crates/nlos-process/src/supervisor_pid.rs`:

(a) `use` 区后新增:

```rust
/// 一条监管注册在 kill 时的信号作用域(W82-C 托管约定 C)。
///
/// `Process` 是既有语义(只对注册 os pid 发信号);`ProcessGroup` 记录该
/// 子进程以进程组组长身份拉起(unix `process_group(0)`,pgid == os pid),
/// kill 信号作用于整组。scope 是注册期事实,登记后不可变:同
/// `(process, generation, os pid)` 的 replay 携带不同 scope 属于矛盾
/// 陈述,fail-closed 拒绝;代际推进(Supersede)自然携带新 scope。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KillScope {
    /// 仅注册 os pid 本体(经典单进程语义)。
    Process,
    /// 注册 os pid 的整个进程组(树回收语义;Windows 上 `taskkill /T`
    /// 等价覆盖)。
    ProcessGroup,
}
```

(b) `RegisterSupervisorPidRequest` 与 `SupervisorPidEntry` 各加字段(放在 `registered_at_ms` 之后):

```rust
    /// 本注册的 kill 作用域(见 [`KillScope`])。
    pub kill_scope: KillScope,
```

(c) `SupervisorPidRegistryError` 加变体:

```rust
    /// The same generation and OS pid replayed with a different kill
    /// scope; the registration's scope is an immutable fact (fail-closed,
    /// zero side effect).
    KillScopeMismatch {
        process_id: ProcessId,
        generation: Generation,
        registered: KillScope,
        presented: KillScope,
    },
```

及其 Display 臂:

```rust
            Self::KillScopeMismatch {
                registered,
                presented,
                ..
            } => write!(
                formatter,
                "kill scope mismatch rejected for the same registration: \
                 registered {registered:?}, presented {presented:?}"
            ),
```

(d) `register()` 内 `let entry = SupervisorPidEntry { ... }` 加 `kill_scope: request.kill_scope,`;replay 分支(`if current.os_pid == request.os_pid` 之前)加:

```rust
            if current.kill_scope != request.kill_scope {
                return Err(SupervisorPidRegistryError::KillScopeMismatch {
                    process_id: request.process_id,
                    generation: request.process_generation,
                    registered: current.kill_scope,
                    presented: request.kill_scope,
                });
            }
```

- [ ] **Step 3: 实现 platform_kill.rs 组杀面**

(a) `PlatformKillAdapter` trait 增加默认方法:

```rust
    /// Group-scoped signal over the registered OS pid's whole process
    /// group (W82-C). `force == false` 请求协作终止(Unix SIGTERM),
    /// `force == true` 请求强杀(Unix SIGKILL)。默认实现 fail-closed:
    /// 不支持组语义的适配器(Stub/Noop/错宿主)一律拒绝,不静默降级为
    /// 单进程信号。
    fn signal_platform_kill_group(
        &self,
        _process_id: ProcessId,
        _process_generation: Generation,
        _force: bool,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError> {
        Err(PlatformKillAdapterError::Platform(
            "process-group kill unsupported by this adapter",
        ))
    }
```

(b) `#[cfg(unix)] impl PosixPlatformKillAdapter` 块内加私有助手,并在其 `impl PlatformKillAdapter` 中覆写(既有 `signal_platform_kill` 保持逐字节不变):

```rust
    fn signal_group(
        &self,
        process_id: ProcessId,
        process_generation: Generation,
        force: bool,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError> {
        use nix::errno::Errno;
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;

        let os_pid = self.pid_map.resolve(process_id, process_generation)?;
        let signal = if force { Signal::SIGKILL } else { Signal::SIGTERM };
        match killpg(Pid::from_raw(os_pid.cast_signed()), signal) {
            Ok(()) => Ok(PlatformKillAdapterOutcome::Signaled),
            Err(Errno::ESRCH) => Ok(PlatformKillAdapterOutcome::AlreadyTerminated),
            Err(_) => Err(PlatformKillAdapterError::Platform(
                "killpg failed for mapped os pid group",
            )),
        }
    }
```

```rust
    fn signal_platform_kill_group(
        &self,
        process_id: ProcessId,
        process_generation: Generation,
        force: bool,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError> {
        self.signal_group(process_id, process_generation, force)
    }
```

(c) `#[cfg(windows)] impl PlatformKillAdapter for WindowsPlatformKillAdapter` 覆写(taskkill /F /T 本就树杀+强杀,force 差异如实映射为同一路径):

```rust
    fn signal_platform_kill_group(
        &self,
        process_id: ProcessId,
        process_generation: Generation,
        _force: bool,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError> {
        // `taskkill /F /T` 已经是整树强杀:组语义与 force 语义在此合一。
        self.signal_platform_kill(process_id, process_generation)
    }
```

- [ ] **Step 4: 实现 supervisor.rs 分组拉起与 kill 分派**

(a) `spawn_supervised` 重构为委托(公开签名与行为不变):

```rust
    pub fn spawn_supervised(
        &self,
        request: SpawnSupervisedRequest,
        command: &mut Command,
    ) -> Result<SupervisedSpawn, SupervisorError> {
        self.spawn_supervised_scoped(request, command, KillScope::Process)
    }

    /// Spawns `command` as a process-group leader host child
    /// (`process_group(0)` on Unix — safe `CommandExt`, so the child's
    /// pgid equals its OS pid and a group kill reaches the whole tree;
    /// no-op on Windows, where `taskkill /T` already tree-kills) and
    /// registers it under [`KillScope::ProcessGroup`]. W82-C 托管约定 C
    /// 的受管拉起入口。
    pub fn spawn_supervised_grouped(
        &self,
        request: SpawnSupervisedRequest,
        command: &mut Command,
    ) -> Result<SupervisedSpawn, SupervisorError> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        self.spawn_supervised_scoped(request, command, KillScope::ProcessGroup)
    }

    fn spawn_supervised_scoped(
        &self,
        request: SpawnSupervisedRequest,
        command: &mut Command,
        kill_scope: KillScope,
    ) -> Result<SupervisedSpawn, SupervisorError> {
        let mut child = command.spawn().map_err(SupervisorError::Spawn)?;
        let os_pid = child.id();
        match self.registry.register(RegisterSupervisorPidRequest {
            process_id: request.process_id,
            process_generation: request.process_generation,
            os_pid,
            registered_at_ms: request.registered_at_ms,
            kill_scope,
        }) {
            Ok(decision) => Ok(SupervisedSpawn { child, decision }),
            Err(error) => {
                let torn_down_os_pid = child.id();
                let _ = child.kill();
                let _ = child.wait();
                Err(SupervisorError::SpawnRefused {
                    cause: error,
                    torn_down_os_pid,
                })
            }
        }
    }
```

(注意:拒绝路径的 `child.kill()` 在 unix 上是 SIGKILL 单 pid——分组拒绝时孙进程可能残留;此处与既有语义一致(注册失败发生在孙进程产生前),保持不动。)

(b) `kill()` 的 `adapter.signal_platform_kill(...)` 调用改为 scope 分派:

```rust
        let signal_result = if entry.kill_scope == KillScope::ProcessGroup {
            adapter.signal_platform_kill_group(process_id, entry.process_generation, false)
        } else {
            adapter.signal_platform_kill(process_id, entry.process_generation)
        };
        match signal_result {
```

(c) 新增 `kill_force`(放在 `kill` 之后;`HostKillAdapter` 的 group 委托见 (d)):

```rust
    /// Force-kills (Unix `killpg(SIGKILL)`, Windows `taskkill /F /T`) the
    /// process group registered for `(process_id, expected_generation)`.
    /// Only meaningful for [`KillScope::ProcessGroup`] registrations: a
    /// `Process`-scope registration fails closed rather than group-signal
    /// an unrelated process group that pid may happen to lead.
    ///
    /// # Errors
    ///
    /// Fails closed on an unknown mapping, a stale generation, a
    /// `Process`-scope registration, or an adapter rejection.
    #[cfg(any(unix, windows))]
    pub fn kill_force(
        &self,
        process_id: ProcessId,
        expected_process_generation: Generation,
    ) -> Result<SupervisorSignalOutcome, SupervisorError> {
        let entry = self.resolve_current_entry(process_id, expected_process_generation)?;
        self.run_kill_after_resolve_hook();
        if entry.kill_scope != KillScope::ProcessGroup {
            return Err(SupervisorError::PlatformKill(
                crate::platform_kill::PlatformKillAdapterError::Platform(
                    "kill_force requires a ProcessGroup-scope registration",
                ),
            ));
        }
        let adapter = HostKillAdapter::from_fenced_entry(&entry);
        match adapter.signal_platform_kill_group(process_id, entry.process_generation, true) {
            Ok(crate::platform_kill::PlatformKillAdapterOutcome::Signaled) => {
                Ok(SupervisorSignalOutcome::Applied)
            }
            Ok(crate::platform_kill::PlatformKillAdapterOutcome::AlreadyTerminated) => {
                Ok(SupervisorSignalOutcome::AlreadyTerminated)
            }
            Err(error) => Err(SupervisorError::PlatformKill(error)),
        }
    }

    /// 非 unix/windows 宿主 fail-closed(与 `kill` 的平台矩阵对齐)。
    #[cfg(not(any(unix, windows)))]
    pub fn kill_force(
        &self,
        process_id: ProcessId,
        expected_process_generation: Generation,
    ) -> Result<SupervisorSignalOutcome, SupervisorError> {
        let _ = (process_id, expected_process_generation);
        Err(SupervisorError::UnsupportedOnPlatform {
            operation: "kill_force",
        })
    }
```

(d) `impl PlatformKillAdapter for HostKillAdapter` 加 group 委托:

```rust
    fn signal_platform_kill_group(
        &self,
        process_id: ProcessId,
        process_generation: Generation,
        force: bool,
    ) -> Result<
        crate::platform_kill::PlatformKillAdapterOutcome,
        crate::platform_kill::PlatformKillAdapterError,
    > {
        match self {
            #[cfg(unix)]
            Self::Posix(adapter) => {
                adapter.signal_platform_kill_group(process_id, process_generation, force)
            }
            #[cfg(windows)]
            Self::Windows(adapter) => {
                adapter.signal_platform_kill_group(process_id, process_generation, force)
            }
        }
    }
```

(e) `use crate::supervisor_pid::{...}` 加 `KillScope`。

- [ ] **Step 5: lib.rs re-export**

`pub use supervisor_pid::{...}` 行加入 `KillScope`(按字母序放在 `RegisterSupervisorPidRequest` 前)。

- [ ] **Step 6: 既有测试字面量机械修补**

Run: `grep -n "RegisterSupervisorPidRequest {" crates/nlos-process/tests/*.rs crates/nlos-process/src/*.rs`
对每个字面量(预期 ~10 处,分布在 platform_kill.rs / supervisor.rs / supervisor_pid_registry.rs / supervisor_pid_scan.rs 的 tests)在 `registered_at_ms: ...` 行后加 `kill_scope: KillScope::Process,`;对应文件 `use nlos_process::{...}`/`use crate::...` 导入 `KillScope`。若 grep 还发现 `SupervisorPidEntry {` 字面量,同样补 `kill_scope: KillScope::Process,`。

- [ ] **Step 7: 跑新旧测试确认通过**

Run: `cargo test -p nlos-process 2>&1 | tail -8`
Expected: 新 managed_supervision 5 测试 PASS + 既有 88+2 全 PASS。

- [ ] **Step 8: 提交**

```bash
cargo fmt -p nlos-process
git add crates/nlos-process/src/supervisor_pid.rs crates/nlos-process/src/platform_kill.rs \
  crates/nlos-process/src/supervisor.rs crates/nlos-process/src/lib.rs \
  crates/nlos-process/tests/managed_supervision.rs crates/nlos-process/tests/fixtures/worker_env_dump.sh \
  $(grep -l "kill_scope" crates/nlos-process/tests/*.rs | grep -v managed_supervision | tr '\n' ' ')
git diff --staged --stat   # 只含 nlos-process
git -c user.name="cty12356541" -c user.email="171764500+cty12356541@users.noreply.github.com" commit -m "feat(nlos-process): 受管进程组拉起与组杀原语(KillScope 事实记录) (W82-C)

KillScope(Process/ProcessGroup)登记入 RegisterSupervisorPidRequest/
SupervisorPidEntry(replay 携带矛盾 scope fail-closed);unix 分组拉起
process_group(0)(safe CommandExt)+ killpg(SIGTERM/SIGKILL) 组杀,
Windows 沿用 taskkill /F /T;SpawnSupervisedRequest 字段集冻结不动。
验证:cargo test -p nlos-process 新 5 + 既有全绿(macOS)。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: nlos-process managed.rs —— 三变量白名单 + 升级式回收

**Files:**
- Create: `crates/nlos-process/src/managed.rs`
- Modify: `crates/nlos-process/src/lib.rs`(`pub mod managed;` + re-export)
- Create: `crates/nlos-process/tests/managed_env.rs`
- Create: `crates/nlos-process/tests/fixtures/worker_env_dump.py`

**Interfaces:**
- Consumes: Task 2 的 `KillScope::ProcessGroup`、`spawn_supervised_grouped`、`kill`/`kill_force`、`SpawnSupervisedRequest`、`SupervisedSpawn`
- Produces(Task 4 依赖):
  - `pub const MANAGED_ENV_AUTH_ENDPOINT: &str = "LLMOS_AUTH_ENDPOINT";`
  - `pub const MANAGED_ENV_PROCESS_IDENTITY_SEED: &str = "LLMOS_PROCESS_IDENTITY_SEED";`
  - `pub const MANAGED_ENV_ROOT_CAPABILITY_SLOT: &str = "LLMOS_ROOT_CAPABILITY_SLOT";`
  - `pub struct ManagedWorkerEnv { pub auth_endpoint: String, pub process_identity_seed: String, pub root_capability_slot: String }` + `pub fn entries(&self) -> [(String, String); 3]`
  - `pub fn prepare_managed_command(program: &str, argv: &[String], cwd: &Path, env: &ManagedWorkerEnv) -> std::process::Command`
  - `pub fn spawn_managed_worker(supervisor: &ProcessSupervisor, request: SpawnSupervisedRequest, program: &str, argv: &[String], cwd: &Path, env: &ManagedWorkerEnv) -> Result<SupervisedSpawn, SupervisorError>`
  - `pub enum ManagedKillOutcome { Terminated, AlreadyTerminated }`
  - `pub struct ManagedKillReceipt { pub process_id: ProcessId, pub process_generation: Generation, pub os_pid: u32, pub outcome: ManagedKillOutcome, pub escalated: bool, pub group_drained: bool, pub exit_status: Option<std::process::ExitStatus> }`
  - `pub fn terminate_group_with_escalation(supervisor: &ProcessSupervisor, process_id: ProcessId, expected_generation: Generation, child: &mut std::process::Child, grace: std::time::Duration) -> Result<ManagedKillReceipt, SupervisorError>`

- [ ] **Step 1: 写失败测试 `crates/nlos-process/tests/managed_env.rs`**

```rust
//! W82-C 受管三变量白名单注入与升级式组回收。unix 门控(组原语仅 Unix)。
#![cfg(unix)]

use nlos_process::{
    MANAGED_ENV_AUTH_ENDPOINT, MANAGED_ENV_PROCESS_IDENTITY_SEED,
    MANAGED_ENV_ROOT_CAPABILITY_SLOT, ManagedWorkerEnv, ProcessId, ProcessSupervisor,
    SpawnSupervisedRequest, prepare_managed_command, spawn_managed_worker,
    terminate_group_with_escalation,
};
use nlos_types::Generation;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("w82c-me-{}-{tag}-{}", std::process::id(), line!()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn sample_env() -> ManagedWorkerEnv {
    ManagedWorkerEnv {
        auth_endpoint: "/tmp/w82c-auth.sock".to_owned(),
        process_identity_seed: "/tmp/w82c-operator.key".to_owned(),
        root_capability_slot: "slot-token-xyz".to_owned(),
    }
}

fn poll_file(path: &Path, seconds: u32) -> Option<String> {
    for _ in 0..(seconds * 20) {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn parse_env_dump(text: &str) -> std::collections::BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn pid_gone(pid: u32, seconds: u32) -> bool {
    for _ in 0..(seconds * 20) {
        let ok = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn resolve_python3() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("python3"))
        .find(|candidate| candidate.is_file())
}

#[test]
fn env_constants_and_entries_order() {
    assert_eq!(MANAGED_ENV_AUTH_ENDPOINT, "LLMOS_AUTH_ENDPOINT");
    assert_eq!(MANAGED_ENV_PROCESS_IDENTITY_SEED, "LLMOS_PROCESS_IDENTITY_SEED");
    assert_eq!(MANAGED_ENV_ROOT_CAPABILITY_SLOT, "LLMOS_ROOT_CAPABILITY_SLOT");
    let entries = sample_env().entries();
    let names: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        [
            MANAGED_ENV_AUTH_ENDPOINT,
            MANAGED_ENV_PROCESS_IDENTITY_SEED,
            MANAGED_ENV_ROOT_CAPABILITY_SLOT
        ]
    );
    assert!(entries[2].1.contains("slot-token-xyz"));
}

#[test]
fn prepared_command_carries_exactly_three_envs_and_cwd() {
    let cwd = temp_dir("prep");
    let command = prepare_managed_command(
        "/bin/sh",
        &["script.sh".to_owned()],
        &cwd,
        &sample_env(),
    );
    let mut envs: Vec<(String, String)> = command
        .get_envs()
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|b| b.to_string_lossy().into_owned()).unwrap_or_default()))
        .collect();
    envs.sort();
    assert_eq!(envs.len(), 3, "exactly three env vars, got {envs:?}");
    assert_eq!(command.get_current_dir(), Some(cwd.as_path()));
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn spawned_worker_env_is_exactly_three_vars_and_parent_does_not_leak() {
    let dir = temp_dir("sh");
    let base = dir.join("w");
    let argv = vec![
        fixture("worker_env_dump.sh").display().to_string(),
        base.display().to_string(),
    ];
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([4; 16]);
    let mut spawned = spawn_managed_worker(
        &supervisor,
        SpawnSupervisedRequest {
            process_id,
            process_generation: Generation::INITIAL,
            registered_at_ms: 0,
        },
        "/bin/sh",
        &argv,
        &dir,
        &sample_env(),
    )
    .expect("spawn");
    let dump = poll_file(&base.with_extension("env"), 15).expect("env dump");
    let map = parse_env_dump(&dump);
    // cargo test 父环境必然含 PATH/HOME 等 → 集合恰等三键即证父环境零泄漏
    assert_eq!(
        map.keys().collect::<Vec<_>>(),
        [
            "LLMOS_AUTH_ENDPOINT",
            "LLMOS_PROCESS_IDENTITY_SEED",
            "LLMOS_ROOT_CAPABILITY_SLOT"
        ],
        "child env leaked: {map:?}"
    );
    assert_eq!(map.get("LLMOS_AUTH_ENDPOINT").unwrap(), "/tmp/w82c-auth.sock");
    assert_eq!(map.get("LLMOS_ROOT_CAPABILITY_SLOT").unwrap(), "slot-token-xyz");
    let receipt = terminate_group_with_escalation(
        &supervisor,
        process_id,
        Generation::INITIAL,
        spawned.child(),
        Duration::from_secs(5),
    )
    .expect("teardown");
    assert!(receipt.group_drained, "{receipt:?}");
    let grandpid: u32 = poll_file(&base.with_extension("grandpid"), 5)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid_gone(grandpid, 10), "grandchild {grandpid} survived");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn python_worker_env_is_exactly_three_vars() {
    let Some(python) = resolve_python3() else {
        panic!("python3 required for the W82-C gate-letter test (acceptance: 拉起外部 Python 组件)");
    };
    let dir = temp_dir("py");
    let base = dir.join("w");
    let argv = vec![
        fixture("worker_env_dump.py").display().to_string(),
        base.display().to_string(),
    ];
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([5; 16]);
    let mut spawned = spawn_managed_worker(
        &supervisor,
        SpawnSupervisedRequest {
            process_id,
            process_generation: Generation::INITIAL,
            registered_at_ms: 0,
        },
        python.to_str().expect("utf8 python path"),
        &argv,
        &dir,
        &sample_env(),
    )
    .expect("spawn");
    let dump = poll_file(&base.with_extension("env"), 20).expect("python env dump");
    let map = parse_env_dump(&dump);
    assert_eq!(map.len(), 3, "python child env leaked: {map:?}");
    assert!(map.contains_key("LLMOS_PROCESS_IDENTITY_SEED"));
    terminate_group_with_escalation(
        &supervisor,
        process_id,
        Generation::INITIAL,
        spawned.child(),
        Duration::from_secs(5),
    )
    .expect("teardown");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn escalation_forces_term_ignoring_direct_child_and_drains_group() {
    let dir = temp_dir("esc");
    let marker = dir.join("m");
    // 直子 sh 忽略 SIGTERM(trap ''),孙进程 sleep 300 → 宽限超时 → 强杀
    let mut script = Command::new("/bin/sh");
    script.arg("-c").arg(format!(
        "trap '' TERM; sleep 300 & echo $! > {gp}; echo $$ > {pid}; wait",
        gp = marker.with_extension("grandpid").display(),
        pid = marker.with_extension("pid").display(),
    ));
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([6; 16]);
    let mut spawned = supervisor
        .spawn_supervised_grouped(
            SpawnSupervisedRequest {
                process_id,
                process_generation: Generation::INITIAL,
                registered_at_ms: 0,
            },
            &mut script,
        )
        .expect("grouped spawn");
    let grandpid: u32 = poll_file(&marker.with_extension("grandpid"), 10)
        .expect("grandpid")
        .trim()
        .parse()
        .unwrap();
    let receipt = terminate_group_with_escalation(
        &supervisor,
        process_id,
        Generation::INITIAL,
        spawned.child(),
        Duration::from_millis(800),
    )
    .expect("escalation teardown");
    assert!(receipt.escalated, "expected SIGKILL escalation: {receipt:?}");
    assert!(receipt.group_drained);
    assert!(receipt.exit_status.is_some());
    assert!(pid_gone(grandpid, 10), "grandchild survived SIGKILL drain");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn already_terminated_direct_child_still_drains_lingering_grandchild() {
    let dir = temp_dir("orphan");
    let marker = dir.join("m");
    let mut script = Command::new("/bin/sh");
    script.arg("-c").arg(format!(
        "sleep 300 & echo $! > {gp}; exit 0",
        gp = marker.with_extension("grandpid").display(),
    ));
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([7; 16]);
    let mut spawned = supervisor
        .spawn_supervised_grouped(
            SpawnSupervisedRequest {
                process_id,
                process_generation: Generation::INITIAL,
                registered_at_ms: 0,
            },
            &mut script,
        )
        .expect("grouped spawn");
    let grandpid: u32 = poll_file(&marker.with_extension("grandpid"), 10)
        .expect("grandpid")
        .trim()
        .parse()
        .unwrap();
    let receipt = terminate_group_with_escalation(
        &supervisor,
        process_id,
        Generation::INITIAL,
        spawned.child(),
        Duration::from_secs(5),
    )
    .expect("orphan drain teardown");
    assert_eq!(receipt.outcome, nlos_process::ManagedKillOutcome::AlreadyTerminated);
    assert!(receipt.group_drained, "lingering grandchild must be drained: {receipt:?}");
    assert!(pid_gone(grandpid, 10));
    let _ = std::fs::remove_dir_all(&dir);
}
```

`crates/nlos-process/tests/fixtures/worker_env_dump.py`:

```python
"""W82-C gate-letter fixture: dump full environ, then hold a grandchild.

argv[1] = output basename: writes <base>.env / <base>.pid / <base>.grandpid.
"""
import os
import subprocess
import sys
import time

base = sys.argv[1]
with open(base + ".env", "w", encoding="utf-8") as handle:
    for key in sorted(os.environ):
        handle.write(f"{key}={os.environ[key]}\n")
with open(base + ".pid", "w", encoding="utf-8") as handle:
    handle.write(str(os.getpid()))
child = subprocess.Popen(["sleep", "300"])
with open(base + ".grandpid", "w", encoding="utf-8") as handle:
    handle.write(str(child.pid))
while child.poll() is None:
    time.sleep(0.5)
```

Run: `cargo test -p nlos-process --test managed_env 2>&1 | tail -5`
Expected: 编译失败(managed 模块不存在)。

- [ ] **Step 2: 实现 `crates/nlos-process/src/managed.rs`**

```rust
//! W82-C 托管约定 C 的受管 worker 注入与回收面。
//!
//! 三变量白名单:受管 worker 子进程环境 = `env_clear()` 后恰好三个
//! `LLMOS_*` 变量(spec §4.2)——子进程无 PATH(裸解释器名由编排器在
//! 父侧解析为绝对路径),父环境零继承。升级式回收:组 SIGTERM → 宽限
//! 轮询直子 → 组 SIGKILL → 有界组排空核验,at-least-once(重放安全)。
//! Unix 依赖 Task 2 的 `process_group(0)` + `killpg`;Windows 沿用
//! `taskkill /F /T`(单级强杀,无宽限二级,平台差异如实登记)。

use std::path::Path;
use std::process::{Child, Command, ExitStatus};
use std::time::Duration;

use nlos_types::{Generation, ProcessId};

use crate::supervisor::{ProcessSupervisor, SpawnSupervisedRequest, SupervisedSpawn, SupervisorError, SupervisorSignalOutcome};
use crate::supervisor_pid::KillScope;

/// 认证 IPC 端点(Unix socket / Windows 命名管道路径字符串)。
pub const MANAGED_ENV_AUTH_ENDPOINT: &str = "LLMOS_AUTH_ENDPOINT";
/// 进程身份种子(当前约定为 operator 种子文件路径;见 spec §8)。
pub const MANAGED_ENV_PROCESS_IDENTITY_SEED: &str = "LLMOS_PROCESS_IDENTITY_SEED";
/// 根能力槽位(字节透传;格式契约 TODO 对表 W82-B 设计)。
pub const MANAGED_ENV_ROOT_CAPABILITY_SLOT: &str = "LLMOS_ROOT_CAPABILITY_SLOT";

const DRAIN_PROBE_ATTEMPTS: u32 = 200;
const DRAIN_PROBE_STEP: Duration = Duration::from_millis(10);
const GRACE_POLL_STEP: Duration = Duration::from_millis(20);

/// 一次受管拉起注入的三变量值(顺序即注入顺序)。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedWorkerEnv {
    pub auth_endpoint: String,
    pub process_identity_seed: String,
    pub root_capability_slot: String,
}

impl ManagedWorkerEnv {
    /// Ordered `(NAME, VALUE)` pairs — exactly three, no more, no less.
    #[must_use]
    pub fn entries(&self) -> [(String, String); 3] {
        [
            (MANAGED_ENV_AUTH_ENDPOINT.to_owned(), self.auth_endpoint.clone()),
            (
                MANAGED_ENV_PROCESS_IDENTITY_SEED.to_owned(),
                self.process_identity_seed.clone(),
            ),
            (
                MANAGED_ENV_ROOT_CAPABILITY_SLOT.to_owned(),
                self.root_capability_slot.clone(),
            ),
        ]
    }
}

/// 构造最小权限命令:env_clear + 恰三变量 + cwd(unix 分组由
/// [`spawn_managed_worker`] 追加)。纯构造,可跨平台单测(get_envs)。
#[must_use]
pub fn prepare_managed_command(
    program: &str,
    argv: &[String],
    cwd: &Path,
    env: &ManagedWorkerEnv,
) -> Command {
    let mut command = Command::new(program);
    command.args(argv);
    command.current_dir(cwd);
    command.env_clear();
    for (name, value) in env.entries() {
        command.env(name, value);
    }
    command
}

/// 分组拉起一个受管 worker:三变量白名单 + 进程组组长(KillScope 记录)。
///
/// # Errors
///
/// 透传 [`ProcessSupervisor::spawn_supervised_grouped`] 的失败
/// (宿主 spawn 失败,或注册拒绝——此时子进程已被拆干净)。
pub fn spawn_managed_worker(
    supervisor: &ProcessSupervisor,
    request: SpawnSupervisedRequest,
    program: &str,
    argv: &[String],
    cwd: &Path,
    env: &ManagedWorkerEnv,
) -> Result<SupervisedSpawn, SupervisorError> {
    let mut command = prepare_managed_command(program, argv, cwd, env);
    supervisor.spawn_supervised_grouped(request, &mut command)
}

/// 升级式组回收的终端结果分类。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedKillOutcome {
    /// 首发组信号时目标仍存活(后续按回执 escalated 区分是否强杀)。
    Terminated,
    /// 首发组信号时目标组已整体不存在(ESRCH / taskkill 128)。
    AlreadyTerminated,
}

/// 一次受管 worker 组回收的 at-least-once 回执。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedKillReceipt {
    pub process_id: ProcessId,
    pub process_generation: Generation,
    pub os_pid: u32,
    pub outcome: ManagedKillOutcome,
    /// 宽限超时后是否升级到组 SIGKILL(Windows 单级强杀恒 false)。
    pub escalated: bool,
    /// 组排空核验:反复组信号至 ESRCH(有界次数内)。
    pub group_drained: bool,
    pub exit_status: Option<ExitStatus>,
}

/// 升级式组回收:组 SIGTERM → 宽限轮询直子 → (未退)组 SIGKILL →
/// 有界组排空核验。at-least-once:任一步重放安全;首发 AlreadyTerminated
/// 仍会执行排空核验(直子先亡、孙进程残留的场景由此覆盖)。
///
/// 注意:直子在宽限内自行退出即进入排空核验——孙进程在直子退出瞬间
/// 被强杀收尾(整组已收过 SIGTERM;回收优先于孙进程的额外宽限,
/// spec §4.4 契约)。
///
/// # Errors
///
/// 代际栅栏拒绝(未知映射/陈旧代际/Process scope 的 kill_force 门槛)
/// 或宿主信号失败——均为 fail-closed,调用方回执如实记录。
pub fn terminate_group_with_escalation(
    supervisor: &ProcessSupervisor,
    process_id: ProcessId,
    expected_generation: Generation,
    child: &mut Child,
    grace: Duration,
) -> Result<ManagedKillReceipt, SupervisorError> {
    let os_pid = child.id();
    let first = supervisor.kill(process_id, expected_generation)?;
    let outcome = match first {
        SupervisorSignalOutcome::Applied => ManagedKillOutcome::Terminated,
        SupervisorSignalOutcome::AlreadyTerminated => ManagedKillOutcome::AlreadyTerminated,
    };

    let mut exit_status = None;
    let mut waited = false;
    let mut elapsed = Duration::ZERO;
    while elapsed < grace {
        if let Some(status) = child
            .try_wait()
            .map_err(SupervisorError::Spawn)?
        {
            exit_status = Some(status);
            waited = true;
            break;
        }
        std::thread::sleep(GRACE_POLL_STEP);
        elapsed += GRACE_POLL_STEP;
    }
    let mut escalated = false;
    if !waited {
        supervisor.kill_force(process_id, expected_generation)?;
        escalated = true;
        exit_status = Some(child.wait().map_err(SupervisorError::Spawn)?);
    }

    let mut group_drained = false;
    for _ in 0..DRAIN_PROBE_ATTEMPTS {
        match supervisor.kill_force(process_id, expected_generation) {
            Ok(SupervisorSignalOutcome::AlreadyTerminated) => {
                group_drained = true;
                break;
            }
            Ok(SupervisorSignalOutcome::Applied) => {
                std::thread::sleep(DRAIN_PROBE_STEP);
            }
            Err(error) => return Err(error),
        }
    }

    Ok(ManagedKillReceipt {
        process_id,
        process_generation: expected_generation,
        os_pid,
        outcome,
        escalated,
        group_drained,
        exit_status,
    })
}

// KillScope 仅出现在 doc 链接中;保持导入用于文档不变量锚点。
const _: fn(KillScope) = |scope| {
    let _ = scope;
};
```

(注:末尾 `const _` 锚点若 clippy 报无用可改为在模块文档里以 `[`KillScope`]` 链接;实现者按 clippy 反馈二选一,删掉亦可。)

lib.rs:`mod` 区加 `pub mod managed;`(在 migration 之后、model 之前按字母序),re-export 区加:

```rust
pub use managed::{
    MANAGED_ENV_AUTH_ENDPOINT, MANAGED_ENV_PROCESS_IDENTITY_SEED,
    MANAGED_ENV_ROOT_CAPABILITY_SLOT, ManagedKillOutcome, ManagedKillReceipt, ManagedWorkerEnv,
    prepare_managed_command, spawn_managed_worker, terminate_group_with_escalation,
};
```

- [ ] **Step 3: 跑测试确认通过**

Run: `cargo test -p nlos-process 2>&1 | tail -8`
Expected: managed_env 6 测试 PASS + Task 2 的 5 测试 + 既有全 PASS。

- [ ] **Step 4: 提交**

```bash
cargo fmt -p nlos-process
git add crates/nlos-process/src/managed.rs crates/nlos-process/src/lib.rs \
  crates/nlos-process/tests/managed_env.rs crates/nlos-process/tests/fixtures/worker_env_dump.py
git diff --staged --stat
git -c user.name="cty12356541" -c user.email="171764500+cty12356541@users.noreply.github.com" commit -m "feat(nlos-process): 受管三变量白名单注入与升级式组回收 (W82-C)

ManagedWorkerEnv(env_clear+恰三 LLMOS_* 变量)、prepare_managed_command
(可跨平台 get_envs 单测)、spawn_managed_worker(分组拉起)、
terminate_group_with_escalation(组 SIGTERM→宽限→组 SIGKILL→有界排空核验,
at-least-once 回执)。验证:cargo test -p nlos-process 新 6 + 全量绿(macOS,
含 python3 门字面测试)。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: nlos-up `workers` 子命令端到端

**Files:**
- Modify: `crates/nlos-up/Cargo.toml`(deps 加 nlos-package/nlos-process 路径依赖)
- Modify: `crates/nlos-up/src/lib.rs`(`UpError::ManagedWorker` 变体 + `derive_managed_worker_process_id` + `resolve_program_path`)
- Modify: `crates/nlos-up/src/main.rs`(USAGE / Command 枚举 / parse_cli_args / finalize / dispatch / `run_workers`)
- Create: `crates/nlos-up/tests/workers_e2e.rs`(`#![cfg(unix)]`)
- Create: `crates/nlos-up/tests/fixtures/worker_env_dump.sh`(Task 2/3 同名夹具副本,按任务书"夹具放本 crate tests/ 下")
- Modify: `Cargo.lock`(机械再生,随提交)

**Interfaces:**
- Consumes: Task 1 `read_managed_worker_manifest`;Task 2 `SpawnSupervisedRequest`/`ProcessSupervisor`;Task 3 `ManagedWorkerEnv`/`spawn_managed_worker`/`terminate_group_with_escalation`;既有 `bootstrap_trial_root`/`assemble`/`session_env_from_live_daemon`/`wait_for_shutdown_signal`/`shutdown_embedded_daemon`/`print_daemon_ready`
- Produces: CLI `nlos-up workers --root <DIR> --workers-manifest <PATH> [--grace-ms <N>]`;lib 新公开 `resolve_program_path(program: &str, path_env: Option<&str>) -> Option<PathBuf>`、`derive_managed_worker_process_id(manifest_path: &Path, unit_name: &str) -> ProcessId`、`UpError::ManagedWorker(String)`

- [ ] **Step 1: 写失败测试 `crates/nlos-up/tests/workers_e2e.rs`**

```rust
//! W82-C 端到端:真实 `nlos-up workers` 二进制 + 双单元清单 + 三变量
//! 白名单 + SIGTERM 主进程 → 子进程树(含孙)干净回收。unix 门控。
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static SEQ: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "w82c-up-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        Self(dir)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
}

fn poll_file(path: &Path, seconds: u32) -> Option<String> {
    for _ in 0..(seconds * 20) {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn parse_env_dump(text: &str) -> std::collections::BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

fn pid_gone(pid: u32, seconds: u32) -> bool {
    for _ in 0..(seconds * 20) {
        let ok = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// 运行中的 nlos-up workers:后台线程全量收集 stdout(直到 EOF),
/// 主线程轮询关键行;SIGTERM 后 join 收全输出。
struct RunningUp {
    child: Child,
    stdout: std::sync::Arc<std::sync::Mutex<String>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl RunningUp {
    fn wait_for_line(&self, needle: &str, seconds: u32) -> String {
        for _ in 0..(seconds * 20) {
            if let Ok(buf) = self.stdout.lock() {
                if buf.contains(needle) {
                    return buf.clone();
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("nlos-up workers never printed {needle:?}:\n{}", self.snapshot());
    }

    fn snapshot(&self) -> String {
        self.stdout.lock().map(|buf| buf.clone()).unwrap_or_default()
    }

    fn sigterm_and_collect(mut self) -> (String, std::process::ExitStatus) {
        let pid = self.child.id();
        let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).status();
        let status = self.child.wait().expect("wait nlos-up exit");
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        (self.snapshot(), status)
    }
}

fn spawn_nlos_up_workers(root: &Path, manifest: &Path) -> RunningUp {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nlos-up"))
        .args([
            "workers",
            "--root",
            &root.display().to_string(),
            "--workers-manifest",
            &manifest.display().to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn nlos-up workers");
    let stdout = child.stdout.take().expect("piped stdout");
    let shared = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = std::sync::Arc::clone(&shared);
    let reader = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if let Ok(mut buf) = sink.lock() {
                        buf.push_str(&line);
                        buf.push('\n');
                    }
                }
                Err(_) => break,
            }
        }
    });
    RunningUp {
        child,
        stdout: shared,
        reader: Some(reader),
    }
}

#[test]
fn workers_launch_env_whitelist_and_sigterm_reclaims_tree() {
    let scratch = ScratchDir::new("e2e");
    let root = scratch.0.join("root");
    let manifest_path = scratch.0.join("workers.manifest");
    let dump_base = |name: &str| scratch.0.join(name);

    std::fs::write(
        &manifest_path,
        format!(
            "# W82-C e2e dual-unit manifest\n\
             worker = adapter /bin/sh {sh} {a}\n\
             slot = adapter adapter-slot-token\n\
             worker = brain /bin/sh {sh} {b}\n\
             slot = brain brain-slot-token\n",
            sh = fixture("worker_env_dump.sh").display(),
            a = dump_base("adapter").display(),
            b = dump_base("brain").display(),
        ),
    )
    .expect("write manifest");

    let up = spawn_nlos_up_workers(&root, &manifest_path);
    let stdout_head = up.wait_for_line("READY service=system_control", 60);
    assert!(stdout_head.contains("WORKERS manifest="), "{stdout_head}");
    assert!(stdout_head.contains("units=2"), "{stdout_head}");
    assert!(stdout_head.contains("WORKER name=adapter pid="), "{stdout_head}");
    assert!(stdout_head.contains("WORKER name=brain pid="), "{stdout_head}");

    for (unit, slot) in [("adapter", "adapter-slot-token"), ("brain", "brain-slot-token")] {
        let dump = poll_file(&dump_base(unit).with_extension("env"), 30)
            .unwrap_or_else(|| panic!("{unit} env dump missing"));
        let map = parse_env_dump(&dump);
        assert_eq!(
            map.keys().collect::<Vec<_>>(),
            [
                "LLMOS_AUTH_ENDPOINT",
                "LLMOS_PROCESS_IDENTITY_SEED",
                "LLMOS_ROOT_CAPABILITY_SLOT"
            ],
            "{unit} child env leaked: {map:?}"
        );
        assert_eq!(
            map.get("LLMOS_AUTH_ENDPOINT").unwrap(),
            &root.join("system-control-auth.sock").display().to_string()
        );
        assert_eq!(
            map.get("LLMOS_PROCESS_IDENTITY_SEED").unwrap(),
            &root.join("keys/operator-ed25519.key").display().to_string()
        );
        assert_eq!(map.get("LLMOS_ROOT_CAPABILITY_SLOT").unwrap(), slot);
    }

    // 主进程退出(SIGTERM)→ 子进程树(直子 + 孙)回收
    let (stdout, status) = up.sigterm_and_collect();
    assert!(stdout.contains("KILL name=adapter outcome="), "{stdout}");
    assert!(stdout.contains("KILL name=brain outcome="), "{stdout}");
    assert!(stdout.contains("STOPPED service=system_control"), "{stdout}");
    assert!(status.success(), "exit {:?}\n{stdout}", status.code());

    for unit in ["adapter", "brain"] {
        let pid: u32 = poll_file(&dump_base(unit).with_extension("pid"), 5)
            .unwrap_or_else(|| panic!("{unit} pid file missing"))
            .trim()
            .parse()
            .unwrap();
        let grandpid: u32 = poll_file(&dump_base(unit).with_extension("grandpid"), 5)
            .unwrap_or_else(|| panic!("{unit} grandpid file missing"))
            .trim()
            .parse()
            .unwrap();
        assert!(pid_gone(pid, 20), "worker {unit} pid {pid} survived main-exit reclaim");
        assert!(
            pid_gone(grandpid, 20),
            "grandchild of {unit} pid {grandpid} survived main-exit reclaim"
        );
    }
}

#[test]
fn workers_python_unit_env_whitelist_gate_letter() {
    let Some(python) = resolve_python3() else {
        panic!("python3 required for the W82-C gate-letter e2e (acceptance: 拉起外部 Python 组件)");
    };
    let scratch = ScratchDir::new("py");
    let root = scratch.0.join("root");
    let manifest_path = scratch.0.join("workers.manifest");
    std::fs::write(
        &manifest_path,
        format!(
            "worker = pyworker {py} {script} {base}\n",
            py = python.display(),
            script = fixture("worker_env_dump.py").display(),
            base = scratch.0.join("pyw").display(),
        ),
    )
    .expect("write manifest");

    let up = spawn_nlos_up_workers(&root, &manifest_path);
    let stdout_head = up.wait_for_line("READY service=system_control", 60);
    assert!(stdout_head.contains("WORKER name=pyworker"), "{stdout_head}");

    let dump = poll_file(&scratch.0.join("pyw.env"), 30).expect("python env dump");
    let map = parse_env_dump(&dump);
    assert_eq!(map.len(), 3, "python child env leaked: {map:?}");
    assert!(map.contains_key("LLMOS_AUTH_ENDPOINT"));

    let (_stdout, status) = up.sigterm_and_collect();
    assert!(status.success());
    let pid: u32 = poll_file(&scratch.0.join("pyw.pid"), 5).unwrap().trim().parse().unwrap();
    assert!(pid_gone(pid, 20), "python worker survived reclaim");
}

fn resolve_python3() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("python3"))
        .find(|candidate| candidate.is_file())
}
```

`crates/nlos-up/tests/fixtures/worker_env_dump.sh` 与 `worker_env_dump.py`:与 Task 3 的 `crates/nlos-process/tests/fixtures/` 两文件逐字节相同(按任务书,夹具放本 crate tests/ 下)。

Run: `cargo test -p nlos-up --test workers_e2e 2>&1 | tail -5`
Expected: 失败——`workers` 子命令不存在(usage 报错退出码 1,`wait_for_ready` panic 于 EOF)。

- [ ] **Step 2: Cargo.toml + lib.rs 助手**

`crates/nlos-up/Cargo.toml` `[dependencies]` 加:

```toml
nlos-package = { path = "../nlos-package" }
nlos-process = { path = "../nlos-process" }
```

`crates/nlos-up/src/lib.rs`:

(a) `UpError` 枚举加变体 + Display 臂(找到枚举与 `impl fmt::Display for UpError`,按既有风格):

```rust
    /// 受管 worker 拉起/回收链路失败(W82-C)。
    ManagedWorker(String),
```

```rust
            UpError::ManagedWorker(detail) => {
                write!(formatter, "managed worker failure: {detail}")
            }
```

(b) 文件尾部(既有测试模块之前)加:

```rust
/// 受管 worker `ProcessId` 派生域(W82-C):SHA-256(domain ‖ 清单路径 ‖
/// 单元名)前 16 字节,沿仓内 derive_16 惯例;每进程内唯一即可。
const MANAGED_WORKER_PROCESS_ID_DOMAIN: &[u8] = b"llmos/nlos-up/managed-worker-process-id/v1";

/// Derives the authority-shaped `ProcessId` for one managed worker unit.
#[must_use]
pub fn derive_managed_worker_process_id(manifest_path: &Path, unit_name: &str) -> ProcessId {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(MANAGED_WORKER_PROCESS_ID_DOMAIN);
    hasher.update(manifest_path.as_os_str().as_encoded_bytes());
    hasher.update(unit_name.as_bytes());
    let digest = hasher.finalize();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    ProcessId::from_bytes(id)
}

/// 父进程侧解释器解析:绝对路径原样;裸名沿 `path_env`(缺省取当前进程
/// PATH)搜首个存在文件;带分隔符的相对名拒绝(受管子进程环境无 PATH,
/// 解析必须在父侧完成)。Windows 侧补 `.exe`/`.cmd`/`.bat` 探测。
#[must_use]
pub fn resolve_program_path(program: &str, path_env: Option<&str>) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if candidate.is_absolute() {
        return Some(candidate.to_path_buf());
    }
    if program.contains('/') || program.contains('\\') {
        return None;
    }
    let owned;
    let path_value = match path_env {
        Some(value) => value,
        None => {
            owned = std::env::var("PATH").ok()?;
            &owned
        }
    };
    for dir in std::env::split_paths(path_value) {
        let mut names = vec![program.to_owned()];
        if cfg!(windows) {
            names.extend([
                format!("{program}.exe"),
                format!("{program}.cmd"),
                format!("{program}.bat"),
            ]);
        }
        for name in names {
            let joined = dir.join(name);
            if joined.is_file() {
                return Some(joined);
            }
        }
    }
    None
}
```

(`ProcessId` 已随 `nlos-types` 在 lib.rs 现有 use 中;若无需补 `use nlos_types::ProcessId;`。)

- [ ] **Step 3: main.rs 四点接线 + run_workers**

(a) `USAGE` 加行(desktop 行后):

```
  nlos-up workers [--root <DIR>] --workers-manifest <FILE> [--grace-ms <N>] [--keypool-config <FILE>] [--no-keypool-probe]
```

(b) `Command` 枚举加:

```rust
    Workers {
        options: BootstrapOptions,
        workers_manifest: PathBuf,
        grace_ms: u64,
    },
```

(c) `parse_cli_args` 局部变量区加 `let mut workers_manifest: Option<PathBuf> = None; let mut grace_ms: u64 = 3_000;`,flag 循环加两臂(在 `--dry-run` 臂后):

```rust
            "--workers-manifest" if subcommand == "workers" => {
                workers_manifest = Some(PathBuf::from(next_val()?));
            }
            "--grace-ms" if subcommand == "workers" => {
                let raw = next_val()?;
                grace_ms = raw.parse::<u64>().map_err(|_| {
                    UpError::InvalidArgument(format!(
                        "--grace-ms expects a positive integer, got '{raw}'"
                    ))
                })?;
                if grace_ms == 0 {
                    return Err(UpError::InvalidArgument(
                        "--grace-ms must be positive".to_owned(),
                    ));
                }
            }
```

并把两参传入 `finalize_parsed_subcommand`(签名加 `workers_manifest: Option<PathBuf>, grace_ms: u64`)。

(d) `finalize_parsed_subcommand` 加臂:

```rust
        "workers" => {
            let workers_manifest = workers_manifest.ok_or_else(|| {
                UpError::InvalidArgument(
                    "workers requires --workers-manifest <FILE>".to_owned(),
                )
            })?;
            Ok(ParseOutcome::Command(Command::Workers {
                options,
                workers_manifest,
                grace_ms,
            }))
        }
```

(e) `main()` dispatch 加臂:

```rust
        Command::Workers {
            options,
            workers_manifest,
            grace_ms,
        } => run_workers(&options, &workers_manifest, grace_ms).await,
```

(f) `run_desktop` 之后新增 `run_workers`(镜像其装配前奏;import 区补 `nlos_types::Generation`):

```rust
struct LaunchedWorker {
    name: String,
    process_id: nlos_types::ProcessId,
    spawn: nlos_process::SupervisedSpawn,
}

async fn run_workers(
    options: &BootstrapOptions,
    workers_manifest: &Path,
    grace_ms: u64,
) -> Result<(), UpError> {
    let manifest_dir = workers_manifest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let manifest = nlos_package::read_managed_worker_manifest(workers_manifest)
        .map_err(|error| UpError::InvalidArgument(format!("{error}")))?;
    println!(
        "WORKERS manifest={} units={}",
        workers_manifest.display(),
        manifest.units.len()
    );

    let receipt = bootstrap_trial_root(options)?;
    let (daemon, endpoints) = assemble(
        daemon_options_for_receipt(&receipt),
        tokio::runtime::Handle::current(),
    )
    .map_err(|error| UpError::DesktopProcess(format!("daemon assembly failed: {error}")))?;
    daemon
        .dispatch_command(&ControlCommand::InspectHealth, 1_000_000, 0)
        .map_err(|error| UpError::DesktopProcess(format!("daemon health check failed: {error}")))?;

    let stop = Arc::new(AtomicBool::new(false));
    let authenticated = tokio::spawn(serve_authenticated_endpoint(
        Arc::clone(&daemon),
        endpoints.listener_authenticated,
        Arc::clone(&stop),
    ));
    let plain = tokio::spawn(serve_plain_endpoint(
        Arc::clone(&daemon),
        endpoints.listener_plain,
        Arc::clone(&stop),
    ));

    let session_env = session_env_from_live_daemon(&daemon, &receipt);
    print_daemon_ready(&daemon, &receipt);

    // 三变量派生(spec §4.2):endpoint = 活守护认证 socket;seed = operator
    // 种子文件路径;slot = 清单字节透传(格式契约 TODO 对表 W82-B)。
    let supervisor = nlos_process::ProcessSupervisor::new();
    let mut launched: Vec<LaunchedWorker> = Vec::new();
    for unit in &manifest.units {
        let program = nlos_up::resolve_program_path(&unit.interpreter, None).ok_or_else(
            || {
                UpError::ManagedWorker(format!(
                    "worker {:?}: interpreter {:?} could not be resolved to an absolute path",
                    unit.name, unit.interpreter
                ))
            },
        )?;
        let cwd = match &unit.cwd {
            Some(relative) => manifest_dir.join(relative),
            None => manifest_dir.clone(),
        };
        let env = nlos_process::ManagedWorkerEnv {
            auth_endpoint: session_env.auth_socket_path.display().to_string(),
            process_identity_seed: session_env.key_file.display().to_string(),
            root_capability_slot: unit.root_capability_slot.clone(),
        };
        let process_id = derive_managed_worker_process_id(workers_manifest, &unit.name);
        let request = nlos_process::SpawnSupervisedRequest {
            process_id,
            process_generation: Generation::INITIAL,
            registered_at_ms: 0,
        };
        let spawn = nlos_process::spawn_managed_worker(
            &supervisor,
            request,
            program.to_str().ok_or_else(|| {
                UpError::ManagedWorker(format!(
                    "worker {:?}: interpreter path is not valid UTF-8",
                    unit.name
                ))
            })?,
            &unit.argv,
            &cwd,
            &env,
        )
        .map_err(|error| UpError::ManagedWorker(format!("worker {:?}: {error}", unit.name)))?;
        println!("WORKER name={} pid={} status=spawned", unit.name, spawn.os_pid());
        launched.push(LaunchedWorker {
            name: unit.name.clone(),
            process_id,
            spawn,
        });
    }
    println!("READY service=system_control workers={}", launched.len());

    // 监护:全部退出或关停信号(Ctrl-C / SIGTERM)即进入回收。
    loop {
        let mut any_alive = false;
        for worker in &mut launched {
            match worker.spawn.child().try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => any_alive = true,
                Err(error) => {
                    eprintln!("WORKER name={} poll_error={error}", worker.name);
                }
            }
        }
        if !any_alive {
            break;
        }
        tokio::select! {
            () = wait_for_shutdown_signal() => break,
            () = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
        }
    }

    // 回收:同步升级式 teardown(短阻塞 dev 工具语境可接受)。
    for worker in &mut launched {
        match nlos_process::terminate_group_with_escalation(
            &supervisor,
            worker.process_id,
            Generation::INITIAL,
            worker.spawn.child(),
            std::time::Duration::from_millis(grace_ms),
        ) {
            Ok(kill_receipt) => println!(
                "KILL name={} outcome={:?} escalated={} group_drained={}",
                worker.name, kill_receipt.outcome, kill_receipt.escalated, kill_receipt.group_drained
            ),
            Err(error) => println!(
                "KILL name={} outcome=error detail=\"{error}\"",
                worker.name
            ),
        }
    }

    shutdown_embedded_daemon(&daemon, &stop, authenticated, plain).await;
    println!("STOPPED service=system_control");
    Ok(())
}
```

- [ ] **Step 4: 跑 e2e 确认通过 + 既有测试不回归**

Run: `cargo test -p nlos-up 2>&1 | tail -8`
Expected: workers_e2e 2 测试 PASS + bootstrap_smoke 6 测试 PASS(dry-run env 断言不动、全绿)。

- [ ] **Step 5: 提交(含 Cargo.lock 机械再生)**

```bash
cargo fmt -p nlos-up
git add crates/nlos-up/Cargo.toml crates/nlos-up/src/lib.rs crates/nlos-up/src/main.rs \
  crates/nlos-up/tests/workers_e2e.rs crates/nlos-up/tests/fixtures/worker_env_dump.sh \
  crates/nlos-up/tests/fixtures/worker_env_dump.py Cargo.lock
git diff --staged --stat
git -c user.name="cty12356541" -c user.email="171764500+cty12356541@users.noreply.github.com" commit -m "feat(nlos-up): workers 子命令接线受管拉起与主进程退出树回收 (W82-C)

bootstrap+内嵌守护 → nlos-package 清单解析 → 父侧解释器解析 →
三变量白名单分组拉起 → SIGTERM/Ctrl-C 升级式组回收(KILL 回执行);
desktop 链路与 dry-run 断言零改动。验证:cargo test -p nlos-up
workers_e2e 2 + bootstrap_smoke 6 全绿(macOS;含 python3 门字面 e2e)。

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

(Cargo.lock 为路径依赖机械再生,越出三 crate 枚举但为接线必然产物——已在 PR 正文与回执显式标注。)

---

### Task 5: 全量验证门与收尾

**Files:** 无新文件(修复回涉前序任务的写集文件)

- [ ] **Step 1: 全量门**

```bash
cargo fmt --all -- --check
cargo test --workspace --no-fail-fast 2>&1 | tail -20
cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -20
```

Expected: fmt 零漂移;workspace 全绿(特别:nlos-process 既有 88+2、nlos-runtime-tokio kill_receipt 越集测试、nlos-up bootstrap_smoke);clippy 零警告(pedantic 全开)。任何失败按 systematic-debugging 处理后重跑,修复作为对应任务的追加提交。

- [ ] **Step 2: 提交前五查复核每笔提交**

```bash
git log origin/main..HEAD --oneline   # 无夹带
git diff --staged                     # 逐行
```

- [ ] **Step 3: push + PR**

```bash
git push -u origin claude/w82c-managed-process-462c8d
```

`gh pr create --base main` 正文:问题/根因/修复/验证数据表(实际命令+数字)/未运行项(Windows 实机、吊销联动留 INT、slot 格式待 W82-B 对表、docs/superpowers 与 Cargo.lock 越枚举标注)/验收门映射表。

---

## Self-Review 记录

- **Spec 覆盖**:§4.1 格式→Task 1;§4.2 三变量→Task 3(注入)+ Task 4(派生);§4.3 拉起协议→Task 4;§4.4 回收协议→Task 2+3;§6 平台矩阵→Task 5 验证声明;§7 四类测试→Task 1(类1解析)/Task 3(类2、3)/Task 4(类1拉起+类4);§2 冻结决策 1/2/3 全部落实(决策 3 = kill 接线不触 ProcessAuthority,Task 2 显式保持其不动)。无缺口。
- **占位符扫描**:唯一 TODO 是 spec 冻结决策 2 的既定标注(slot 格式),非计划占位。
- **类型一致性**:`KillScope` 定义(Task 2 supervisor_pid.rs)与消费(Task 2 supervisor.rs、Task 3 managed.rs doc)一致;`ManagedWorkerEnv`/`prepare_managed_command`/`terminate_group_with_escalation` 签名 Task 3 定义 = Task 4 消费;`MAX_WORKER_NAME_BYTES`、`read_managed_worker_manifest` Task 1 定义 = Task 4 消费;`resolve_program_path(program, path_env)` 双参在 Task 4 步骤内自洽。
