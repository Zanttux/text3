#!/usr/bin/env python3
"""Measure Text³ on large files (#206, docs/PERFORMANCE.md "Large files").

Runs the in-app workload driver (crates/text3-app/src/benchprobe.rs and
shell/bench.rs): each run starts the release binary with TEXT3_BENCH set,
the app opens the file after its first frame, runs the scenario through
the window's own code paths, appends one JSON line and exits. This script
generates the input files (fixed generator, no randomness), runs every
workload --runs times with a scratch profile (never the user's settings,
recovery folder or session) and reports median and p95 per metric plus
the peak resident memory of the process.

Workloads (full set; --quick is the smaller CI set):
  open-10mb, open-100mb  open a plain-text file in an editor tab
  open-300mb             above the 256 MB threshold: opens in the log viewer
  log-1gb                log viewer: index 1 GiB, then filter "ERROR"
                         (plain) and a regular expression
  find-100mb             find bar: count every "timeout", then a regex
  type-50mb              type 200 characters into the middle, one per frame
  scroll-50mb            move down one screen (40 lines) per frame, 200 times

Peak memory: macOS and Linux from the child's rusage (os.wait4), Windows
from GetProcessMemoryInfo (ctypes); null where neither works.

Usage: scripts/bench/workloads.py [--quick] [--runs 5] [--only NAME ...]
       [--scratch DIR] [--no-build | --binary PATH] [--json out.json]
       [--check bench/budgets.json]
Exit status 1 when --check is given and a budget is exceeded.
"""

import argparse
import json
import math
import os
import platform
import shutil
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
EXE = "text3.exe" if os.name == "nt" else "text3"
DEFAULT_SCRATCH = Path(
    os.environ.get("TEXT3_WORKLOADS_SCRATCH")
    or (Path(os.environ.get("RUNNER_TEMP") or os.environ.get("TMPDIR") or "/tmp")
        / "text3-workloads")
)
MIB = 1 << 20
# Changing the generator changes the file names, so old files are not reused.
GENERATOR = "g1"

FIND_WORD = "timeout"
FIND_REGEX = r"time\w*out after \d+ ms"
LOG_QUERY = "ERROR"
LOG_REGEX = r"ERROR .*id=\d*7\b"


def workloads(quick):
    """name -> (scenario, file size in bytes, extra environment)."""
    if quick:
        return {
            "open-10mb": ("open", 10 * MIB, {}),
            "open-100mb": ("open", 100 * MIB, {}),
            "log-200mb": ("log", 200 * MIB, {"QUERY": LOG_QUERY, "REGEX": LOG_REGEX}),
            "find-50mb": ("find", 50 * MIB, {"QUERY": FIND_WORD, "REGEX": FIND_REGEX}),
            "type-50mb": ("type", 50 * MIB, {"STEPS": "100"}),
            "scroll-50mb": ("scroll", 50 * MIB, {"STEPS": "100"}),
        }
    return {
        "open-10mb": ("open", 10 * MIB, {}),
        "open-100mb": ("open", 100 * MIB, {}),
        "open-300mb": ("open", 300 * MIB, {}),
        "log-1gb": ("log", 1024 * MIB, {"QUERY": LOG_QUERY, "REGEX": LOG_REGEX}),
        "find-100mb": ("find", 100 * MIB, {"QUERY": FIND_WORD, "REGEX": FIND_REGEX}),
        "type-50mb": ("type", 50 * MIB, {"STEPS": "200"}),
        "scroll-50mb": ("scroll", 50 * MIB, {"STEPS": "200"}),
    }


