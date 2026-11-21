//! `workers.manifest` —— 受管外部 Worker 拉起清单(W82-C 托管约定 C)。
//!
//! 行式格式,与 `package.manifest` 同纪律:`#` 注释与空行忽略;
//! `key = value` 两侧 trim,首个 `=` 切分。三种记录行:
//!
//! ```text
//! worker = <name> <interpreter> <argv...>   # argv 至少 1 个 token,原样透传
//! cwd  = <name> <dir>    # 可选;相对路径,相对清单目录;仅可引用已声明的 worker
//! slot = <name> <token>  # 可选;32 位小写十六进制(W82-B D7 16B CapabilityId);仅可引用已声明的 worker
//! ```
//!
//! 刻意独立于 `package.manifest`(不扩展包格式);`slot` 值严格遵循
//! W82-B D7 契约 `^[0-9a-f]{32}$`(未写 `slot` 行时为空字符串向后兼容)。

use std::fmt;
use std::path::{Path, PathBuf};

/// 受管 worker 名字段节数上限(与 nlos-artifact entry-name 常量同量级)。
pub const MAX_WORKER_NAME_BYTES: usize = 128;
const MAX_TOKEN_BYTES: usize = 4096;

/// 校验 `LLMOS_ROOT_CAPABILITY_SLOT` 值是否符合 W82-B D7 契约:
/// 严格 32 位小写 ASCII 十六进制字符(`^[0-9a-f]{32}$`,即 16 字节权威
/// `CapabilityId`,不含 slot 序号、generation 或分隔符)。
#[must_use]
pub fn is_valid_root_capability_slot(slot: &str) -> bool {
    slot.len() == 32
        && slot
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

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
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        line: usize,
        detail: String,
    },
}

impl fmt::Display for ManagedWorkerManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "read managed worker manifest {}: {source}",
                    path.display()
                )
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
///
/// # Errors
/// 文件不可读时返回 `Io`(带路径),清单违规时透传解析错误。
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
///
/// # Errors
/// 任一行违反格式契约(未知键、重复名、越界 cwd、非法解释器等)即返回
/// `Parse`(带行号);本函数无 I/O,不会产生 `Io` 变体。
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
                    ManagedWorkerManifestError::Parse {
                        line: line_no,
                        detail,
                    }
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
                    ManagedWorkerManifestError::Parse {
                        line: line_no,
                        detail,
                    }
                })?;
                validate_cwd(&dir).map_err(|detail| ManagedWorkerManifestError::Parse {
                    line: line_no,
                    detail,
                })?;
                let index = require_declared(&index_by_name, &name, "cwd").map_err(|detail| {
                    ManagedWorkerManifestError::Parse {
                        line: line_no,
                        detail,
                    }
                })?;
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
                    ManagedWorkerManifestError::Parse {
                        line: line_no,
                        detail,
                    }
                })?;
                let index = require_declared(&index_by_name, &name, "slot").map_err(|detail| {
                    ManagedWorkerManifestError::Parse {
                        line: line_no,
                        detail,
                    }
                })?;
                if !units[index].root_capability_slot.is_empty() {
                    return Err(ManagedWorkerManifestError::Parse {
                        line: line_no,
                        detail: format!("duplicate slot for worker {name:?}"),
                    });
                }
                if !is_valid_root_capability_slot(&token) {
                    return Err(ManagedWorkerManifestError::Parse {
                        line: line_no,
                        detail: format!(
                            "slot for worker {name:?} must be 32 lowercase hex characters (D7 CapabilityId), got {token:?}"
                        ),
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
        return Err(
            "worker = <name> <interpreter> <argv...> (argv needs at least one token)".to_owned(),
        );
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
    text.lines()
        .enumerate()
        .map(|(index, line)| {
            let trimmed = line.trim().to_owned();
            (index + 1, trimmed)
        })
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
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

#[cfg(test)]
mod tests {
    use super::*;

    const ADAPTER_SLOT: &str = "0123456789abcdef0123456789abcdef";
    const BRAIN_SLOT: &str = "fedcba9876543210fedcba9876543210";
    const QQ_AGENT: &str = "\
# qq-agent 受管双进程示例
worker = adapter python3 adapter/main.py --port 7901
slot = adapter 0123456789abcdef0123456789abcdef
worker = brain python3 brain/main.py --mode resident
slot = brain fedcba9876543210fedcba9876543210
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
            vec![
                "adapter/main.py".to_owned(),
                "--port".to_owned(),
                "7901".to_owned()
            ]
        );
        assert_eq!(adapter.cwd, None);
        assert_eq!(adapter.root_capability_slot, ADAPTER_SLOT);
        assert_eq!(manifest.units[1].name, "brain");
        assert_eq!(manifest.units[1].root_capability_slot, BRAIN_SLOT);
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
        let text =
            format!("worker = a /bin/sh a.sh\nslot = a {ADAPTER_SLOT}\nslot = a {BRAIN_SLOT}\n");
        let err = parse_managed_worker_manifest(&text).unwrap_err();
        assert!(format!("{err}").contains("duplicate slot"), "{err}");
    }

    #[test]
    fn accepts_valid_d7_slot_and_omitted_slot_compatibility() {
        assert!(is_valid_root_capability_slot(ADAPTER_SLOT));
        assert!(is_valid_root_capability_slot(&"0".repeat(32)));
        assert!(is_valid_root_capability_slot(&"f".repeat(32)));

        let omitted = parse_managed_worker_manifest("worker = a /bin/sh a.sh\n").expect("omitted");
        assert_eq!(omitted.units[0].root_capability_slot, "");
    }

    #[test]
    fn rejects_non_d7_root_capability_slots() {
        let invalid_cases = [
            "",
            "root-cap-placeholder",
            "0123456789ABCDEF0123456789ABCDEF",     // uppercase
            "0123456789abcdef0123456789abcdeF",     // mixed case
            "0123456789abcdef0123456789abcde",      // 31 chars (too short)
            "0123456789abcdef0123456789abcdef0",    // 33 chars (too long)
            "0123456789abcdef0123456789abcdeg",     // non-hex char 'g'
            "01234567-89ab-cdef-0123-456789abcdef", // hyphen separator
            "01234567:89ab:cdef:0123:456789abcdef", // colon separator
            "0x0123456789abcdef0123456789abcdef",   // 0x prefix
        ];
        for bad in invalid_cases {
            assert!(
                !is_valid_root_capability_slot(bad),
                "expected invalid slot {bad:?}"
            );
            if !bad.is_empty() {
                let text = format!("worker = a /bin/sh a.sh\nslot = a {bad}\n");
                let err = parse_managed_worker_manifest(&text).unwrap_err();
                assert!(
                    format!("{err}").contains("32 lowercase hex"),
                    "unexpected error for {bad:?}: {err}"
                );
            }
        }
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
        let err = read_managed_worker_manifest(std::path::Path::new(
            "/nonexistent-w82c/workers.manifest",
        ))
        .unwrap_err();
        assert!(format!("{err}").contains("/nonexistent-w82c"), "{err}");
    }
}
