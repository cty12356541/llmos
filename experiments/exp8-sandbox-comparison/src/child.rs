//! 子进程模式（父进程用 current_exe 自 spawn，避免外部依赖）：
//! cpu-spin | cpu-spin-rlimit-cpu | mem-alloc | mem-alloc-rlimit-as | probe | crash | stubborn

use std::io::Write as _;

use nix::sys::resource::{setrlimit, Resource};
use nix::sys::signal::{
    sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal,
};

pub fn run(args: &[String]) -> ! {
    let mode = args.first().map(String::as_str).unwrap_or("");
    match mode {
        "cpu-spin" => cpu_spin(),
        "cpu-spin-rlimit-cpu" => {
            let secs: u64 = args[1].parse().expect("secs");
            setrlimit(Resource::RLIMIT_CPU, secs, secs + 1).expect("setrlimit cpu");
            cpu_spin()
        }
        "mem-alloc" => mem_alloc(&args[1].parse().unwrap(), &args[2].parse().unwrap(), false, 0),
        "mem-alloc-rlimit-as" => {
            let (limit_mb, target_mb, chunk_mb): (u64, u64, u64) = (
                args[1].parse().unwrap(),
                args[2].parse().unwrap(),
                args[3].parse().unwrap(),
            );
            let bytes = limit_mb * 1024 * 1024;
            match setrlimit(Resource::RLIMIT_AS, bytes, bytes) {
                Ok(()) => println!("setrlimit_as=OK limit_mb={limit_mb}"),
                Err(e) => println!("setrlimit_as=ERR:{e} limit_mb={limit_mb}"),
            }
            let _ = std::io::stdout().flush();
            mem_alloc(&target_mb, &chunk_mb, true, limit_mb)
        }
        "probe" => probe(),
        "crash" => std::process::abort(),
        "stubborn" => {
            ignore_term();
            let ms: u64 = args[1].parse().unwrap();
            spin_for(ms);
            std::process::exit(0);
        }
        _ => {
            eprintln!("unknown child mode: {mode}");
            std::process::exit(2);
        }
    }
}

fn cpu_spin() -> ! {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    loop {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        std::hint::black_box(x);
    }
}

fn spin_for(ms: u64) {
    let start = std::time::Instant::now();
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    while start.elapsed().as_millis() < ms as u128 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        std::hint::black_box(x);
    }
}

fn mem_alloc(target_mb: &u64, chunk_mb: &u64, rlimited: bool, limit_mb: u64) -> ! {
    let chunk = chunk_mb * 1024 * 1024;
    let mut done: u64 = 0;
    let mut i: u64 = 0;
    while done < *target_mb {
        let buf = vec![0u8; chunk as usize];
        for b in buf.iter().step_by(4096) {
            std::hint::black_box(*b);
        }
        drop(buf);
        i += 1;
        done += chunk_mb;
        println!(
            "chunk={i} mb={done} maxrss_kb={}",
            crate::util::self_maxrss_kb()
        );
        let _ = std::io::stdout().flush();
    }
    println!(
        "result=reached_target rlimited={rlimited} limit_mb={limit_mb} maxrss_kb={}",
        crate::util::self_maxrss_kb()
    );
    std::process::exit(0);
}

fn probe() -> ! {
    println!("env_count={}", std::env::vars().count());
    println!("fd_count={}", std::fs::read_dir("/dev/fd").map(|d| d.count()).unwrap_or(9999));
    println!("etc_hosts={}", describe_open("/etc/hosts"));
    let home = std::env::var("HOME").unwrap_or_default();
    println!("home_stat={}", describe_open(&home));
    println!("socket_create={}", describe_socket());
    println!(
        "tcp_connect={}",
        match std::net::TcpStream::connect_timeout(
            &"127.0.0.1:1".parse().unwrap(),
            std::time::Duration::from_millis(300),
        ) {
            Ok(_) => "OK_CONNECTED",
            Err(e) => match e.kind() {
                std::io::ErrorKind::ConnectionRefused => "REFUSED(syscall permitted)",
                _ => return_print("ERR"),
            },
        }
    );
    let tmp = std::env::temp_dir().join("exp8-probe-write.txt");
    println!(
        "tmp_write={}",
        match std::fs::write(&tmp, b"probe") {
            Ok(()) => {
                let _ = std::fs::remove_file(&tmp);
                "OK".to_string()
            }
            Err(e) => format!("ERR:{e}"),
        }
    );
    println!("maxrss_kb={}", crate::util::self_maxrss_kb());
    return_print("done");
}

fn return_print(s: &'static str) -> ! {
    println!("probe_end={s}");
    std::process::exit(0);
}

fn describe_open(path: &str) -> String {
    match std::fs::metadata(path) {
        Ok(m) => format!("OK(size={})", m.len()),
        Err(e) => format!("ERR:{e}"),
    }
}

fn describe_socket() -> String {
    use std::net::TcpListener;
    match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => {
            drop(l);
            "OK".to_string()
        }
        Err(e) => format!("ERR:{e}"),
    }
}

/// 本文件唯一 unsafe 点：注册 SIG_IGN 使子进程对 SIGTERM 免疫，用于度量 SIGKILL 升级路径
#[allow(unsafe_code)]
fn ignore_term() {
    let action = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
    let _ = unsafe { sigaction(Signal::SIGTERM, &action) };
}
