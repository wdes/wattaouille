# wattaouille

> *watt + ratatouille + ouille! (ouch!)*  — a French pun for the moment
> you find out what's been eating your laptop's battery.

A live, friendly **CPU and energy** monitor for Linux laptops, written
in Rust with **zero dependencies** — just `std` and `/proc`/`/sys`.

It started as "what's making my ThinkPad's fan spin?" and grew into
something that answers, in plain language:

- **What's eating my CPU right now?**
- **What has been eating it across this whole session?**
- **How many watts is the package pulling, and which process is to blame?**
- **Is the battery seeing more draw than the CPU explains? By how much?**
- **How long until the battery dies at the current rate?**

If you've ever stared at `htop` and wished it would just *tell* you that
the spinning thing was Librewolf and not "300 lines of process tree", this
is for you.

```
wattaouille — 1500 ms · 12 CPU(s) · 715 procs · ~42s tracked
  · ⚡ RAPL 12.5 W avg · 0.146 Wh (525 J)
  · 🔋 BAT 18.0 W avg · 0.210 Wh (756 J) · 87% · 4h 23m left
  · Δ non-CPU +5.5 W avg · 0.064 Wh (231 J · +31%)

SESSION TOP CONSUMERS  (cumulative since program start)
    PID     AVG%     NOW%    NOW W      TOTAL W (J)   COMMAND
  18105    47.4%    48.5%   1.25W    0.65W (27J)     Claude Code (Happy: wattaouille)
  24040    44.1%    49.9%   1.10W    0.61W (26J)     Claude Code (Happy: action-pull-request-merge)
   4785    21.9%    25.3%   0.55W    0.30W (13J)     Guake terminal
   4560    17.6%    17.6%   0.45W    0.24W (10J)     [librewolf, 50 procs] librewolf …
   3653     8.6%    10.6%   0.21W    0.12W (5J)      Xorg :0 -seat seat0 …

LIVE TREE  (this 1500 ms sample)
    PID     %CPU    %CORE  TREE / COMMAND
   4785     2.1%    25.3%  Guake terminal
  18063     0.4%     4.7%  ├─ Happy (wattaouille)
  18105     4.0%    48.5%  │  └─ Claude Code (Happy: wattaouille)
  24040     4.2%    49.9%  ├─ Claude Code (Happy: dotfiles)
  17274     0.4%     5.3%  └─ Claude Code (Happy: action-pull-request-merge)
   3653     0.9%    10.6%  Xorg :0 …
   4560     0.2%     2.7%  [librewolf, 50 procs] librewolf …
  28421     0.1%     1.0%  [opera, 42 procs] opera
  29173     0.1%     1.0%  mysqld (projA)
  29286     0.1%     1.0%  mysqld (projB)
```

## Quick start

```bash
git clone <this repo> wattaouille && cd wattaouille
cargo build --release
./target/release/wattaouille
```

That's it. No npm, no pip, no `apt install`, no daemon. One static binary.

```bash
cargo test --release   # 27 unit tests, no /proc dependence
```

## What makes it different

### It hides the boring stuff

`init → lightdm → lightdm-session-child → xfce4-session → bash → bash → bash → node → node → node → claude.exe` — eight rows of context, one busy process. Most monitors print all of it.

`wattaouille` flattens any 0%-CPU "middleman" out of the tree and surfaces the actual busy leaf at the top level. The result is a tree that fits on one screen and tells you *who*, not *via what chain*.

### It collapses browser worker farms

Modern browsers spawn 40+ helper processes. They make the tree unreadable.

`wattaouille` knows about Firefox/Librewolf, Chrome/Chromium, Opera, Brave and Vivaldi. The whole subtree folds into a single line:

```
4560   17.6%   2.7%   [librewolf, 50 procs]   librewolf --sm-client-id …
```

CPU and watts shown are the **aggregate** across the whole subtree.

### It speaks human about the things you actually run

