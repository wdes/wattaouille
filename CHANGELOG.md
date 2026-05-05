# Changelog

All notable changes to **wattaouille** are recorded here. Format loosely
follows [Keep a Changelog](https://keepachangelog.com/) and [Semantic
Versioning](https://semver.org/).

## Unreleased

## v1.3.4

### Added
- Version string in the live header:
  `wattaouille v1.3.4 — 1500 ms · 12 CPU(s) · …`
- `-V` / `--version` flag prints the version and exits.

## v1.3.3

### Fixed
- **Mouse-scroll garbage in the alt screen.** Scrolling with the mouse
  wheel inside the running `wattaouille` translated to arrow-key escape
  sequences (`\x1B[A`, `\x1B[B`) which the kernel tty driver echoed
  straight back into the alt-screen view as visible characters
  ("weird chars"). Now stdin is put in non-canonical / non-echo mode
  while the alt screen is up, and restored on Ctrl+C alongside the
  cursor and alt-screen-exit sequences. Adds `libc = "0.2"` as an
  explicit dep (already pulled in transitively via `ctrlc`).

## v1.3.2

### Added
- `Element (Matrix)` label for `element-desktop` (matched by either the
  truncated `element-desktop` comm or the un-truncated `Element`).
- `Claude shell (<folder>)` label for the `bash -c source
  /home/.../.claude/shell-snapshots/...` processes Claude Code spawns for
  every tool call. The cwd basename distinguishes parallel projects.
- This `CHANGELOG.md`.

### Tests
- `+3` (Element, Claude shell with/without cwd). Total 49.

## v1.3.1

### Fixed
- **Backlight off detection.** The 🖥 line used to print a phantom
  `~1.4 W` even when the panel was blanked (Fn-Fx / DPMS). `BacklightSensor`
  now also reads `bl_power` and treats any non-zero value
  (1=NORMAL, 2=VSYNC_SUSPEND, 3=HSYNC_SUSPEND, 4=POWERDOWN) as off, so
  the line disappears entirely.
- **Wi-Fi rfkill detection.** `operstate` stays at `down` on rfkill;
  switched to `/sys/class/net/<iface>/carrier` (1 = link up). The 📡
  Wi-Fi line is now hidden when nothing is associated, instead of
  showing `~0.0 W`.

### Changed
- **Width-stable wattage formatting.** All wattage values now render as
  `{:>5.1}` (5 chars wide → `  0.5`, ` 12.5`, `123.5`), so the bits to
  the right don't shift between frames as values cross 10 W or 100 W.
  Drift uses `{:>+6.1}`. Affects RAPL, BAT, 🖥, 📡, and Δ lines.

### Tests
- `+1`. Total 46.

## v1.3.0

### Added
- **RAPL subdomain breakout** in the header:
  `⚡ RAPL 12.5 W avg (core 8.1 W · uncore 1.4 W · dram 0.6 W)`.
  Same chmod gates the subdomains as the package counter.
- **Per-process I/O column** in the leaderboard. `/proc/[pid]/io`
  read+write_bytes deltas per frame, formatted with `fmt_byte_rate`.
  Browser collapse roots sum their subtree's I/O. Other-user procs
  show `—` (the kernel denies the read).
- **Display panel watt estimate** (`🖥`). `BacklightSensor` reads
  `/sys/class/backlight/*/brightness` and models panel draw with a
  linear envelope: 0.5 W floor (panel electronics) to 3.5 W full.
- **Wi-Fi radio watt estimate** (`📡`). `NetSensor` classifies each
  interface as wireless and `estimate_wifi_radio_watts()` models
  radio draw: 0 W not-associated, 0.7 W idle, ramps linearly to
  2.5 W at 5 MB/s aggregated rx+tx.
- **Total network throughput** (`📶`) across non-loopback interfaces.
- **Estimate-prefix convention.** Any modeled value (display W,
  Wi-Fi W) is prefixed with `~` to flag it as a non-sensor reading.
  Per-user request.
- More pretty labels: Slack desktop, Discord, VS Code, Spotify,
  Thunderbird.

### Tests
- `+9` (parse_proc_io, BacklightSensor, three wifi cases, fmt_byte_rate,
  RaplDomain wraparound, Slack). Total 45.

## v1.2.2

### Fixed
- **Ctrl+C handler deadlock.** v1.2.1's handler tried to acquire
  `io::stdout().lock()`, but the render loop in `main` holds that lock
  for its entire run, so the handler thread blocked forever and the
  program never exited. Now writes the restore sequence directly via
  `libc::write` to fd 1, bypassing the Rust mutex.

### Added
- **MPL-2.0 license.** Added `LICENSE` and corrected `Cargo.toml`'s
  `license` field. README license section updated.

## v1.2.1

### Added
- **Ctrl+C terminal restore.** SIGINT/SIGTERM handler installed via the
  `ctrlc` crate (first dependency added) writes the alt-screen-exit +
  cursor-show escapes before exiting cleanly. (Note: this version had a
  deadlock — fixed in v1.2.2.)
- **Two-line header.** Runtime stats on line 1, energy bits on line 2.
  Keeps each line under terminal width even with RAPL + BAT + drift.
- **~30 more pretty labels.** Whole XFCE family, IBus, PipeWire,
  WirePlumber, dbus-daemon, systemd-journald/-logind/-udevd,
  NetworkManager, ModemManager, bluetoothd, accounts-daemon, polkitd,
  udisksd, upowerd, colord, rtkit-daemon, snapd, CUPS, smartd, boltd,
  xiccd, YubiKey-touch-detector, the kernel-truncated names
  (`power-profiles-`, `xdg-desktop-por`, …), rootlesskit, slirp4netns,
  caddy, apache2, crowdsec, anydesk, agetty, solaar, blueman, smartgit.sh.
- **Argv-aware shortcuts:** `containerd-shim (sha[:8])` from `-id`,
  `Xfce panel plugin (<name>)` from the `.so` basename, `Blueman (X)`
  from python wrappers, `ng serve (:port)` from Angular CLI.

### Tests
- `+9`. Total 36.

## v1.2.0

### Renamed
- **`monitor` → `wattaouille`.** Cargo crate, binary, every user-facing
  string, README. Project moved from `@williamdes/monitor` to
  `@wdes/wattaouille`.

### Added
- `Guake terminal` label for `python3 /usr/bin/guake`.
- `mysqld (cwd-basename)` / `mariadbd (cwd-basename)` to distinguish
  multiple instances by data-dir.
- **Battery state of charge + time-to-empty.** Header reads
  `🔋 BAT 18.0 W avg · 0.210 Wh (756 J) · 87% · 4h 23m left` while
  discharging, `🔌 on AC · 87%` while plugged in.
- `fmt_hours()` for `Xh YYm` formatting.
- **Drift as a rate.** `Δ non-CPU +5.5 W avg · 0.064 Wh (231 J · +31%)`
  so the gap reads as "rest of system pulling about 5 W."
- **27 unit tests** in `mod tests`.

## v1.1.3

### Changed
- **Total session energy in Wh** in the header alongside joules:
  `⚡ RAPL 12.5 W avg · 0.146 Wh (525 J)`.

## v1.1.2

### Added
- **Per-process total wattage** in the leaderboard. The `TOTAL J`
  column became `TOTAL W (J)` — `0.65W (27J)` for each process.

## v1.1.1

### Changed
- **Header W is session average, not instantaneous.** Both the rate and
  the cumulative figure now describe the same window, instead of mixing
  "this 1.5s sample" with "since start". Per-frame "now" stays in the
  leaderboard's `NOW W` column.

## v1.1.0

### Added
- **`PowerSensor::instructions()`** — multi-line setup walkthrough
  tailored to the kernel's reported cause (no `powercap`, no
  `intel-rapl`, mode-0400 files). Printed to stderr before entering the
  alt screen, then waits for Enter so the chmod/udev commands stay in
  the user's scrollback.
