//! D1：CPU 限制生效延迟与精度
//! Wasmtime：fuel（指令粒度、确定性）与 epoch（tick 粒度、异步中断）
//! host Process：轮询 + SIGKILL（poll 粒度 + 调度延迟）与 RLIMIT_CPU（1 秒量子）

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wasmtime::{Config, Engine, Instance, Module, Store, Trap};

use crate::guest::SPIN;
use crate::util::{children_cpu_ms, median, sleep_ms, Recorder, CLOCK_TICK_MS};

pub fn run(rec: &mut Recorder) -> anyhow::Result<()> {
    eprintln!("d1: fuel_precision...");
    fuel_precision(rec)?;
    eprintln!("d1: fuel_overhead...");
    fuel_overhead(rec)?;
    eprintln!("d1: epoch_deadline...");
    epoch_deadline(rec)?;
    eprintln!("d1: process_poll_kill...");
    process_poll_kill(rec)?;
    eprintln!("d1: process_rlimit_cpu...");
    process_rlimit_cpu(rec)?;
    eprintln!("d1: done");
    Ok(())
}

fn fuel_engine() -> anyhow::Result<Engine> {
    let mut cfg = Config::new();
    cfg.consume_fuel(true);
    Ok(Engine::new(&cfg)?)
}

fn read_counter(store: &mut Store<()>, instance: &Instance) -> anyhow::Result<i64> {
    let mem = instance.get_memory(&mut *store, "memory").expect("memory export");
    let mut buf = [0u8; 8];
    mem.read(&*store, 0, &mut buf)?;
    Ok(i64::from_le_bytes(buf))
}

fn fuel_scenario(engine: &Engine, fuel: u64, n: i64) -> anyhow::Result<(bool, i64)> {
    let module = Module::new(engine, SPIN)?;
    let mut store = Store::new(engine, ());
    store.set_fuel(fuel)?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let spin = instance.get_typed_func::<(i64,), ()>(&mut store, "spin")?;
    let out_of_fuel = match spin.call(&mut store, (n,)) {
        Err(e) => matches!(e.downcast_ref::<Trap>(), Some(Trap::OutOfFuel)) || e.to_string().contains("fuel"),
        Ok(()) => false,
    };
    let iters = read_counter(&mut store, &instance)?;
    Ok((out_of_fuel, iters))
}

fn fuel_precision(rec: &mut Recorder) -> anyhow::Result<()> {
    let engine = fuel_engine()?;
    for fuel in [1_000u64, 10_000, 100_000] {
        let (t1, i1) = fuel_scenario(&engine, fuel, 1_000_000_000)?;
        let (_, i2) = fuel_scenario(&engine, fuel, 1_000_000_000)?;
        rec.add(
            "D1-CPU",
            &format!("wasmtime-fuel({fuel})"),
            "trap=OutOfFuel",
            t1,
        );
        rec.add("D1-CPU", &format!("wasmtime-fuel({fuel})"), "iters_run1", i1);
        rec.add(
            "D1-CPU",
            &format!("wasmtime-fuel({fuel})"),
            "deterministic(run1==run2)",
            i1 == i2,
        );
    }
    Ok(())
}

fn fuel_overhead(rec: &mut Recorder) -> anyhow::Result<()> {
    let n: i64 = 20_000_000;
    let reps = 5;

    let mut cfg = Config::new();
    let engine_u = Engine::new(&cfg)?;
    let module_u = Module::new(&engine_u, SPIN)?;
    let mut store_u = Store::new(&engine_u, ());
    let instance_u = Instance::new(&mut store_u, &module_u, &[])?;
    let spin_u = instance_u.get_typed_func::<(i64,), ()>(&mut store_u, "spin")?;
    spin_u.call(&mut store_u, (1_000_000,))?;
    let mut un_samples = vec![0.0f64; reps];
    for s in un_samples.iter_mut() {
        let t0 = Instant::now();
        spin_u.call(&mut store_u, (n,))?;
        *s = t0.elapsed().as_secs_f64() * 1000.0;
    }

    let engine_m = fuel_engine()?;
    let module_m = Module::new(&engine_m, SPIN)?;
    let mut store_m = Store::new(&engine_m, ());
    store_m.set_fuel(u64::MAX / 4)?;
    let instance_m = Instance::new(&mut store_m, &module_m, &[])?;
    let spin_m = instance_m.get_typed_func::<(i64,), ()>(&mut store_m, "spin")?;
    spin_m.call(&mut store_m, (1_000_000,))?;
    let mut m_samples = vec![0.0f64; reps];
    for s in m_samples.iter_mut() {
        let t1 = Instant::now();
        spin_m.call(&mut store_m, (n,))?;
        *s = t1.elapsed().as_secs_f64() * 1000.0;
    }

    let un = median(&mut un_samples);
    let mm = median(&mut m_samples);
    rec.add(
        "D1-CPU",
        "wasmtime-fuel-overhead(20M iters,5rep)",
        "metered_samples_ms",
        format!("{m_samples:?}"),
    );
    rec.add(
        "D1-CPU",
        "wasmtime-fuel-overhead(20M iters,5rep)",
        "unmetered_samples_ms",
        format!("{un_samples:?}"),
    );
    rec.add(
        "D1-CPU",
        "wasmtime-fuel-overhead",
        "median_metered/unmetered/overhead_pct",
        format!(
            "{mm:.1}/{un:.1}/{:.1} (MEASURED-UNSTABLE：跨次运行样本方差大于效应本身，本机噪声下不能给出可靠 fuel 开销百分比，仅可确认量级为同数量级)",
            (mm - un) / un * 100.0
        ),
    );
    Ok(())
}

