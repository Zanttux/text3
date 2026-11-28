#!/usr/bin/env python3
"""Write docs/PERFORMANCE.md and its charts from bench/results/*.json (#179).

Only measured numbers are shown: every figure comes from a results file
written by scripts/bench/compare.py (or a later measuring script), with the
machine, versions and method next to it. No dependencies: the charts are
plain SVG written here, readable in GitHub's light and dark themes.

Usage: scripts/bench/report.py   (from the repository root or anywhere)
"""
import json
from pathlib import Path
from xml.sax.saxutils import escape

REPO = Path(__file__).resolve().parents[2]
RESULTS = REPO / "bench" / "results"
OUT = REPO / "docs" / "PERFORMANCE.md"
CHARTS = REPO / "docs" / "performance"

ACCENT = "#788B53"  # BRAND.md accent: Text³'s bars
OTHER = "#A3A3A3"   # neutral gray for the other editors


def bar_chart(title, unit, rows, path, log=False):
    """Horizontal bars; rows = [(label, value, is_text3)]. Lower is better."""
    import math

    width, left, bar_h, gap, top = 640, 170, 22, 10, 40
    height = top + len(rows) * (bar_h + gap) + 10
    vmax = max(v for _, v, _ in rows)
    scale = (lambda v: math.log10(max(v, 1)) / math.log10(max(vmax, 10))) if log \
        else (lambda v: v / vmax)
    span = width - left - 110
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" '
        f'viewBox="0 0 {width} {height}" font-family="-apple-system,Segoe UI,sans-serif" '
        f'font-size="13">',
        "<style>text{fill:#404040}@media (prefers-color-scheme:dark){text{fill:#D4D4D4}}</style>",
        f'<text x="0" y="18" font-weight="600">{escape(title)}</text>',
        f'<text x="{width}" y="18" text-anchor="end" font-size="11">'
        f'{"log scale, " if log else ""}lower is better</text>',
    ]
    for i, (label, v, ours) in enumerate(rows):
        y = top + i * (bar_h + gap)
        w = max(2, span * scale(v))
        out.append(f'<text x="{left - 8}" y="{y + 16}" text-anchor="end"'
                   f'{" font-weight=\"600\"" if ours else ""}>{escape(label)}</text>')
        out.append(f'<rect x="{left}" y="{y}" width="{w:.1f}" height="{bar_h}" rx="3" '
                   f'fill="{ACCENT if ours else OTHER}"/>')
        out.append(f'<text x="{left + w + 6:.1f}" y="{y + 16}">{v:,.0f} {unit}</text>')
    out.append("</svg>")
    path.write_text("\n".join(out) + "\n", encoding="utf-8")