def line(n):
    """Line `n` of every generated file: an ASCII log line of about 90
    bytes. ERROR every 53rd line, WARN every 7th, "timeout" every 41st."""
    if n % 53 == 0:
        level = "ERROR"
    elif n % 7 == 0:
        level = "WARN "
    else:
        level = "INFO "
    if n % 41 == 0:
        msg = f"upstream timeout after {n % 5000} ms"
    else:
        msg = f"request id={n * 7919 % 100003:06} took {n % 977} ms ok"
    s = n % 86400
    return (f"{n:09} 2026-01-01T{s // 3600:02}:{s // 60 % 60:02}:{s % 60:02} "
            f"{level} worker-{n % 16:02} {msg}\n")


def dataset(data, size):
    """A file of whole generated lines, at most `size` bytes (cached)."""
    path = data / f"{GENERATOR}-{size // MIB}mib.txt"
    done = path.with_suffix(".done")
    if path.exists() and done.exists():
        return path
    data.mkdir(parents=True, exist_ok=True)
    print(f"generating {path.name} …", flush=True)
    written = n = 0
    with open(path, "w", encoding="ascii", newline="\n") as f:
        while True:
            block = [line(i) for i in range(n, n + 100_000)]
            chunk = "".join(block)
            if written + len(chunk) > size:
                for ln in block:
                    if written + len(ln) > size:
                        break
                    f.write(ln)
                    written += len(ln)
                break
            f.write(chunk)
            written += len(chunk)
            n += len(block)
    done.write_text(str(written))
    return path


def fresh_profile(path):
    shutil.rmtree(path, ignore_errors=True)
    (path / "config").mkdir(parents=True)
    (path / "data").mkdir(parents=True)
    # Settings format: crates/text3-app/src/settings.rs. No first-start
    # offer and no update check over the network.
    (path / "config" / "settings").write_text(
        "text3-settings 1\ncheck_updates=false\ndefault_apps_asked=true\n"
    )


def env_for(profile, scenario, file, out, extra, timeout):
    env = {k: v for k, v in os.environ.items() if not k.startswith(("TEXT3_", "SANAIN_"))}
    env.update({
        "TEXT3_CONFIG_DIR": str(profile / "config"),
        "TEXT3_RECOVERY_DIR": str(profile / "data" / "recovery"),
        "TEXT3_NO_SESSION": "1",
        "TEXT3_LOCALE": "en-US",
        "TEXT3_BENCH": scenario,
        "TEXT3_BENCH_FILE": str(file),
        "TEXT3_BENCH_OUT": str(out),
        "TEXT3_BENCH_TIMEOUT": str(int(timeout)),
    })
    if os.name == "nt":
        # The OpenGL renderer needs a GPU driver; CI runners have none, and
        # wgpu falls back to Direct3D's software adapter.
        env.setdefault("TEXT3_RENDERER", os.environ.get("TEXT3_RENDERER", "wgpu"))
    for k, v in extra.items():
        env[f"TEXT3_BENCH_{k}"] = v
    return env


def windows_peak(handle):
    """PeakWorkingSetSize of a process handle (bytes), or None."""
    try:
        import ctypes
        from ctypes import wintypes

        class Counters(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD),
                ("PageFaultCount", wintypes.DWORD),
                ("PeakWorkingSetSize", ctypes.c_size_t),
                ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t),
            ]

        c = Counters()
        c.cb = ctypes.sizeof(c)
        psapi = ctypes.WinDLL("psapi")
        psapi.GetProcessMemoryInfo.argtypes = [
            wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD]
        if psapi.GetProcessMemoryInfo(int(handle), ctypes.byref(c), c.cb):
            return c.PeakWorkingSetSize
    except (OSError, AttributeError, ValueError):
        pass
    return None


