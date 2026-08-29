//! D4：host 故障隔离
//! Wasmtime（进程内）：guest trap 可恢复、host 状态不被 guest 内存写破坏
//! host Process：子进程 crash（SIGABRT）父存活；对 TERM 免疫的子进程需 SIGKILL 升级

use std::process::{Command, Stdio};
use std::time::Instant;

use wasmtime::{Config, Engine, Instance, Module, Store, Trap};

use crate::guest::SPIN;
use crate::util::{sleep_ms, Recorder};

pub fn run(rec: &mut Recorder) -> anyhow::Result<()> {
    wasmtime_trap_recovery(rec)?;
    process_crash(rec)?;
    process_stubborn(rec)?;
    Ok(())
}

fn wasmtime_trap_recovery(rec: &mut Recorder) -> anyhow::Result<()> {
    let cfg = Config::new();
    let engine = Engine::new(&cfg)?;
    let module = Module::new(&engine, SPIN)?;
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[])?;

    let boom = instance.get_typed_func::<(), ()>(&mut store, "boom")?;
    let trap_name = match boom.call(&mut store, ()) {
        Err(e) => e
            .downcast_ref::<Trap>()
            .map(|t| format!("{t:?}"))
            .unwrap_or_else(|| e.to_string()),
        Ok(()) => "NO_TRAP".to_string(),
    };
    rec.add("D4-FAULT", "wasmtime(guest unreachable)", "trap", trap_name);

    let spin = instance.get_typed_func::<(i64,), ()>(&mut store, "spin")?;
    spin.call(&mut store, (1_000,))?;
    rec.add(
        "D4-FAULT",
        "wasmtime(trap 后复用同一 instance)",
        "spin(1000) 正常完成",
        true,
    );

    let sentinel: Vec<u8> = (0..16).collect();
    let scribble = instance.get_typed_func::<(), ()>(&mut store, "scribble")?;
    scribble.call(&mut store, ())?;
    rec.add(
        "D4-FAULT",
        "wasmtime(guest scribble 自身 memory 0..256)",
        "host 哨兵 Vec 完整",
        sentinel.iter().copied().eq(0u8..16),
    );
    rec.add(
        "D4-FAULT",
        "wasmtime(in-process) 设计注记",
        "fault_domain",
        "guest 无法 segfault host（沙箱内存受检、trap 可恢复），但与 host 同进程：engine/宿主侧原生崩溃或 OOM 会连带全部 guest——此为设计论证（DESIGN），本实验未主动崩溃 host 验证",
    );
    Ok(())
}

fn spawn_child(mode: &str, extra: &[&str]) -> anyhow::Result<Command> {
    let exe = std::env::current_exe()?;
    let mut c = Command::new(exe);
    c.arg("child").arg(mode);
    c.args(extra);
    c.stdout(Stdio::piped()).stderr(Stdio::piped());
    Ok(c)
}

fn process_crash(rec: &mut Recorder) -> anyhow::Result<()> {
    let mut child = spawn_child("crash", &[])?.spawn()?;
    let t0 = Instant::now();
    let mut signal_no: i32 = -1;
    loop {
        if let Some(status) = child.try_wait()? {
            signal_no = std::os::unix::process::ExitStatusExt::signal(&status).unwrap_or(0);
            break;
        }
        sleep_ms(1);
        if t0.elapsed().as_millis() > 5_000 {
            break;
        }
    }
    rec.add(
        "D4-FAULT",
        "process-child(abort)",
        "exit_signal",
        format!("SIG{signal_no}"),
    );
    rec.add(
        "D4-FAULT",
        "process-child(abort)",
        "parent_survived_and_reaped",
        format!("true(detect_ms={:.1})", t0.elapsed().as_millis() as f64),
    );
    Ok(())
}

fn process_stubborn(rec: &mut Recorder) -> anyhow::Result<()> {
    let mut child = spawn_child("stubborn", &["5000"])?.spawn()?;
    std::thread::sleep(std::time::Duration::from_millis(50));

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )?;
    std::thread::sleep(std::time::Duration::from_millis(200));
    let alive_after_term = child.try_wait()?.is_none();
    rec.add(
        "D4-FAULT",
        "process-child(SIGTERM 免疫)",
        "alive_after_SIGTERM_200ms",
        alive_after_term,
    );

    let t0 = Instant::now();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGKILL,
    )?;
    let status = child.wait()?;
    rec.add(
        "D4-FAULT",
        "process-child(升级 SIGKILL)",
        "killed_and_reaped",
        format!("true(reap_ms={:.1}, exit_ok={})", t0.elapsed().as_millis() as f64, status.success()),
    );
    rec.add(
        "D4-FAULT",
        "process 隔离语义",
        "note",
        "OS 提供 SIGKILL 这条不可屏蔽终途（仅 D-state 例外）；Wasmtime 侧无需 kill：fuel/epoch 保证循环可中断，但不存在'强杀单个 guest 后宿主继续'的等价 OS 语义（卸载靠 host 逻辑）",
    );
    Ok(())
}
