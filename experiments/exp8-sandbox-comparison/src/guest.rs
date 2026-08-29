//! 手写 WAT guest（免 wasm 工具链依赖）：计算循环 / 内存增长 / trap / 宿主动作 / WASI 探针

pub const SPIN: &str = r#"
(module
  (memory (export "memory") 1)
  ;; 自旋：每轮把内存[0]处的 i64 计数器 +1，供 host 在 fuel/epoch 中断后精确读出已执行轮数
  (func (export "spin") (param $n i64)
    (local $i i64)
    (block $exit
      (loop $l
        (br_if $exit (i64.ge_u (local.get $i) (local.get $n)))
        (i64.store (i32.const 0)
          (i64.add (i64.load (i32.const 0)) (i64.const 1)))
        (local.set $i (i64.add (local.get $i) (i64.const 1)))
        (br $l))))
  (func (export "grow_to") (param $pages i32) (result i32)
    (memory.grow (local.get $pages)))
  (func (export "boom") (unreachable))
  (func (export "scribble")
    (local $p i32)
    (block $d
      (loop $w
        (br_if $d (i32.ge_u (local.get $p) (i32.const 256)))
        (i32.store8 (local.get $p) (i32.const 255))
        (local.set $p (i32.add (local.get $p) (i32.const 1)))
        (br $w))))
)
"#;

/// WASI preview1 探针：调用敏感 host 调用，把各自 errno 写到内存固定偏移
/// 结果布局：[8]=clock [12]=fd_write [16]=path_open(写) [20]=environ [24]=path_open(路径逃逸)
/// 数据布局：[40]=iovec{48,4} [48]="test" [64]="secret.txt" [80]="../../etc/hosts"
/// 辅助区：[104..132]（clock/nwritten/opened_fd/env 计数等 scratch）
pub const WASI_PROBE: &str = r#"
(module
  (import "wasi_snapshot_preview1" "clock_time_get"
    (func $clock (param i32 i64 i32) (result i32)))
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "path_open"
    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
  (import "wasi_snapshot_preview1" "environ_sizes_get"
    (func $env_sizes (param i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 40) "\30\00\00\00\04\00\00\00")
  (data (i32.const 48) "test")
  (data (i32.const 64) "secret.txt")
  (data (i32.const 80) "../../etc/hosts")
  (func (export "probe")
    (i32.store (i32.const 8)
      (call $clock (i32.const 1) (i64.const 1) (i32.const 104)))
    (i32.store (i32.const 12)
      (call $fd_write (i32.const 1) (i32.const 40) (i32.const 1) (i32.const 112)))
    (i32.store (i32.const 16)
      (call $path_open
        (i32.const 3) (i32.const 0) (i32.const 64) (i32.const 10)
        (i32.const 0) (i64.const 2) (i64.const 2) (i32.const 0) (i32.const 116)))
    (i32.store (i32.const 20)
      (call $env_sizes (i32.const 120) (i32.const 124)))
    (i32.store (i32.const 24)
      (call $path_open
        (i32.const 3) (i32.const 0) (i32.const 80) (i32.const 15)
        (i32.const 0) (i64.const 2) (i64.const 2) (i32.const 0) (i32.const 128))))
)
"#;