def run_once(binary, env, timeout):
    """Start the app, wait for it; returns (exit code, peak RSS bytes or None, stderr)."""
    proc = subprocess.Popen(
        [str(binary)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE
    )
    err = []
    reader = threading.Thread(target=lambda: err.append(proc.stderr.read()), daemon=True)
    reader.start()
    killer = threading.Timer(timeout, proc.kill)
    killer.start()
    try:
        if hasattr(os, "wait4"):
            _, status, usage = os.wait4(proc.pid, 0)
            proc.returncode = os.waitstatus_to_exitcode(status)
            # ru_maxrss: bytes on macOS, KiB on Linux.
            scale = 1 if sys.platform == "darwin" else 1024
            peak = usage.ru_maxrss * scale or None
        else:
            proc.wait()
            peak = windows_peak(proc._handle) if os.name == "nt" else None
    finally:
        killer.cancel()
    reader.join(5)
    stderr = (err[0] if err else b"").decode("utf-8", "replace")
    return proc.returncode, peak, stderr


def nearest_rank(values, q):
    s = sorted(values)
    return s[max(0, math.ceil(q * len(s)) - 1)]


def summarize(runs):
    """Per metric: median, p95, min, max over the runs."""
    out = {}
    keys = sorted({k for r in runs for k, v in r.items()
                   if isinstance(v, (int, float)) and not isinstance(v, bool)})
    for k in keys:
        vals = [r[k] for r in runs if isinstance(r.get(k), (int, float))]
        if not vals:
            continue
        out[k] = {
            "median": statistics.median(vals),
            "p95": nearest_rank(vals, 0.95),
            "min": min(vals),
            "max": max(vals),
        }
    for k in sorted({k for r in runs for k, v in r.items() if isinstance(v, (str, bool))}):
        vals = {r.get(k) for r in runs}
        out[k] = vals.pop() if len(vals) == 1 else sorted(map(str, vals))
    return out


def machine():
    info = {"os": platform.platform(), "machine": platform.machine(),
            "python": platform.python_version(), "cpus": os.cpu_count()}
    if sys.platform == "darwin":
        for key, cmd in {"cpu": ["sysctl", "-n", "machdep.cpu.brand_string"],
                         "ram_bytes": ["sysctl", "-n", "hw.memsize"]}.items():
            try:
                info[key] = subprocess.run(cmd, capture_output=True, text=True,
                                           check=True).stdout.strip()
            except (OSError, subprocess.CalledProcessError):
                pass
    try:
        info["commit"] = subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"], cwd=REPO,
            capture_output=True, text=True, check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        pass
    if hasattr(os, "getloadavg"):
        info["load_average_start"] = [round(x, 1) for x in os.getloadavg()]
    return info


def os_key():
    if os.name == "nt":
        return "windows"
    return "macos" if sys.platform == "darwin" else "linux"


def check(results, budgets):
    """Compare each workload's median against bench/budgets.json."""
    over = []
    table = {k: dict(v) for k, v in budgets.get("workloads", {}).items()
             if isinstance(v, dict)}
    # Per-platform limits replace the common ones metric by metric.
    for name, limits in budgets.get("workloads_by_os", {}).get(os_key(), {}).items():
        if isinstance(limits, dict):
            table.setdefault(name, {}).update(limits)
    for name, limits in table.items():
        if name.startswith("_") or name not in results["workloads"]:
            continue
        got = results["workloads"][name]["summary"]
        for metric, limit in limits.items():
            if metric.startswith("_"):
                continue
            # "<metric>" compares the median; "<metric>@p95" the p95.
            key, _, stat = metric.partition("@")
            stat = stat or "median"
            value = got.get(key, {}).get(stat) if isinstance(got.get(key), dict) else None
            if value is None:
                if key == "peak_rss_mib":
                    print(f"  {name}: {key} not measured on this platform")
                    continue
                over.append(f"{name}: {metric} missing")
                continue
            mark = "ok" if value <= limit else "OVER"
            print(f"  {mark:4} {name:13} {metric:24} {value:10.1f}  budget {limit}")
            if value > limit:
                over.append(f"{name}: {metric} {value:.1f} > {limit}")
    return over


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--quick", action="store_true", help="smaller CI set")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--only", nargs="*", help="workload names to run")
    ap.add_argument("--scratch", type=Path, default=DEFAULT_SCRATCH)
    ap.add_argument("--no-build", action="store_true")
    ap.add_argument("--binary", type=Path, help="measure this build (implies --no-build)")
    ap.add_argument("--timeout", type=float, default=900, help="seconds per run")
    ap.add_argument("--json", type=Path, help="write the results here")
    ap.add_argument("--check", type=Path, help="fail when over these budgets")
    args = ap.parse_args()

    if not args.no_build and not args.binary:
        subprocess.run(["cargo", "build", "--release", "--locked", "-p", "text3-app"],
                       cwd=REPO, check=True)
    binary = (args.binary or REPO / "target" / "release" / EXE).resolve()
    scratch = args.scratch.resolve()
    scratch.mkdir(parents=True, exist_ok=True)
    chosen = workloads(args.quick)
    if args.only:
        unknown = set(args.only) - set(chosen)
        if unknown:
            sys.exit(f"unknown workloads: {', '.join(sorted(unknown))}")
        chosen = {k: v for k, v in chosen.items() if k in args.only}

    results = {"kind": "workloads", "date": time.strftime("%Y-%m-%d"),
               "set": "quick" if args.quick else "full", "runs": args.runs,
               "method": "scripts/bench/workloads.py", "machine": machine(),
               "binary_bytes": binary.stat().st_size, "workloads": {}}
    print(f"machine: {json.dumps(results['machine'])}")
    print(f"binary: {binary}\nscratch: {scratch}")
    failed = []
    for name, (scenario, size, extra) in chosen.items():
        file = dataset(scratch / "data", size)
        runs = []
        for i in range(args.runs):
            profile = scratch / "profile"
            fresh_profile(profile)
            out = scratch / "result.jsonl"
            out.unlink(missing_ok=True)
            env = env_for(profile, scenario, file, out, extra, args.timeout)
            code, peak, stderr = run_once(binary, env, args.timeout + 30)
            rec = {}
            if out.exists():
                lines = out.read_text(encoding="utf-8").strip().splitlines()
                if lines:
                    rec = json.loads(lines[-1])
            if code != 0 or "error" in rec or not rec:
                msg = rec.get("error") or f"exit {code}: {stderr.strip()[-1500:]}"
                print(f"  {name} run {i + 1}: FAILED ({msg})", flush=True)
                failed.append(f"{name}: {msg}")
                break
            rec["peak_rss_mib"] = peak / MIB if peak else None
            rec.pop("file", None)
            runs.append(rec)
            shown = ", ".join(
                f"{k}={v:.1f}" if isinstance(v, float) else f"{k}={v}"
                for k, v in rec.items()
                if k.endswith(("_ms", "_p50", "_p95", "_max", "_matches", "_mib"))
                and v is not None
            )
            print(f"  {name} run {i + 1}: {shown}", flush=True)
        if runs:
            results["workloads"][name] = {
                "scenario": scenario, "file_bytes": file.stat().st_size,
                "env": extra, "summary": summarize(runs), "runs": runs}
    if hasattr(os, "getloadavg"):
        results["machine"]["load_average_end"] = [round(x, 1) for x in os.getloadavg()]

    print("\nsummary (median / p95):")
    for name, w in results["workloads"].items():
        print(f"  {name}")
        for k, s in w["summary"].items():
            if isinstance(s, dict) and (k.endswith(("_ms", "_p50", "_p95", "_max", "_mib"))):
                print(f"    {k:22} {s['median']:10.1f} / {s['p95']:10.1f}")
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(results, indent=2) + "\n", encoding="utf-8")
        print(f"results: {args.json}")
    if args.check:
        budgets = json.loads(args.check.read_text(encoding="utf-8"))
        print(f"\nbudgets ({args.check}):")
        over = check(results, budgets)
        failed += over
    if failed:
        sys.exit("failed:\n  " + "\n  ".join(failed))


if __name__ == "__main__":
    main()
