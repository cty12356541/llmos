"""Observe one unchanged Linux libtest benchmark with read-only kernel counters."""

import json
import os
from collections import Counter
from pathlib import Path
import resource
import selectors
import subprocess
import sys
import time

TARGET = "c_bench_task_sharded_scale"
BENCHMARK = "road_c002_sharded_spawn_message_commit_scales_without_global_lock_or_total_order"
INTERVAL_NS = 200_000_000
PROC = Path("/proc")
CGROUP_FIELDS = (
    "cpu.max", "cpu.stat", "cpu.pressure", "cpuset.cpus.effective",
    "io.max", "io.stat", "io.pressure", "memory.max", "memory.current",
    "memory.events", "memory.pressure",
)
STATUS_KEYS = (
    "Threads:", "VmRSS:", "Cpus_allowed_list:", "voluntary_ctxt_switches:",
    "nonvoluntary_ctxt_switches:",
)
VMSTAT_KEYS = ("pgmajfault", "pswpin", "pswpout", "pgscan_", "pgsteal_")
READ_FAILURES = Counter()


def read_kernel(path):
    try:
        return path.read_text().rstrip()
    except OSError as exc:
        # Exited threads and unavailable counters are recorded, including their errno.
        READ_FAILURES[f"{path.name}:errno={exc.errno}"] += 1
        return None


def matching_lines(value, prefixes):
    if value is None:
        return None
    return "\n".join(line for line in value.splitlines() if line.startswith(prefixes))


def cgroup_ancestors(pid):
    membership = read_kernel(PROC / str(pid) / "cgroup") or ""
    group_path = next((line[3:] for line in membership.splitlines() if line.startswith("0::")), None)
    mountinfo = read_kernel(PROC / str(pid) / "mountinfo") or ""
    for line in mountinfo.splitlines():
        fields, _, filesystem = line.partition(" - ")
        if not filesystem.startswith("cgroup2 ") or group_path is None:
            continue
        root, mount = map(Path, fields.split()[3:5])
        if Path(group_path).is_relative_to(root):
            relative = Path(group_path).relative_to(root)
            return [mount / part for part in (relative, *relative.parents)]
    return []


def process_tree(root_pid):
    pending = [root_pid] if root_pid is not None else []
    processes = {}
    while pending:
        pid = pending.pop()
        if pid in processes:
            continue
        directory = PROC / str(pid)
        try:
            threads = sorted((directory / "task").iterdir())
        except OSError as exc:
            READ_FAILURES[f"task:errno={exc.errno}"] += 1
            continue
        processes[pid] = {
            "stat": read_kernel(directory / "stat"),
            "io": read_kernel(directory / "io"),
            "status": matching_lines(read_kernel(directory / "status"), STATUS_KEYS),
            "threads": {
                thread.name: {field: read_kernel(thread / field) for field in ("stat", "schedstat", "wchan")}
                for thread in threads
            },
        }
        # libtest spawns shard workers from the test thread, not the initial thread.
        for thread in threads:
            pending.extend(map(int, (read_kernel(thread / "children") or "").split()))
    return processes


def sample(pid, groups, sequence):
    started = time.monotonic_ns()
    result = {
        "sequence": sequence,
        "monotonic_ns": started,
        "wall_ns": time.time_ns(),
        "proc_stat": read_kernel(PROC / "stat"),
        "diskstats": read_kernel(PROC / "diskstats"),
        "vmstat": matching_lines(read_kernel(PROC / "vmstat"), VMSTAT_KEYS),
        "meminfo": matching_lines(read_kernel(PROC / "meminfo"), ("MemAvailable:", "Dirty:", "Writeback:")),
        "pressure": {kind: read_kernel(PROC / "pressure" / kind) for kind in ("cpu", "io", "memory")},
        "cgroups": {str(group): {field: read_kernel(group / field) for field in CGROUP_FIELDS} for group in groups},
        "processes": process_tree(pid),
    }
    result["sample_ns"] = time.monotonic_ns() - started
    return result


def find_test_executable(build_log, package, output):
    expected = package / "tests" / f"{TARGET}.rs"
    artifacts = []
    with build_log.open(encoding="utf-8") as records:
        for line in records:
            message = json.loads(line)
            if message.get("reason") != "compiler-artifact" or not message.get("executable"):
                continue
            if Path(message["target"]["src_path"]) == expected:
                artifacts.append(message)
    if len(artifacts) != 1:
        raise RuntimeError(f"Expected exactly one {TARGET} compiler artifact, found {len(artifacts)}")
    artifact, = artifacts
    save_json(output / "test-artifact.json", artifact)
    profile = artifact["profile"]
    if not profile["test"] or profile["opt_level"] != "0":
        raise RuntimeError(f"Expected the unoptimized test profile, got {profile}")
    return Path(artifact["executable"])