fn epoch_deadline(rec: &mut Recorder) -> anyhow::Result<()> {
    let mut cfg = Config::new();
    cfg.epoch_interruption(true);
    let engine = Engine::new(&cfg)?;
    let module = Module::new(&engine, SPIN)?;

    // 校准：名义 1ms tick 在 macOS 上受 timer coalescing 影响实际更长
    let eff_tick_ms = {
        let cal_engine = engine.clone();
        let t0 = Instant::now();
        let mut n = 0u64;
        while t0.elapsed() < Duration::from_millis(300) {
            std::thread::sleep(Duration::from_millis(CLOCK_TICK_MS));
            cal_engine.increment_epoch();
            n += 1;
        }
        let eff = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
        rec.add(
            "D1-CPU",
            &format!("epoch-tick-calibration(nominal={CLOCK_TICK_MS}ms,300ms window)"),
            "effective_tick_ms",
            format!("{eff:.3}"),
        );
        eff
    };

    for deadline_ms in [10u64, 50, 100] {
        let mut samples = vec![0.0f64; 3];
        for s in samples.iter_mut() {
            let stop = Arc::new(AtomicBool::new(false));
            let ticker_engine = engine.clone();
            let stop2 = stop.clone();
            let ticker = std::thread::spawn(move || {
                while !stop2.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(CLOCK_TICK_MS));
                    ticker_engine.increment_epoch();
                }
            });
            let mut store = Store::new(&engine, ());
            store.set_epoch_deadline(deadline_ms);
            let instance = Instance::new(&mut store, &module, &[])?;
            let spin = instance.get_typed_func::<(i64,), ()>(&mut store, "spin")?;
            let t0 = Instant::now();
            let trapped = match spin.call(&mut store, (1_000_000_000,)) {
                Err(e) => {
                    matches!(e.downcast_ref::<Trap>(), Some(Trap::Interrupt)) || e.to_string().contains("epoch")
                }
                Ok(()) => false,
            };
            let elapsed = t0.elapsed().as_secs_f64() * 1000.0;
            *s = elapsed;
            rec.add(
                "D1-CPU",
                &format!("wasmtime-epoch(deadline={deadline_ms}ms,tick={CLOCK_TICK_MS}ms)"),
                "trapped_by_epoch",
                trapped,
            );
            stop.store(true, Ordering::Relaxed);
            let _ = ticker.join();
        }
        let med = median(&mut samples);
        let expected_by_eff = deadline_ms as f64 * eff_tick_ms;
        rec.add(
            "D1-CPU",
            &format!("wasmtime-epoch(deadline={deadline_ms}ms)"),
            "elapsed_ms_median/overshoot_vs_nominal_ms/overshoot_vs_eff_tick_ms",
            format!(
                "{med:.2}/{:.2}/{:.2}",
                med - deadline_ms as f64,
                med - expected_by_eff
            ),
        );
    }
    Ok(())
}

fn spawn_cpu_spin(rlimit_cpu_secs: Option<u64>) -> anyhow::Result<Command> {
    let exe = std::env::current_exe()?;
    let mut c = Command::new(exe);
    match rlimit_cpu_secs {
        Some(secs) => {
            c.args(["child", "cpu-spin-rlimit-cpu", &secs.to_string()]);
        }
        None => {
            c.args(["child", "cpu-spin"]);
        }
    }
    c.stdout(Stdio::null()).stderr(Stdio::null());
    Ok(c)
}

fn process_poll_kill(rec: &mut Recorder) -> anyhow::Result<()> {
    for deadline_ms in [10u64, 50, 100] {
        let mut overshoots = vec![0.0f64; 3];
        for o in overshoots.iter_mut() {
            let before = children_cpu_ms();
            let mut child = spawn_cpu_spin(None)?.spawn()?;
            let t0 = Instant::now();
            loop {
                if t0.elapsed() >= Duration::from_millis(deadline_ms) {
                    break;
                }
                sleep_ms(CLOCK_TICK_MS);
            }
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGKILL,
            )?;
            let status = child.wait()?;
            let after = children_cpu_ms();
            let actual = (after - before) as f64;
            *o = actual - deadline_ms as f64;
            debug_assert!(!status.success());
        }
        let med = median(&mut overshoots);
        rec.add(
            "D1-CPU",
            &format!("process-kill(deadline={deadline_ms}ms,poll={CLOCK_TICK_MS}ms,SIGKILL,3rep)"),
            "cpu_overshoot_ms_median(min/max见json)",
            format!("{med:.2}"),
        );
        rec.add(
            "D1-CPU",
            &format!("process-kill(deadline={deadline_ms}ms)"),
            "cpu_overshoot_all_ms",
            format!("{overshoots:?}"),
        );
    }
    Ok(())
}

fn process_rlimit_cpu(rec: &mut Recorder) -> anyhow::Result<()> {
    let before = children_cpu_ms();
    let mut child = spawn_cpu_spin(Some(2))?.spawn()?;
    let t0 = Instant::now();
    let status = child.wait()?;
    let wall = t0.elapsed().as_secs_f64();
    let cpu = (children_cpu_ms() - before) as f64 / 1000.0;
    let terminated = !status.success();
    rec.add(
        "D1-CPU",
        "process-rlimit-cpu(soft=2s,quantum=1s)",
        "actual_cpu_s/wall_s/terminated",
        format!("{cpu:.3}/{wall:.3}/{terminated}"),
    );
    rec.add(
        "D1-CPU",
        "process-rlimit-cpu",
        "note",
        "RLIMIT_CPU 以秒为量子，无法表达毫秒级配额；macOS/Linux 同为 1s 粒度",
    );
    Ok(())
}