- `claude.exe --session-id 83df5645-… --append-system-prompt ALWAYS when you start a new chat … (370 chars)` becomes `Claude Code (Happy: wattaouille)`. The folder name is the session, taken straight from `/proc/[pid]/cwd`.
- `node /home/you/.config/yarn/global/node_modules/happy/dist/index.mjs claude` becomes `Happy (wattaouille)`. Daemon variants become `Happy daemon`.
- `/usr/share/smartgit/jre/bin/java -XX:+UseG1GC … -jar /usr/share/smartgit/lib/bootloader.jar` becomes `SmartGit`. (Yes, that's all it ever needed to say.)
- `python3 /usr/bin/guake` becomes `Guake terminal`.
- `mysqld --sql_mode=` running with cwd `/var/lib/mysql/projA` becomes `mysqld (projA)` — perfect when you're juggling multiple instances.

### It remembers

CPU readings refresh every 1.5 s by default — without memory, the busiest process keeps moving up and down the list. Hard to spot.

`wattaouille` keeps a session-cumulative leaderboard at the top of every frame: `AVG%` is your real heavy hitter, `NOW%` is the live spike, and the row sticks even when the process briefly idles.

### It tells you watts

If the kernel lets it (mode 0400 on RAPL since the Platypus side-channel disclosure — see [Setup](#setup-wattage-optional)), `wattaouille` reads Intel RAPL package energy and computes:

- **Total package draw** in W avg, Wh and J for the session
- **Per-process W and J** by scaling the package total by each process's CPU share (same trick Scaphandre uses — silicon doesn't actually report per-PID wattage)

If the kernel doesn't let it, `wattaouille` still works: a single warning line, then everything-but-watts.

### It cross-checks against your battery

Unplug the laptop and `wattaouille` reads `/sys/class/power_supply/BAT*/power_now` and shows it next to RAPL, including:

- **Average wattage** and **Wh consumed** while discharging
- **Current state of charge** (e.g. `87%`)
- **Time-to-empty** at the current draw (e.g. `4h 23m left`)
- **Drift** — `Δ non-CPU +5.5 W avg · 0.064 Wh (231 J · +31%)` — the battery draw the CPU package doesn't account for: screen, RAM, NVMe, Wi-Fi, USB. The drift is shown as a rate (W avg) and as totals (Wh/J), so you can read it as "the rest of the system is currently pulling about 5 W."

Plug back in and BAT switches to `🔌 on AC · 87%` and stops accumulating drift data.

## Usage

```
wattaouille [OPTIONS]

  -i, --interval <MS>   Sampling interval in milliseconds [default: 1500]
  -n, --rows <N>        Total rows budget per frame       [default: 50]
      --no-power        Force wattage off (preview new-user fallback path)
  -h, --help            Show full help and exit
```

Hit `Ctrl+C` to quit. The terminal goes back to whatever was on screen before — `wattaouille` runs in the alternate screen buffer (the same trick `htop`, `vim` and `less` use), so your scrollback isn't polluted.

## Setup (wattage, optional)

Modern kernels make RAPL counters root-readable only. To enable per-process wattage as your user:

**One-shot for this boot:**

```bash
sudo chmod a+r /sys/class/powercap/intel-rapl:0/energy_uj \
     /sys/class/powercap/intel-rapl/intel-rapl:0/intel-rapl:0:*/energy_uj
```

**Persist across reboots (udev rule):**

```bash
echo 'SUBSYSTEM=="powercap", ACTION=="add", RUN+="/bin/chmod a+r /sys%p/energy_uj"' \
  | sudo tee /etc/udev/rules.d/60-rapl-readable.rules
sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=powercap
```

**Or just run with sudo:**

```bash
sudo ./target/release/wattaouille
```

If RAPL isn't readable at startup, `wattaouille` prints these instructions to your scrollback and waits for **Enter** before running. Try `wattaouille --no-power` to preview that path even on a working setup.

## Reading the columns

**Header:**

| Field | Meaning |
| --- | --- |
| `1500 ms` | Sampling interval |
| `12 CPU(s)` | Logical cores in `/proc/stat` |
| `715 procs` | Live processes this frame |
| `~42s tracked` | Cumulative wall time of the session |
| `⚡ RAPL X W avg · Y Wh (Z J)` | Average package wattage and total energy since start |
| `🔋 BAT X W avg · Y Wh (Z J) · N% · Hh Mm left` | Battery side: avg draw, total energy used, state of charge, time-to-empty |
| `🔌 on AC · N%` | Same field while plugged in (state of charge only) |
| `Δ non-CPU +X W avg · Y Wh (Z J · ±N%)` | Battery draw NOT accounted for by RAPL (display + RAM + …) |

**Session leaderboard:**

| Column | Meaning |
| --- | --- |
| `PID` | Process ID (or the collapse-root PID for browser subtrees) |
| `AVG%` | Cumulative CPU since start, as % of one core |
| `NOW%` | This frame's CPU, as % of one core |
| `NOW W` | Estimated watts this frame |
| `TOTAL W (J)` | Average wattage and total joules since start |

**Live tree:**

| Column | Meaning |
| --- | --- |
| `PID` | Process ID |
| `%CPU` | Share of total system CPU during the sample |
| `%CORE` | top-style percent of one core |
| `TREE` | Process tree. `[<browser>, N procs]` = whole browser subtree folded. |

## How does it work?

Per frame:

1. Read `/proc/[pid]/{stat,cmdline,cwd}` for every PID. ~1 ms.
2. Diff against the previous snapshot to get jiffies-per-process and `/proc/stat`'s aggregate to get total system jiffies.
3. Read `/sys/class/powercap/intel-rapl:0/energy_uj` (twice, before and after the sleep). Difference / interval = package watts.
4. Read `/sys/class/power_supply/BAT*/{power_now, energy_now, energy_full}` and the AC `online` flag. Compute battery wattage, state of charge, and time-to-empty.
5. Distribute package watts to processes by CPU share.
6. Walk the tree, hide 0% middlemen, collapse browser subtrees, sort each level by subtree CPU desc.
7. Update session-cumulative jiffies and joules per PID.
8. Render leaderboard + tree, cap at `--rows`, flush.

Total external dependencies: **zero.** `Cargo.toml` lists none.

## Tests

```bash
cargo test --release
```

Twenty-seven unit tests cover the things that don't need `/proc`:
process labelling (Claude Code, Happy, SmartGit, Guake, mysqld), browser
collapse detection, RAPL counter wraparound, the `flatten_visible`
tree-promotion rules, the `XhYm` time formatter, and `cwd_basename` edge
cases. Run them in CI; they don't touch real sysfs.

## Limitations

- **Linux only.** It's all `/proc` and `/sys`.
- **Per-process watts are estimates.** No silicon reports per-PID power. We attribute by CPU share; if your process pegs a core, it gets the watt credit. iGPU work shows up under the iGPU domain (see `/sys/class/powercap/intel-rapl:0:1/uncore`) which we don't currently break out.
- **AMD/ARM:** the kernel exposes RAPL only on Intel. AMD's `amd_energy` driver lives elsewhere; not yet supported.

## License

Whatever you want, basically. (Drop a real license file if you fork.)

---

*Written while listening to Indila's "Mini World":*

> *mini world mini..mini world mini*

Sometimes a song gets stuck in your head and you write its name into a
README to make it stop. It does not work, but you do feel better. ✨