def save_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def observe(argv, cwd, env, output):
    groups = cgroup_ancestors(os.getpid())
    samples = [sample(None, groups, -1)]
    events = []
    streams = {"stdout": bytearray(), "stderr": bytearray()}
    sampler_cpu_start = time.process_time_ns()
    started = time.monotonic_ns()
    missed = 0
    deadline = started
    with subprocess.Popen(argv, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE) as child:
        child_cgroup = read_kernel(PROC / str(child.pid) / "cgroup")
        child_groups = cgroup_ancestors(child.pid)
        if child_groups:
            groups = child_groups
        child_comm = read_kernel(PROC / str(child.pid) / "comm")
        sequence = 0
        with selectors.DefaultSelector() as pipes:
            pipes.register(child.stdout, selectors.EVENT_READ, "stdout")
            pipes.register(child.stderr, selectors.EVENT_READ, "stderr")
            while pipes.get_map() or child.poll() is None:
                now = time.monotonic_ns()
                if now >= deadline:
                    skipped = (now - deadline) // INTERVAL_NS
                    missed += skipped
                    sequence += skipped
                    samples.append(sample(child.pid, groups, sequence))
                    sequence += 1
                    deadline += (skipped + 1) * INTERVAL_NS
                timeout = max(0, deadline - time.monotonic_ns()) / 1_000_000_000
                for key, _ in pipes.select(timeout):
                    data = os.read(key.fd, 65536)
                    if not data:
                        pipes.unregister(key.fileobj)
                        continue
                    stream = key.data
                    events.append({
                        "monotonic_ns": time.monotonic_ns(), "wall_ns": time.time_ns(),
                        "stream": stream, "offset_bytes": len(streams[stream]), "length_bytes": len(data),
                    })
                    streams[stream].extend(data)
        result = child.wait()
    elapsed = time.monotonic_ns() - started
    sampler_cpu = time.process_time_ns() - sampler_cpu_start
    samples.append(sample(None, groups, sequence))
    save_json(output / "result.json", {
        "state": "test-exited", "argv": argv, "cwd": str(cwd), "returncode": result,
        "elapsed_ns": elapsed, "test_pid": child.pid, "test_comm": child_comm,
        "test_cgroup": child_cgroup, "cgroup_ancestors": list(map(str, groups)),
        "sched_schedstats": read_kernel(PROC / "sys/kernel/sched_schedstats"),
        "sample_interval_ns": INTERVAL_NS, "missed_intervals": missed, "samples": len(samples),
        "sampler_pid": os.getpid(), "sampler_cpu_ns": sampler_cpu,
        "sampler_peak_rss_kib": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
        "read_failures": READ_FAILURES,
    })
    for filename, records in (("samples.jsonl", samples), ("output-received.jsonl", events)):
        (output / filename).write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")
    for stream, data in streams.items():
        (output / f"test.{stream}").write_bytes(data)
        print(f"--- test {stream} ---", flush=True)
        sys.stdout.buffer.write(data)
        sys.stdout.buffer.flush()
    return 128 - result if result < 0 else result


def main():
    output = Path(sys.argv[1]).resolve()
    save_json(output / "result.json", {"state": "test-not-run"})
    package = Path.cwd() / "crates" / "nlos-task"
    test = find_test_executable(output / "cargo-build.jsonl", package, output)
    rust_lib = subprocess.check_output(["rustc", "--print", "target-libdir"], text=True).strip()
    env = os.environ.copy()
    paths = [str(test.parent), str(test.parent.parent), rust_lib, env.get("LD_LIBRARY_PATH", "")]
    env["LD_LIBRARY_PATH"] = os.pathsep.join(filter(None, paths))
    ldd = subprocess.run(["ldd", str(test)], env=env, capture_output=True, text=True, check=False)
    (output / "test-ldd.txt").write_text(ldd.stdout + ldd.stderr, encoding="utf-8")
    if ldd.returncode or "not found" in ldd.stdout:
        raise RuntimeError("The test binary has unresolved runtime libraries; see test-ldd.txt")
    argv = [str(test), BENCHMARK, "--exact", "--nocapture", "--test-threads=1"]
    listed = subprocess.check_output([*argv, "--list", "--format=terse"], cwd=package, env=env, text=True)
    (output / "test-list.txt").write_text(listed, encoding="utf-8")
    if listed.splitlines() != [f"{BENCHMARK}: test"]:
        raise RuntimeError(f"Expected exactly one requested test, got {listed!r}")
    return observe(argv, package, env, output)


if __name__ == "__main__":
    sys.exit(main())