def large_files(w):
    """The "Large files" section from a workloads.py results file."""
    m = w["machine"]
    loads = w["workloads"]

    def med(name, key):
        v = loads.get(name, {}).get("summary", {}).get(key)
        return v["median"] if isinstance(v, dict) else None

    def ms(name, key):
        v = med(name, key)
        return "–" if v is None else (f"{v:,.0f} ms" if v >= 10 else f"{v:.1f} ms")

    def mib(name):
        v = med(name, "peak_rss_mib")
        return "–" if v is None else f"{v:,.0f} MiB"

    def count(name, key):
        v = med(name, key)
        return "–" if v is None else f"{v:,.0f}"

    def query(name):
        return loads.get(name, {}).get("env", {}).get("QUERY", "")

    ram = int(m.get("ram_bytes", 0)) // 2**30
    out = [
        f"## Large files ({w['date']})",
        "",
        f"{m.get('cpu', m['machine'])}, {ram} GB, {m['os']}. Commit "
        f"`{m.get('commit', '?')}`. Method: `{w['method']}` (#206): the release build "
        "runs each workload itself, through the window's own code (File › Open, "
        "File › Open Log, the find bar, the log viewer's filter field, text and "
        "key events delivered to the editor as keyboard input), after its first "
        "frame is on screen, then writes the times and exits "
        "(`TEXT3_BENCH`, crates/text3-app/src/benchprobe.rs). "
        f"{w['runs']} runs per workload, a fresh scratch profile each run, medians "
        "shown. Files are generated ASCII log lines (fixed generator, about 79 bytes "
        "per line). *Shown* is from the open command to the start of the frame after "
        "the first one showing the file, i.e. until that frame is on screen. Peak "
        "memory is the largest resident size of the whole process (start-up "
        "included), from the operating system.",
        "",
        "| Workload | Result | Peak memory |",
        "|---|---|---|",
        f"| Open 10 MB (editor tab) | shown {ms('open-10mb', 'shown_ms')} | {mib('open-10mb')} |",
        f"| Open 100 MB (editor tab) | shown {ms('open-100mb', 'shown_ms')} | {mib('open-100mb')} |",
        f"| Open 300 MB (from 256 MB: log viewer) | shown {ms('open-300mb', 'shown_ms')}, "
        f"indexed {ms('open-300mb', 'indexed_ms')} | {mib('open-300mb')} |",
        f"| Log viewer, 1 GiB ({count('log-1gb', 'lines')} lines) | indexed "
        f"{ms('log-1gb', 'indexed_ms')}; filter `{query('log-1gb')}` "
        f"{ms('log-1gb', 'filter_ms')} ({count('log-1gb', 'filter_matches')} lines); regex "
        f"{ms('log-1gb', 'regex_ms')} ({count('log-1gb', 'regex_matches')} lines) "
        f"| {mib('log-1gb')} |",
        f"| Find in 100 MB: count every `{query('find-100mb')}` | "
        f"{ms('find-100mb', 'find_ms')} ({count('find-100mb', 'find_matches')} matches); "
        f"regex {ms('find-100mb', 'find_regex_ms')} | {mib('find-100mb')} |",
        f"| Typing into the middle of 50 MB, one character per frame | app "
        f"{ms('type-50mb', 'ui_ms_p50')} / p95 {ms('type-50mb', 'ui_ms_p95')}; on screen "
        f"{ms('type-50mb', 'frame_ms_p50')} / p95 {ms('type-50mb', 'frame_ms_p95')} "
        f"| {mib('type-50mb')} |",
        f"| Scrolling 50 MB, one screen (40 lines) per frame | app "
        f"{ms('scroll-50mb', 'ui_ms_p50')} / p95 {ms('scroll-50mb', 'ui_ms_p95')}; on screen "
        f"{ms('scroll-50mb', 'frame_ms_p50')} / p95 {ms('scroll-50mb', 'frame_ms_p95')} "
        f"| {mib('scroll-50mb')} |",
        "",
        "Typing and scrolling: *app* is the time from the frame's input to the end "
        "of Text³'s own work for that frame (before painting); *on screen* is from "
        "one frame's input to the next frame's, which includes painting and waiting "
        "for the display (60 Hz here, so 16.7 ms is the floor). Per-frame median and "
        "p95 within a run, then the median over runs. Scrolling moves the caret down "
        "40 lines per frame with the Down key (the editor has no Page Down yet).",
        "",
    ]
    if w.get("note"):
        out += [w["note"], ""]
    out += [
        "CI runs a smaller set (`--quick`: 200 MB log, 50 MB find, 100 typed "
        "characters and scroll steps, 3 runs) on Linux under Xvfb, Windows and "
        "macOS, and fails when a median exceeds `bench/budgets.json`. Hosted "
        "runners paint in software, so their *on screen* times are not comparable "
        "with these. On the Windows runner (Direct3D's software adapter, about "
        "200 ms per frame) the end of a find or filter is noticed a frame later, "
        "so it has its own looser limits (`workloads_by_os`).",
        "",
    ]
    return out


