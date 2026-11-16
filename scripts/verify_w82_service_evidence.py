"""Run the native W82 service probes and retain their evidence on failure."""

import argparse
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile


BLOB_TEST = ["cargo", "test", "--locked", "-p", "nlos-capability", "--test", "blob_open"]
TOOL_TEST = ["cargo", "test", "--locked", "-p", "nlos-wasm-sandbox", "--test"]
WINDOWS_LEAF_TEST = "repeated_first_open_symlink_leaf_swaps_do_not_escape"
RUNNER_KEYS = (
    "GITHUB_SHA", "GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "RUNNER_NAME", "RUNNER_OS", "RUNNER_ARCH",
    "RUNNER_ENVIRONMENT", "ImageOS", "ImageVersion",
)


class Evidence:
    def __init__(self, directory: Path):
        self.directory = directory
        self.directory.mkdir(parents=True, exist_ok=True)
        self.results = self.directory / "results.jsonl"
        self.results.write_text("", encoding="utf-8")
        self.exit_code = 0

    def record(self, label: str, status: str, **fields):
        print(f"{label}: {status}", flush=True)
        with self.results.open("a", encoding="utf-8") as journal:
            journal.write(json.dumps({"label": label, "status": status, **fields}) + "\n")
        if status != "passed":
            self.exit_code = 1

    def check(self, label: str, passed: bool, detail: str):
        self.record(label, "passed" if passed else "failed", detail=detail)
        if not passed:
            print(detail, flush=True)

    def run(self, label: str, command: list[str], *, echo: bool = True) -> tuple[int | None, str]:
        log = self.directory / f"{label}.log"
        print(f"{label}: {json.dumps(command)}", flush=True)
        with log.open("wb") as output:
            try:
                completed = subprocess.run(
                    command, stdout=output, stderr=subprocess.STDOUT, check=False,
                    env={**os.environ, "CARGO_TERM_COLOR": "never"},
                )
                returncode = completed.returncode
            except OSError as error:
                output.write(f"{error}\n".encode())
                returncode = None
        text = log.read_text(encoding="utf-8", errors="replace")
        if echo or returncode != 0:
            print(text, end="", flush=True)
        self.record(label, "passed" if returncode == 0 else "failed",
                    command=command, returncode=returncode, log=log.name)
        return returncode, text


def check_test_passed(evidence: Evidence, probe: str, output: str, test: str):
    evidence.check(f"{probe}:{test}", bool(re.search(rf"\btest {test} \.\.\. ok\b", output)),
                   f"Expected a passing native test result for {test}")


def check_escape_matrix(evidence: Evidence, output: str, system: str):
    leaf = "windows_symlink" if system == "windows" else "unix_symlink"
    parent = "ntfs_junction" if system == "windows" else "unix_symlink"
    for marker in (f"escape_preopen_leaf={leaf}", "escape_preopen_leaf=hard_link",
                   f"escape_preopen_parent={parent}"):
        evidence.check(marker, bool(re.search(rf"\b{marker}\b", output)),
                       f"Missing native escape fixture marker: {marker}")


def run_tool_probes(evidence: Evidence, system: str):
    _, output = evidence.run("tool-latency", [
        *TOOL_TEST, "tool_service", "measured_spawn_and_run", "--", "--exact", "--nocapture",
    ])
    check_test_passed(evidence, "tool-latency", output, "measured_spawn_and_run")
    sample = re.search(
        rf"\bplatform={system} arch=\S+ .*\bsamples=20\b .*\bcatalog=warm "
        r"path=invoke\+spawn\+run\+durable_result p50_ms=\d+\.\d+ p95_ms=\d+\.\d+\b",
        output,
    )
    evidence.check("tool-latency-sample", sample is not None,
                   sample.group() if sample else "Missing warm-catalog 20-sample p50/p95 record")

    _, output = evidence.run("tool-escape-matrix", [*TOOL_TEST, "escape_matrix", "--", "--nocapture"])
    check_escape_matrix(evidence, output, system)


def blob_open_executable(output: str) -> str | None:
    manifest = Path("crates/nlos-capability/Cargo.toml").resolve()
    executables = set()
    for line in output.splitlines():
        if not line.startswith("{"):
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        target = message.get("target", {})
        if (message.get("reason") == "compiler-artifact"
                and Path(message.get("manifest_path", "")).resolve() == manifest
                and target.get("name") == "blob_open"
                and target.get("kind") == ["test"]
                and message.get("executable")):
            executables.add(message["executable"])
    if len(executables) == 1:
        return executables.pop()
    return None


