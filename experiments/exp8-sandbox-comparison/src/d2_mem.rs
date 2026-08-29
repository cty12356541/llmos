//! D2：内存限制 enforcement
//! Wasmtime：Store ResourceLimiter（宿主施加、页粒度=64KiB、内联拒绝 memory.grow）
//! host Process：RLIMIT_AS（macOS 实测是否强制）+ 子进程自报 maxrss

use std::process::{Command, Stdio};

use wasmtime::{Config, Engine, Instance, Module, ResourceLimiter, Store};

use crate::guest::SPIN;
use crate::util::Recorder;

struct MaxBytes(usize);

impl ResourceLimiter for MaxBytes {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        Ok(desired <= self.0)
    }
    fn table_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmtime::Error> {
        Ok(desired <= 10_000)
    }
}

const PAGE: usize = 64 * 1024;

pub fn run(rec: &mut Recorder) -> anyhow::Result<()> {
    wasmtime_limiter(rec)?;
    process_rlimit_as(rec)?;
    Ok(())
}

fn grow_with_limit(max_pages: usize, pages: i32) -> anyhow::Result<i32> {
    let cfg = Config::new();
    let engine = Engine::new(&cfg)?;
    let module = Module::new(&engine, SPIN)?;
    let mut store = Store::new(&engine, MaxBytes(max_pages * PAGE));
    store.limiter(|l: &mut MaxBytes| l);
    let instance = Instance::new(&mut store, &module, &[])?;
    let grow = instance.get_typed_func::<(i32,), i32>(&mut store, "grow_to")?;
    Ok(grow.call(&mut store, (pages,))?)
}

fn wasmtime_limiter(rec: &mut Recorder) -> anyhow::Result<()> {
    let denied = grow_with_limit(10, 100)?;
    rec.add(
        "D2-MEM",
        "wasmtime-limiter(max=10 pages)",
        "grow_to(100) denied(ret=-1)",
        denied == -1,
    );

    let ok_pages = grow_with_limit(6, 5)?;
    let then_denied = {
        let cfg = Config::new();
        let engine = Engine::new(&cfg)?;
        let module = Module::new(&engine, SPIN)?;
        let mut store = Store::new(&engine, MaxBytes(6 * PAGE));
        store.limiter(|l: &mut MaxBytes| l);
        let instance = Instance::new(&mut store, &module, &[])?;
        let grow = instance.get_typed_func::<(i32,), i32>(&mut store, "grow_to")?;
        let _ = grow.call(&mut store, (5,))?;
        let second = grow.call(&mut store, (1,))?;
        second == -1
    };
    rec.add(
        "D2-MEM",
        "wasmtime-limiter(max=6 pages)",
        &format!("grow_to(5)=ok({ok_pages}>=0) then grow_to(1)=denied"),
        then_denied,
    );
    rec.add(
        "D2-MEM",
        "wasmtime-limiter",
        "granularity",
        format!("1 wasm page = {} KiB，拒绝发生在 memory.grow 调用点（内联、确定性）", PAGE / 1024),
    );
    Ok(())
}

fn spawn_mem_child(rlimit_mb: Option<u64>, target_mb: u64, chunk_mb: u64) -> anyhow::Result<Command> {
    let exe = std::env::current_exe()?;
    let mut c = Command::new(exe);
    match rlimit_mb {
        Some(limit) => {
            c.args([
                "child",
                "mem-alloc-rlimit-as",
                &limit.to_string(),
                &target_mb.to_string(),
                &chunk_mb.to_string(),
            ]);
        }
        None => {
            c.args(["child", "mem-alloc", &target_mb.to_string(), &chunk_mb.to_string()]);
        }
    }
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    Ok(c)
}

fn run_mem_child(rec: &mut Recorder, tag: &str, cmd: &mut Command) -> anyhow::Result<()> {
    let out = cmd.output()?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr_head: String = String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|l| !l.is_empty())
        .take(2)
        .collect::<Vec<_>>()
        .join(" | ");
    let setrlimit_line = stdout
        .lines()
        .find(|l| l.starts_with("setrlimit_as="))
        .unwrap_or("setrlimit_as=absent")
        .to_string();
    let last_chunk = stdout
        .lines()
        .filter(|l| l.starts_with("chunk="))
        .last()
        .unwrap_or("chunk=0 mb=0 maxrss_kb=?")
        .to_string();
    let reached = stdout.contains("result=reached_target");
    let signal = std::os::unix::process::ExitStatusExt::signal(&out.status);
    rec.add(
        "D2-MEM",
        tag,
        "exit(ok/signal/abort)",
        if out.status.success() {
            "ok".to_string()
        } else if let Some(s) = signal {
            format!("SIG{s}")
        } else {
            "abort(alloc_err/panic)".to_string()
        },
    );
    rec.add("D2-MEM", tag, "reached_target", reached);
    rec.add("D2-MEM", tag, "setrlimit", setrlimit_line);
    rec.add("D2-MEM", tag, "last_progress", last_chunk);
    if !stderr_head.is_empty() {
        rec.add("D2-MEM", tag, "stderr_head", stderr_head);
    }
    Ok(())
}

fn process_rlimit_as(rec: &mut Recorder) -> anyhow::Result<()> {
    let mut baseline = spawn_mem_child(None, 256, 16)?;
    run_mem_child(rec, "process-baseline(no rlimit, target 256MB)", &mut baseline)?;

    // 低位限制：低于 macOS 进程自身 VA 常驻（dyld shared cache 等）的临界点
    let mut low = spawn_mem_child(Some(64), 256, 16)?;
    run_mem_child(rec, "process-rlimit-as-LOW(64MB, target 256MB)", &mut low)?;

    // 高位限制：子进程可正常启动后，观察超限分配是否被拒
    let mut high = spawn_mem_child(Some(2048), 4096, 256)?;
    run_mem_child(rec, "process-rlimit-as-HIGH(2GB, target 4GB)", &mut high)?;

    rec.add(
        "D2-MEM",
        "process-rlimit-as",
        "platform_analysis",
        "实测：macOS(arm64) 上 setrlimit(RLIMIT_AS) 直接返回 EINVAL（任何值都无法设置），子进程随后无限制分配至 256MB/4GB 目标——macOS 上 host-process 侧不存在可用的内核级内存配额原语（RLIMIT_RSS 亦为历史遗留 no-op），需 Mach task limit 等外部机制（未测）。对比 wasmtime limiter：64KiB 页粒度、仅约束 guest 线性内存、在 memory.grow 调用点内联确定性拒绝、跨平台一致。Linux 的 RLIMIT_AS 在 mmap 路径生效（DESIGN 引用，本实验未实测 Linux）",
    );
    Ok(())
}