def main():
    files = sorted(RESULTS.glob("*.json"))
    compares = [json.loads(f.read_text(encoding="utf-8")) for f in files]
    compares = [c for c in compares if c.get("kind") == "compare"]
    CHARTS.mkdir(parents=True, exist_ok=True)
    md = [
        "# Performance",
        "",
        "Lightness and speed are the point of Text³ (docs/QUALITY_GATES.md, G2).",
        "This page is generated by `scripts/bench/report.py` from the measured",
        "results in `bench/results/`; every number comes from a real run, with",
        "the machine, versions and method beside it. Run the measurements",
        "yourself with the scripts named below.",
        "",
    ]
    if compares:
        c = compares[-1]
        apps = c["apps"]
        order = sorted(apps, key=lambda n: apps[n]["window_ms"]["median"])
        m = c["machine"]
        md += [
            f"## Compared with other editors ({c['date']})",
            "",
            f"{m['os']}, {m['cpu']}, {m['memory_gib']} GB. Text³ commit "
            f"`{c['text3_commit']}`. Method: `{c['method']}`: each app opens the "
            f"same {c['file']} file through Launch Services (`open -n -a`, as a "
            f"double-click does), {c['runs']} alternating runs after one warm-up, "
            "scratch profiles. *Window* is the time until the app's first window "
            "is on screen (window server, polled every millisecond; no in-app "
            f"probe). *Memory* is the resident memory of all the app's processes "
            f"after {c['settle_s']:.0f} s idle. *Size* is the app bundle on disk.",
            "",
            "![Time to first window](performance/compare-window.svg)",
            "",
            "![Memory after opening a file](performance/compare-memory.svg)",
            "",
            "| App | Version | Window, median / p95 | Memory, median | App size |",
            "|---|---|---|---|---|",
        ]
        for n in order:
            a = apps[n]
            name = f"**{n}**" if n == "Text³" else n
            md.append(
                f"| {name} | {a['version']} | {a['window_ms']['median']:.0f} / "
                f"{a['window_ms']['p95']:.0f} ms | {a['idle_rss_mib']['median']:.0f} MiB | "
                f"{a['app_size_bytes'] / 2**20:.0f} MiB |"
            )
        if c.get("not_installed"):
            md += ["", "Not measured on this machine (not installed): "
                   + ", ".join(c["not_installed"]) + "."]
        md += [
            "",
            f"Machine load average at the end of the run: {m['load_average_end']}. "
            "Window time is not the same as the file's text being drawn: it is the "
            "one point that can be measured the same way for every app. TextEdit "
            "is part of macOS, so its libraries are already loaded and shared, "
            "which keeps its size and memory small.",
            "",
        ]
        bar_chart("Time to first window (median)", "ms",
                  [(n, apps[n]["window_ms"]["median"], n == "Text³") for n in order],
                  CHARTS / "compare-window.svg")
        mem_order = sorted(apps, key=lambda n: apps[n]["idle_rss_mib"]["median"])
        bar_chart("Memory after opening a 4 KB file (median)", "MiB",
                  [(n, apps[n]["idle_rss_mib"]["median"], n == "Text³") for n in mem_order],
                  CHARTS / "compare-memory.svg", log=True)
    loads = [json.loads(f.read_text(encoding="utf-8")) for f in files]
    loads = [w for w in loads if w.get("kind") == "workloads" and w.get("set") == "full"]
    if loads:
        md += large_files(loads[-1])
    md += [
        "## Over time",
        "",
        "Text³'s own start-up and memory per change are in docs/BENCHMARKS.md",
        "(`scripts/measure-startup.py`, which uses the in-app start-up probe and",
        "compares against the previous build). Each new comparison run adds a",
        "results file here, so the history accumulates with the code.",
        "",
        "## Not measured yet",
        "",
        "- Windows (Notepad++, Sublime Text, VS Code) and Linux: planned on",
        "  throwaway CI runners, so no third-party program is installed on a",
        "  developer's own machine.",
        "- Large files, typing latency and search speed in other editors (Text³'s",
        "  own are above), and the large-file workloads on Windows and Linux",
        "  hardware outside CI.",
        "",
    ]
    OUT.write_text("\n".join(md), encoding="utf-8")
    print(f"wrote {OUT.relative_to(REPO)} and {CHARTS.relative_to(REPO)}/")


if __name__ == "__main__":
    main()
