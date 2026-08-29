//! exp8：Wasmtime/WASI vs 独立 host Process 隔离对比
//! 用法：cargo run --release [scenario 过滤，如 d1|d2|d3|d4|all]

mod child;
mod d1_cpu;
mod d2_mem;
mod d3_cap;
mod d4_fault;
mod guest;
mod report;
mod util;

use util::Recorder;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("child") {
        child::run(&args[1..]);
    }
    let filter = args.first().map(String::as_str).unwrap_or("all");
    println!("exp8 sandbox comparison | {}", report::platform_line());
    let mut rec = Recorder::new();
    let mut failed = false;
    if filter == "all" || filter == "d1" {
        if let Err(e) = d1_cpu::run(&mut rec) {
            eprintln!("d1 FAILED: {e:#}");
            failed = true;
        }
    }
    if filter == "all" || filter == "d2" {
        if let Err(e) = d2_mem::run(&mut rec) {
            eprintln!("d2 FAILED: {e:#}");
            failed = true;
        }
    }
    if filter == "all" || filter == "d3" {
        if let Err(e) = d3_cap::run(&mut rec) {
            eprintln!("d3 FAILED: {e:#}");
            failed = true;
        }
    }
    if filter == "all" || filter == "d4" {
        if let Err(e) = d4_fault::run(&mut rec) {
            eprintln!("d4 FAILED: {e:#}");
            failed = true;
        }
    }

    println!("\n==== 结果（{} 行）====", rec.rows.len());
    for r in &rec.rows {
        println!("[{}] {} | {} = {}", r.dim, r.scenario, r.metric, r.value);
    }
    if let Err(e) = report::emit(&rec) {
        eprintln!("report write FAILED: {e}");
        failed = true;
    }
    println!("\nresults 已写入 results/exp8-results.md 与 exp8-results.json");
    std::process::exit(i32::from(failed));
}
