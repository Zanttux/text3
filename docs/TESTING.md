# Quality gates

Run locally and in CI:

    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo test --workspace --locked

Unit: rope offsets, graphemes, undo/redo, transactions, encodings, newline preservation, parser and action errors. Property tests: random edits + undo restore original, coordinate round trips, unchanged file bytes. Integration: save/reopen, crash recovery, external conflict, disk full, permission denial, corrupt session, symlinks, concurrent instances. Fuzz parsers, decoding, recovery and transformations with bounded inputs. UI: keyboard, IME, screen reader, tabs, split, drag/drop, DPI, native dialogs. Security: malicious Markdown/XML/YAML, no outbound traffic in default use, dependency audit and license checks.

Disk-full drill: `crates/text3-app/tests/disk_full.rs` runs only when `TEXT3_FULL_DIR` names a directory on a small throwaway filesystem (it refuses to fill more than 512 MiB); otherwise it prints "skipped" and passes. It fills the filesystem itself, leaving 64 KiB free. CI runs it after the workspace tests on a 4 MiB tmpfs (Linux), a 2 MiB HFS+ disk image (macOS) and an 8 MiB NTFS VHD created with `diskpart` (Windows; runners are administrators). Locally on macOS:

    hdiutil create -size 2m -fs HFS+ -volname Text3Full -layout NONE full.dmg
    hdiutil attach -nobrowse -mountpoint ./full full.dmg
    TEXT3_FULL_DIR=$PWD/full cargo test -p text3-app --test disk_full --locked
    hdiutil detach ./full

On Linux: `sudo mount -t tmpfs -o size=4m tmpfs /mnt/text3-full` and `chown` it to yourself first.

Proposed performance budgets (not measurements): cold start p95 <= 500 ms, warm <= 250 ms, idle RSS <= 100 MiB, typing p95 <= 16 ms on documented reference machine; 100 MB file opens <= 2s where feasible; 10 GB read-only log mode bounded memory (P1). Record OS, CPU, RAM, disk, build, dataset, 20+ runs, cold/warm method, median/p95 and peak RSS. Do not claim targets met without evidence. Start-up and memory are measured with `scripts/measure-startup.py` (release build, scratch profile via TEXT3_CONFIG_DIR/TEXT3_RECOVERY_DIR, 20 runs per case; `--baseline <old binary>` alternates launches of two builds so both see the same machine load — use it for before/after, since absolute times move by tens of ms with background load). The app reports its own start-up marks when TEXT3_MEASURE_STARTUP=<file> is set (TEXT3_MEASURE_EXIT=1 exits after the first full frame; crates/text3-app/src/startprobe.rs); time before `main` comes from the launcher, peak RSS from `/usr/bin/time -l`, idle RSS from `ps` after 5 s. "Cold-ish" on macOS = a fresh copy of the binary and an empty profile; the OS file cache cannot be dropped without root, so true cold start is not measured. Large-file workloads (open, log viewer index and filter, find, typing and scrolling latency, peak memory) are measured with `scripts/bench/workloads.py` (TEXT3_BENCH=<scenario> with TEXT3_BENCH_FILE and TEXT3_BENCH_OUT makes the release build run one scripted workload through its own UI code after the first frame and exit; crates/text3-app/src/benchprobe.rs); `--quick --check bench/budgets.json` is the CI version. Compare Notepad++ on same Windows hardware and tasks; no cross-platform apples-to-oranges comparisons.

Release gate: all P0 acceptance tests, no known data-loss/security blockers, signed artifacts where available, dependency inventory, manual smoke on supported OS, recovery drill, accessibility report, human approval. CI should run fmt/clippy/test on Windows/macOS/Linux; platform packaging smoke tests and reproducible build documentation. Never mark skipped tests as passed.
