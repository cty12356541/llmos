//! D3：capability 权限面收敛
//! Wasmtime/WASI：默认拒绝（无 preopen/env/stdio 时敏感调用返回 errno），显式授予后仅授部分可用
//! host Process：子进程默认继承完整 ambient authority（env/FD/文件/socket 全通）

use std::process::{Command, Stdio};

use wasmtime::{Engine, Instance, Linker, Module, Store};
use wasmtime_wasi::preview1::{self, WasiP1Ctx};
use wasmtime_wasi::WasiCtxBuilder;

use crate::guest::WASI_PROBE;
use crate::util::{errno_name, Recorder};

pub fn run(rec: &mut Recorder) -> anyhow::Result<()> {
    wasi_deny_all(rec)?;
    wasi_grant_partial(rec)?;
    process_probe(rec)?;
    Ok(())
}

fn instantiate(engine: &Engine, wasi: WasiP1Ctx) -> anyhow::Result<(Store<WasiP1Ctx>, Instance)> {
    let mut linker: Linker<WasiP1Ctx> = Linker::new(engine);
    preview1::add_to_linker_sync(&mut linker, |t| t)?;
    let module = Module::new(engine, WASI_PROBE)?;
    let mut store = Store::new(engine, wasi);
    let instance = linker.instantiate(&mut store, &module)?;
    Ok((store, instance))
}

fn run_probe(store: &mut Store<WasiP1Ctx>, instance: &Instance) -> anyhow::Result<[i32; 5]> {
    let probe = instance.get_typed_func::<(), ()>(&mut *store, "probe")?;
    probe.call(&mut *store, ())?;
    let mem = instance
        .get_memory(&mut *store, "memory")
        .expect("memory export");
    let mut buf = [0u8; 20];
    mem.read(&*store, 8, &mut buf)?;
    let get = |i: usize| i32::from_le_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap());
    Ok([get(0), get(1), get(2), get(3), get(4)])
}

fn wasi_deny_all(rec: &mut Recorder) -> anyhow::Result<()> {
    let mut cfg = wasmtime::Config::new();
    let engine = Engine::new(&mut cfg)?;
    let wasi = WasiCtxBuilder::new().build_p1();
    let (mut store, instance) = instantiate(&engine, wasi)?;
    let [clock, fdw, path_w, env, path_t] = run_probe(&mut store, &instance)?;
    let tag = "wasmtime-wasi(deny-all: 无 preopen/无显式 env；v36 实测默认含 stdout+clock)";
    rec.add("D3-CAP", tag, "clock_time_get", errno_name(clock));
    rec.add("D3-CAP", tag, "fd_write(stdout)", errno_name(fdw));
    rec.add("D3-CAP", tag, "path_open(无 preopen)", errno_name(path_w));
    rec.add("D3-CAP", tag, "environ_sizes_get(空集 0 变量)", errno_name(env));
    rec.add("D3-CAP", tag, "path_open(路径逃逸, 无 preopen)", errno_name(path_t));
    Ok(())
}

fn wasi_grant_partial(rec: &mut Recorder) -> anyhow::Result<()> {
    let mut cfg = wasmtime::Config::new();
    let engine = Engine::new(&mut cfg)?;
    let scratch = std::env::temp_dir().join("exp8-scratch-readonly");
    std::fs::create_dir_all(&scratch)?;
    std::fs::write(scratch.join("secret.txt"), b"x")?;
    let scratch_path = scratch.to_string_lossy().to_string();
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdout();
    builder.preopened_dir(
        scratch_path,
        "scratch",
        wasmtime_wasi::DirPerms::READ,
        wasmtime_wasi::FilePerms::READ,
    )?;
    let wasi = builder.build_p1();
    let (mut store, instance) = instantiate(&engine, wasi)?;
    let [clock, fdw, path_w, env, path_t] = run_probe(&mut store, &instance)?;
    let tag = "wasmtime-wasi(grant: stdout+只读 preopen scratch)";
    rec.add("D3-CAP", tag, "fd_write(stdout)", errno_name(fdw));
    rec.add(
        "D3-CAP",
        tag,
        "path_open(写打开只读 preopen 内的 secret.txt)",
        errno_name(path_w),
    );
    rec.add(
        "D3-CAP",
        tag,
        "path_open(路径逃逸 ../../etc/hosts)",
        errno_name(path_t),
    );
    rec.add("D3-CAP", tag, "clock_time_get", errno_name(clock));
    rec.add("D3-CAP", tag, "environ_sizes_get(空集 0 变量)", errno_name(env));
    Ok(())
}

fn process_probe(rec: &mut Recorder) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let out = Command::new(exe)
        .args(["child", "probe"])
        .stdout(Stdio::piped())
        .output()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let tag = "host-process(默认 spawn：完整 ambient authority)";
    for line in stdout.lines() {
        if line.starts_with("probe_end") {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            rec.add("D3-CAP", tag, k, v);
        }
    }
    rec.add(
        "D3-CAP",
        "对比口径",
        "wasmtime 敏感操作成功数（默认/显式授予）",
        "由上方 errno 判定；process 侧 env/fd/文件/socket/tmp-write 默认全部可用",
    );
    Ok(())
}