- **`--no-power`** flag to force the disabled path even when RAPL is
  readable, for previewing the new-user experience.
- **Battery cross-check.** `BatterySensor` reads `BAT*/power_now` and
  the `AC/online` flag. While discharging the header shows BAT watts
  and joules alongside the RAPL totals; once both have accumulated some
  data, a drift figure (Δ%) shows the share of battery energy not
  accounted for by the CPU package.

## v1.0.3

### Added
- **Per-process wattage via Intel RAPL.** `PowerSensor` reads
  `/sys/class/powercap/intel-rapl:0/energy_uj`; two reads bracketing
  the sample window give frame joules. Per-process watts are estimated
  by scaling the package total by each process's CPU share (same
  approach as Scaphandre — silicon doesn't actually report per-PID
  power). Cumulative joules tracked across the session.
- Leaderboard gains `NOW W` and `TOTAL J` columns. Header shows total
  package wattage and total session joules.
- One-line warning printed when RAPL isn't readable; W/J columns
  hidden in that mode (program runs unprivileged, no other regression).

## v1.0.2

### Added
- **`Claude Code (Happy: <folder>)`** labels — each process Sample now
  captures `/proc/[pid]/cwd`. The folder basename appears in the label
  so multiple parallel sessions are trivially identifiable.
- The `node …/happy/dist/index.mjs claude` wrapper now collapses to
  `Happy (<folder>)`. Daemon variants (`daemon start-sync`) become
  `Happy daemon`.

## v1.0.1

### Added
- **`SmartGit`** label for the bundled JRE + bootloader.jar combo.

## v1.0.0

### Added
- Initial release of the live process tree CPU monitor.
- Continuous sampling loop in the alternate screen buffer (so scrollback
  doesn't grow). `--interval`, `--rows`, `--help`.
- Two-section view: SESSION TOP CONSUMERS (cumulative) and LIVE TREE.
- Idle-middleman flattening: 0%-CPU non-collapse processes are hidden,
  their children promoted up the tree.
- Browser collapse for firefox/librewolf/chrome/chromium/opera/brave/
  vivaldi — `[<browser>, N procs] <argv>`.
- Claude Code labelling (`Claude Code` or `Claude Code (Happy)` when
  launched through the Happy wrapper, walking the real ppid chain).
- Zero external dependencies (plain `std` + `/proc`).
