//! 共享工具：计时、子进程 rusage、errno 名表、结果行

use std::time::Duration;

use nix::sys::resource::{getrusage, UsageWho};
use nix::sys::time::TimeValLike;

#[derive(Clone)]
pub struct Row {
    pub dim: &'static str,
    pub scenario: String,
    pub metric: String,
    pub value: String,
}

pub struct Recorder {
    pub rows: Vec<Row>,
}

impl Recorder {
    pub fn new() -> Self {
        Self { rows: Vec::new() }
    }
    pub fn add(
        &mut self,
        dim: &'static str,
        scenario: &str,
        metric: &str,
        value: impl std::fmt::Display,
    ) {
        self.rows.push(Row {
            dim,
            scenario: scenario.to_string(),
            metric: metric.to_string(),
            value: value.to_string(),
        });
    }
}

pub fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn median(xs: &mut [f64]) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    xs[xs.len() / 2]
}

pub fn children_cpu_ms() -> i64 {
    let u = getrusage(UsageWho::RUSAGE_CHILDREN).expect("getrusage children");
    (u.user_time() + u.system_time()).num_milliseconds()
}

pub fn self_maxrss_kb() -> i64 {
    let u = getrusage(UsageWho::RUSAGE_SELF).expect("getrusage self");
    if cfg!(target_os = "macos") {
        u.max_rss() / 1024
    } else {
        u.max_rss()
    }
}

pub fn sleep_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

pub const CLOCK_TICK_MS: u64 = 1;

pub fn errno_name(code: i32) -> String {
    let name = match code {
        0 => "SUCCESS",
        1 => "E2BIG",
        2 => "EACCES",
        8 => "EBADF",
        9 => "EBADMSG",
        13 => "ECONNABORTED",
        21 => "EFAULT",
        28 => "EINVAL",
        36 => "ENAMETOOLONG",
        44 => "ENODEV",
        45 => "ENOENT",
        46 => "ENOEXEC",
        53 => "ENOSYS",
        55 => "ENOTDIR",
        56 => "ENOTEMPTY",
        58 => "ENOTSUP",
        61 => "EOVERFLOW",
        63 => "EPERM",
        64 => "EPIPE",
        74 => "EXDEV",
        75 => "ENOTCAPABLE",
        _ => "UNKNOWN",
    };
    format!("{code}({name})")
}
