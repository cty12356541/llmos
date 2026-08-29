//! 结果输出：markdown 表 + JSON（落到 exp8/results/），并打印环境指纹

use std::io::Write as _;

use crate::util::{now_ms, Recorder};

pub fn emit(rec: &Recorder) -> std::io::Result<()> {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let dir = std::path::Path::new(manifest).join("results");
    std::fs::create_dir_all(&dir)?;

    let mut md = String::new();
    md.push_str(&format!(
        "# exp8 原始结果（run at {} ms, {}）\n\n",
        now_ms(),
        platform_line()
    ));
    md.push_str("| 维度 | 场景 | 指标 | 值 |\n|---|---|---|---|\n");
    for r in &rec.rows {
        md.push_str(&format!(
            "| {} | {} | {} | {} |\n",
            r.dim, r.scenario, r.metric, r.value
        ));
    }
    let mut f = std::fs::File::create(dir.join("exp8-results.md"))?;
    f.write_all(md.as_bytes())?;

    let mut json = String::from("[\n");
    for (i, r) in rec.rows.iter().enumerate() {
        let comma = if i + 1 == rec.rows.len() { "" } else { "," };
        json.push_str(&format!(
            "  {{\"dim\": \"{}\", \"scenario\": \"{}\", \"metric\": \"{}\", \"value\": \"{}\"}}{comma}\n",
            escape(&r.dim),
            escape(&r.scenario),
            escape(&r.metric),
            escape(&r.value),
        ));
    }
    json.push_str("]\n");
    let mut f = std::fs::File::create(dir.join("exp8-results.json"))?;
    f.write_all(json.as_bytes())?;
    Ok(())
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn platform_line() -> String {
    format!(
        "os={} arch={} exp8={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        env!("CARGO_PKG_VERSION"),
    )
}