def check_openat2_trace(evidence: Evidence, trace: Path, *, injected: bool):
    # strace -ff keeps a separate file per thread so syscall lines never interleave.
    result = r"-1 ENOSYS[^\n]* \(INJECTED\)" if injected else r"[0-9]+"
    pattern = re.compile(rf"^openat2\([^\n]*\bRESOLVE_BENEATH\b[^\n]*\)\s+= {result}$",
                         re.MULTILINE)
    for thread_trace in sorted(trace.parent.glob(f"{trace.name}.*")):
        match = pattern.search(thread_trace.read_text(encoding="utf-8", errors="replace"))
        if match:
            evidence.record(trace.parent.name, "passed", detail=match.group(),
                            trace=str(thread_trace.relative_to(evidence.directory)))
            return
    expected = "injected ENOSYS" if injected else "successful return"
    evidence.check(trace.parent.name, False, f"No rooted openat2 syscall with {expected} in the trace")


def check_blob_suite(evidence: Evidence, probe: str, output: str, system: str):
    parent = ("repeated_first_open_junction_parent_swaps_do_not_escape" if system == "windows"
              else "repeated_first_open_symlink_parent_and_leaf_swaps_do_not_escape")
    for test in ("opens_only_a_single_link_regular_file_under_the_held_directory",
                 "repeated_first_open_leaf_swaps_do_not_read_the_hard_link_outside_sentinel",
                 parent):
        check_test_passed(evidence, probe, output, test)


def run_linux_blob_probes(evidence: Evidence):
    returncode, output = evidence.run("blob-open-build", [
        *BLOB_TEST, "--no-run", "--message-format=json-render-diagnostics",
    ], echo=False)
    executable = blob_open_executable(output) if returncode == 0 else None
    if executable is None:
        for label in ("blob-open-native", "blob-open-enosys"):
            evidence.record(label, "skipped", detail="Cargo did not produce one blob_open test executable")
        return

    if not shutil.which("strace") and os.getenv("RUNNER_ENVIRONMENT") == "github-hosted":
        evidence.run("strace-apt-update", ["sudo", "-n", "apt-get", "update"])
        evidence.run("strace-apt-install", ["sudo", "-n", "apt-get", "install", "--yes", "strace"])
    strace = shutil.which("strace")
    if not strace:
        _, output = evidence.run("blob-open-native", [executable, "--nocapture"])
        check_blob_suite(evidence, "blob-open-native", output, "linux")
        evidence.record("blob-openat2-trace", "skipped", detail="strace unavailable")
        evidence.record("blob-open-enosys", "skipped", detail="strace unavailable")
        return

    evidence.run("strace-version", [strace, "--version"])
    for label, injected in (("blob-open-native", False), ("blob-open-enosys", True)):
        trace_dir = Path(tempfile.mkdtemp(prefix=f"{label}-trace-", dir=evidence.directory))
        trace = trace_dir / "openat2"
        command = [strace, "-ff", "-e", "trace=openat2", "-o", str(trace)]
        if injected:
            command.extend(["-e", "inject=openat2:error=ENOSYS"])
        _, output = evidence.run(label, [*command, executable, "--nocapture"])
        check_blob_suite(evidence, label, output, "linux")
        check_openat2_trace(evidence, trace, injected=injected)


def run_blob_probes(evidence: Evidence, system: str):
    if system == "linux":
        run_linux_blob_probes(evidence)
        return
    _, output = evidence.run("blob-open-native", [*BLOB_TEST, "--", "--nocapture"])
    check_blob_suite(evidence, "blob-open-native", output, system)
    if system == "windows":
        _, output = evidence.run("blob-open-windows-symlink-leaf", [
            *BLOB_TEST, WINDOWS_LEAF_TEST, "--", "--exact", "--ignored", "--nocapture",
        ])
        check_test_passed(evidence, "blob-open-windows-symlink-leaf", output, WINDOWS_LEAF_TEST)
        swaps = re.search(r"\blink-leaf: swaps=[1-9]\d* .*\bexterior_or_unexpected=0\b", output)
        evidence.check("blob-open-windows-symlink-swaps", swaps is not None,
                       swaps.group() if swaps else "Missing a successful Windows leaf symlink swap record")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output_dir", type=Path)
    args = parser.parse_args()
    sys.stdout.reconfigure(errors="backslashreplace")
    evidence = Evidence(args.output_dir.resolve())
    system = platform.system().lower()
    (evidence.directory / "runner.json").write_text(json.dumps({
        "platform": platform.uname()._asdict(),
        "github_runner": {key: os.environ[key] for key in RUNNER_KEYS if key in os.environ},
    }, indent=2) + "\n", encoding="utf-8")
    _, sha = evidence.run("source-sha", ["git", "rev-parse", "HEAD"])
    if expected_sha := os.getenv("GITHUB_SHA"):
        evidence.check("source-sha-matches-run", sha.strip() == expected_sha,
                       "Checked-out HEAD must match the GitHub run SHA")
    evidence.run("rustc-version", ["rustc", "--version", "--verbose"])
    evidence.run("cargo-version", ["cargo", "--version", "--verbose"])
    run_tool_probes(evidence, {"darwin": "macos"}.get(system, system))
    run_blob_probes(evidence, system)
    return evidence.exit_code


if __name__ == "__main__":
    raise SystemExit(main())
