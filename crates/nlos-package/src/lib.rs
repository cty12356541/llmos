//! `nlos-package` 库面:受管外部 Worker 清单(`workers.manifest`)解析,
//! 供编排器(nlos-up `workers` 子命令)跨 crate 消费。只做解析,不做拉起;
//! CLI 仍在 bin 目标,本 lib 不引入新依赖。

pub mod managed_workers;

pub use managed_workers::{
    MAX_WORKER_NAME_BYTES, ManagedWorkerManifest, ManagedWorkerManifestError, ManagedWorkerUnit,
    parse_managed_worker_manifest, read_managed_worker_manifest,
};
