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
    for component in path.components() {
        if matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::CurDir
        ) {
            return Err("cwd must not contain `..` or `.` components".to_owned());
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
            vec![
                "adapter/main.py".to_owned(),
                "--port".to_owned(),
                "7901".to_owned()
            ]
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
        let err = read_managed_worker_manifest(std::path::Path::new(
            "/nonexistent-w82c/workers.manifest",
        ))
        .unwrap_err();
        assert!(format!("{err}").contains("/nonexistent-w82c"), "{err}");
    }
}
