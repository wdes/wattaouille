use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::thread;
use std::time::Duration;

struct Sample {
    comm: String,
    cmdline_args: Vec<String>,
    ppid: u32,
    cpu_jiffies: u64,
    cwd: Option<String>,
}

fn read_proc_stat(pid: &str) -> Option<Sample> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let lparen = stat.find('(')?;
    let rparen = stat.rfind(')')?;
    let comm = stat[lparen + 1..rparen].to_string();
    let rest: Vec<&str> = stat[rparen + 2..].split_whitespace().collect();
    // After comm: 0=state, 1=ppid, 11=utime, 12=stime
    let ppid: u32 = rest.get(1)?.parse().ok()?;
    let utime: u64 = rest.get(11)?.parse().ok()?;
    let stime: u64 = rest.get(12)?.parse().ok()?;

    let cmdline_raw = fs::read_to_string(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let cmdline_args: Vec<String> = cmdline_raw
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();

    let cwd = fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .and_then(|p| p.to_str().map(String::from));

    Some(Sample {
        comm,
        cmdline_args,
        ppid,
        cpu_jiffies: utime + stime,
        cwd,
    })
}

fn cwd_basename(sample: &Sample) -> Option<&str> {
    sample
        .cwd
        .as_deref()
        .and_then(|p| p.rsplit('/').find(|s| !s.is_empty()))
}

fn is_claude_code(sample: &Sample) -> bool {
    let head = match sample.cmdline_args.first() {
        Some(h) => h.as_str(),
        None => return false,
    };
    head.ends_with("/claude.exe")
        || head.ends_with("/claude")
        || head.contains("@anthropic-ai/claude-code")
}

fn is_smartgit(sample: &Sample) -> bool {
    sample
        .cmdline_args
        .iter()
        .any(|a| a.contains("/smartgit/") || a.ends_with("/smartgit.sh"))
}

/// The `node …/happy/dist/index.mjs <cmd>` wrapper script (or its launcher
/// shim under `happy/scripts/`). Returns the sub-command (`claude`, `daemon`,
/// …) so we can distinguish daemons from session wrappers.
fn happy_subcommand(sample: &Sample) -> Option<&str> {
    let mut args = sample.cmdline_args.iter();
    let _argv0 = args.next()?;
    let mut script = None;
    let mut subcmd = None;
    for arg in args {
        if script.is_none() {
            if arg.contains("/happy/dist/") || arg.contains("/happy/scripts/") {
                script = Some(arg);
            }
        } else {
            subcmd = Some(arg.as_str());
            break;
        }
    }
    script?;
    Some(subcmd.unwrap_or(""))
}

/// True if any ancestor (via real ppid in /proc) was launched through Happy.
fn ancestor_via_happy(pid: u32, snap: &HashMap<u32, Sample>) -> bool {
    let mut next = snap.get(&pid).map(|s| s.ppid).unwrap_or(0);
    while next != 0 {
        let Some(parent) = snap.get(&next) else { break };
        for arg in &parent.cmdline_args {
            // Match `happy` as an argv token, the yarn shim path, the npm
            // installed module path, or its launcher script.
            if arg == "happy"
                || arg.ends_with("/happy")
                || arg.contains("/happy/dist/")
                || arg.contains("/happy/scripts/")
                || arg.contains("/.yarn/bin/happy")
            {
                return true;
            }
        }
        next = parent.ppid;
    }
    false
}

/// Show the executable's basename + remaining args; fall back to comm in brackets
/// for kernel threads and zombies that have no cmdline. Claude Code processes
/// get a much shorter label (with "(Happy)" if launched via Happy).
fn pretty_cmdline(pid: u32, sample: &Sample, snap: &HashMap<u32, Sample>) -> String {
    if is_claude_code(sample) {
        let happy = ancestor_via_happy(pid, snap);
        return match (happy, cwd_basename(sample)) {
            (true, Some(folder)) => format!("Claude Code (Happy: {folder})"),
            (true, None) => "Claude Code (Happy)".to_string(),
            (false, Some(folder)) => format!("Claude Code ({folder})"),
            (false, None) => "Claude Code".to_string(),
        };
    }
    if let Some(subcmd) = happy_subcommand(sample) {
        return match (subcmd, cwd_basename(sample)) {
            ("daemon", _) => "Happy daemon".to_string(),
            (_, Some(folder)) => format!("Happy ({folder})"),
            (_, None) => "Happy".to_string(),
        };
    }
    if is_smartgit(sample) {
        return "SmartGit".to_string();
    }
    if sample.cmdline_args.is_empty() {
        return format!("[{}]", sample.comm);
    }
    let head = &sample.cmdline_args[0];
    let head_base = head.rsplit('/').next().unwrap_or(head);
    if sample.cmdline_args.len() > 1 {
        format!("{} {}", head_base, sample.cmdline_args[1..].join(" "))
    } else {
        head_base.to_string()
    }
}

fn snapshot() -> HashMap<u32, Sample> {
    let mut out = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else { continue };
        let Ok(pid) = name_str.parse::<u32>() else { continue };
        if let Some(sample) = read_proc_stat(name_str) {
            out.insert(pid, sample);
        }
    }
    out
}

fn total_cpu_jiffies() -> u64 {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    let first = stat.lines().next().unwrap_or("");
    first
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse::<u64>().ok())
        .sum()
}

/// Reads `/sys/class/power_supply/BAT*/power_now` (instantaneous draw in µW)
/// and the AC adapter online flag, so we can compare it to the RAPL package
/// total. RAPL only accounts for the CPU package; the battery sees the full
/// system (display, RAM, NVMe, Wi-Fi, etc.), so BAT will normally exceed RAPL
/// when discharging, and the gap is "rest-of-system" draw.
struct BatterySensor {
    power_now_path: Option<String>,
    status_path: Option<String>,
    ac_online_path: Option<String>,
}

impl BatterySensor {
    fn detect() -> Self {
        let mut power_now_path = None;
        let mut status_path = None;
        if let Ok(entries) = fs::read_dir("/sys/class/power_supply") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if name.starts_with("BAT") {
                    let pn = format!("/sys/class/power_supply/{name}/power_now");
                    if fs::metadata(&pn).is_ok() && power_now_path.is_none() {
                        power_now_path = Some(pn);
                        status_path = Some(format!("/sys/class/power_supply/{name}/status"));
                    }
                }
            }
        }
        let ac_online_path = ["AC", "ACAD", "ADP1"]
            .iter()
            .map(|n| format!("/sys/class/power_supply/{n}/online"))
            .find(|p| fs::metadata(p).is_ok());
        Self {
            power_now_path,
            status_path,
            ac_online_path,
        }
    }

    fn watts_now(&self) -> Option<f64> {
        let p = self.power_now_path.as_ref()?;
        let raw: i64 = fs::read_to_string(p).ok()?.trim().parse().ok()?;
        Some(raw.unsigned_abs() as f64 / 1_000_000.0)
    }

    fn discharging(&self) -> bool {
        if let Some(p) = &self.ac_online_path {
            if let Ok(s) = fs::read_to_string(p) {
                return s.trim() == "0";
            }
        }
        if let Some(p) = &self.status_path {
            if let Ok(s) = fs::read_to_string(p) {
                return s.trim().eq_ignore_ascii_case("Discharging");
            }
        }
        false
    }
}

/// Reads Intel RAPL package-0 energy counters. The kernel exposes them under
/// `/sys/class/powercap/intel-rapl:0/energy_uj` as a free-running microjoule
/// counter; the difference between two reads divided by the interval gives
/// average watts. The file is mode 0400 (root-only) since the Platypus
/// side-channel disclosure, so we degrade gracefully when not readable.
struct PowerSensor {
    energy_path: String,
    max_uj: u64,
    enabled: bool,
    disabled_reason: Option<String>,
}

impl PowerSensor {
    const PATH: &'static str = "/sys/class/powercap/intel-rapl:0/energy_uj";
    const WRAP_PATH: &'static str = "/sys/class/powercap/intel-rapl:0/max_energy_range_uj";
    const DOMAIN_DIR: &'static str = "/sys/class/powercap";

    fn detect(force_off: bool) -> Self {
        let max_uj = fs::read_to_string(Self::WRAP_PATH)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        if force_off {
            return Self {
                energy_path: Self::PATH.to_string(),
                max_uj,
                enabled: false,
                disabled_reason: Some("forced off via --no-power".to_string()),
            };
        }
        match fs::read_to_string(Self::PATH) {
            Ok(_) => Self {
                energy_path: Self::PATH.to_string(),
                max_uj,
                enabled: true,
                disabled_reason: None,
            },
            Err(e) => Self {
                energy_path: Self::PATH.to_string(),
                max_uj,
                enabled: false,
                disabled_reason: Some(format!("{e}")),
            },
        }
    }

    /// Diagnose why the sensor is off and return a multi-line block of
    /// instructions tailored to whichever cause the kernel reported.
    fn instructions(&self) -> String {
        let reason = self
            .disabled_reason
            .clone()
            .unwrap_or_else(|| "unknown".into());

        // What's actually on disk? Helps the user sanity-check before chmod.
        let rapl_present = fs::metadata("/sys/class/powercap/intel-rapl:0").is_ok();
        let powercap_present = fs::metadata(Self::DOMAIN_DIR).is_ok();

        let mut s = String::new();
        s.push_str("⚡ Wattage disabled\n");
        s.push_str(&format!("   reason: {reason}\n"));
        if !powercap_present {
            s.push_str(
                "   /sys/class/powercap is missing — your kernel was built without\n   \
                 CONFIG_POWERCAP. No RAPL access is possible on this system.\n",
            );
            return s;
        }
        if !rapl_present {
            s.push_str(
                "   /sys/class/powercap/intel-rapl:0 is missing — likely an AMD or ARM\n   \
                 CPU. Try `ls /sys/class/powercap` to see what's available; AMD\n   \
                 systems may expose `amd_energy` instead.\n",
            );
            return s;
        }
        // RAPL exists but isn't readable — almost always the Platypus mitigation.
        s.push_str("   RAPL counters exist but are mode 0400 (root-only).\n");
        s.push_str("   Pick one to enable wattage:\n\n");
        s.push_str("     # 1. One-shot for this boot:\n");
        s.push_str(
            "     sudo chmod a+r /sys/class/powercap/intel-rapl:0/energy_uj \\\n          \
             /sys/class/powercap/intel-rapl/intel-rapl:0/intel-rapl:0:*/energy_uj\n\n",
        );
        s.push_str("     # 2. Persist via udev (survives reboots):\n");
        s.push_str("     echo 'SUBSYSTEM==\"powercap\", ACTION==\"add\", \\\n");
        s.push_str(
            "       RUN+=\"/bin/chmod a+r /sys%p/energy_uj\"' | \\\n         \
             sudo tee /etc/udev/rules.d/60-rapl-readable.rules\n     \
             sudo udevadm control --reload && sudo udevadm trigger --subsystem-match=powercap\n\n",
        );
        s.push_str("     # 3. Or just run monitor with sudo.\n");
        s
    }

    fn read_uj(&self) -> Option<u64> {
        if !self.enabled {
            return None;
        }
        fs::read_to_string(&self.energy_path)
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// Returns joules consumed between `before` and `after`, handling the
    /// counter wrap at `max_uj`.
    fn joules_between(&self, before: u64, after: u64) -> f64 {
        let delta_uj = if after >= before {
            after - before
        } else {
            self.max_uj.saturating_sub(before).saturating_add(after)
        };
        delta_uj as f64 / 1_000_000.0
    }
}

fn num_cpus() -> u64 {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    stat.lines()
        .filter(|l| l.starts_with("cpu") && !l.starts_with("cpu "))
        .count()
        .max(1) as u64
}

fn term_cols() -> usize {
    env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(160)
}

fn subtree_delta(
    pid: u32,
    deltas: &HashMap<u32, u64>,
    children: &HashMap<u32, Vec<u32>>,
) -> u64 {
    let mut total = *deltas.get(&pid).unwrap_or(&0);
    if let Some(kids) = children.get(&pid) {
        for c in kids {
            total += subtree_delta(*c, deltas, children);
        }
    }
    total
}

/// Processes whose entire subtree should fold into the one line we already
/// print for them. Major browsers (firefox/librewolf, chrome/chromium, opera,
/// brave, vivaldi) spawn dozens of helpers that are noisy individually.
fn is_collapse_root(sample: &Sample) -> bool {
    const BROWSERS: &[&str] = &[
        "librewolf",
        "firefox",
        "chrome",
        "chromium",
        "chromium-browse",
        "opera",
        "brave",
        "brave-browser",
        "vivaldi",
        "vivaldi-bin",
    ];
    if !BROWSERS.contains(&sample.comm.as_str()) {
        return false;
    }
    // The "main" browser process has no subprocess-type flag; workers do.
    // Chromium uses `--type=renderer/gpu-process/zygote/utility/...`,
    // Firefox/derivatives use `-contentproc`.
    !sample
        .cmdline_args
        .iter()
        .any(|a| a.starts_with("--type=") || a == "-contentproc")
}

/// Hide any process that used 0% CPU during the sample by replacing it with
/// its (recursively flattened) visible descendants. Browser collapse roots and
/// the kernel-thread placeholder `kthreadd` are kept so the user can see them.
/// Dead subtrees (zero CPU anywhere underneath) are dropped.
fn flatten_visible(
    candidates: &[u32],
    snap: &HashMap<u32, Sample>,
    deltas: &HashMap<u32, u64>,
    children: &HashMap<u32, Vec<u32>>,
    subtree: &HashMap<u32, u64>,
) -> Vec<u32> {
    let mut out = Vec::new();
    for &pid in candidates {
        if subtree.get(&pid).copied().unwrap_or(0) == 0 {
            continue;
        }
        let Some(sample) = snap.get(&pid) else { continue };
        let own = deltas.get(&pid).copied().unwrap_or(0);
        if own == 0 && !is_collapse_root(sample) {
            let kids = children.get(&pid).map(|v| v.as_slice()).unwrap_or(&[]);
            out.extend(flatten_visible(kids, snap, deltas, children, subtree));
        } else {
            out.push(pid);
        }
    }
    out
}

fn count_descendants(
    pid: u32,
    children: &HashMap<u32, Vec<u32>>,
    deltas: &HashMap<u32, u64>,
) -> (usize, u64) {
    let mut count = 0usize;
    let mut total = 0u64;
    if let Some(kids) = children.get(&pid) {
        for c in kids {
            count += 1;
            total += deltas.get(c).copied().unwrap_or(0);
            let (sub_c, sub_t) = count_descendants(*c, children, deltas);
            count += sub_c;
            total += sub_t;
        }
    }
    (count, total)
}

#[allow(clippy::too_many_arguments)]
fn print_node(
    out: &mut impl Write,
    pid: u32,
    prefix: &str,
    is_last: bool,
    is_root: bool,
    snap: &HashMap<u32, Sample>,
    deltas: &HashMap<u32, u64>,
    subtree: &HashMap<u32, u64>,
    children: &HashMap<u32, Vec<u32>>,
    total_delta: u64,
    cpus: u64,
    cols: usize,
    rows: &mut usize,
    max_rows: usize,
) -> io::Result<()> {
    if *rows >= max_rows {
        return Ok(());
    }
    let Some(sample) = snap.get(&pid) else {
        return Ok(());
    };
    let collapse = is_collapse_root(sample);

    // For collapsed nodes, show aggregated CPU across the whole subtree on the
    // single browser line (instead of just the main process's own jiffies).
    let delta = if collapse {
        subtree.get(&pid).copied().unwrap_or(0)
    } else {
        deltas.get(&pid).copied().unwrap_or(0)
    };
    let pct_total = (delta as f64 / total_delta as f64) * 100.0;
    let pct_core = pct_total * cpus as f64;

    let branch = if is_root {
        ""
    } else if is_last {
        "└─ "
    } else {
        "├─ "
    };
    let mut label = pretty_cmdline(pid, sample, snap);
    if collapse {
        let (descendants, _) = count_descendants(pid, children, deltas);
        label = format!("[{}, {} procs] {}", sample.comm, descendants + 1, label);
    }
    let line_prefix = format!(
        "{:>7}  {:>6.1}%  {:>6.1}%  {}{}",
        pid, pct_total, pct_core, prefix, branch
    );
    let budget = cols.saturating_sub(line_prefix.chars().count()).max(10);
    let label_trunc: String = label.chars().take(budget).collect();
    writeln!(out, "{line_prefix}{label_trunc}")?;
    *rows += 1;

    if collapse {
        return Ok(());
    }

    let child_prefix = if is_root {
        String::new()
    } else if is_last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}│  ")
    };

    // Visible children: flatten any 0%-own-CPU descendants up so idle middlemen
    // don't show, and sort by subtree delta so the busiest path is at the top.
    let raw_kids = children.get(&pid).map(|v| v.as_slice()).unwrap_or(&[]);
    let mut visible = flatten_visible(raw_kids, snap, deltas, children, subtree);
    visible.sort_by(|a, b| {
        subtree
            .get(b)
            .unwrap_or(&0)
            .cmp(subtree.get(a).unwrap_or(&0))
            .then_with(|| a.cmp(b))
    });
    let last_idx = visible.len().saturating_sub(1);
    for (i, child) in visible.iter().enumerate() {
        if *rows >= max_rows {
            break;
        }
        print_node(
            out,
            *child,
            &child_prefix,
            i == last_idx,
            false,
            snap,
            deltas,
            subtree,
            children,
            total_delta,
            cpus,
            cols,
            rows,
            max_rows,
        )?;
    }
    Ok(())
}

fn print_help(prog: &str) {
    println!(
        "Usage: {prog} [OPTIONS]

Two-section view of CPU usage:
  • SESSION TOP CONSUMERS — cumulative since the program started, so heavy
    hitters stay visible even when they idle for a frame.
  • LIVE TREE — this sample's process tree. Idle (0%-CPU) middlemen are
    hidden, and browser subtrees (firefox, chrome, opera, …) collapse into
    a single line.

Claude Code processes are labelled `Claude Code` (or `Claude Code (Happy)`
when launched through the Happy wrapper).

Options:
  -i, --interval <MS>   Sampling interval in milliseconds [default: 1500]
  -n, --rows <N>        Total rows budget per frame       [default: 50]
      --no-power        Force wattage off (test the new-user fallback path)
  -h, --help            Show this help and exit

Columns (leaderboard):
  PID     Process ID (or the collapse-root PID for browser subtrees)
  AVG%    Cumulative CPU usage since start, as % of one core
  NOW%    This frame's CPU usage, as % of one core
  NOW W       Estimated watts this frame (proc's CPU share × package power)
  TOTAL W (J) Cumulative average wattage and total joules since start
Columns (live tree):
  PID     Process ID
  %CPU    Share of total system CPU during the sample
  %CORE   `top`-style percent of one core (= %CPU × num_cpus)
  TREE    `[<browser>, N procs]` = whole browser subtree folded.

Wattage:
  Total package wattage is read from Intel RAPL (`/sys/class/powercap/
  intel-rapl:0/energy_uj`). Per-process watts are estimated by scaling the
  package total by each process's CPU share. RAPL files are root-only by
  default; when monitor can't read them it prints setup instructions and
  pauses for confirmation, then runs without the W/J columns.

  Pass `--no-power` to simulate the disabled path even when RAPL is
  readable — useful for previewing what a new user sees.

Battery cross-check:
  When discharging, monitor also reads `/sys/class/power_supply/BAT*/
  power_now` and shows BAT watts alongside RAPL watts. The drift figure
  (Δ%) is the share of battery energy NOT accounted for by RAPL — that's
  display, RAM, NVMe, Wi-Fi, etc. Plug back in and the BAT readout
  switches to `🔌 on AC`.

Press Ctrl+C to quit."
    );
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        let prog = args
            .first()
            .map(|s| s.rsplit('/').next().unwrap_or(s.as_str()))
            .unwrap_or("monitor");
        print_help(prog);
        return Ok(());
    }
    let interval_ms: u64 = args
        .iter()
        .position(|a| a == "-i" || a == "--interval")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(1500);
    let max_rows: usize = args
        .iter()
        .position(|a| a == "-n" || a == "--rows")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let force_no_power = args.iter().any(|a| a == "--no-power");

    let cpus = num_cpus();
    let mut prev_total = total_cpu_jiffies();
    let mut prev_snap = snapshot();

    let power = PowerSensor::detect(force_no_power);
    let battery = BatterySensor::detect();
    let mut prev_energy_uj = power.read_uj();
    let mut cumulative_joules: HashMap<u32, f64> = HashMap::new();
    let mut total_joules: f64 = 0.0;
    // Battery vs RAPL drift accounting — only accumulates while discharging.
    let mut bat_joules: f64 = 0.0;
    let mut rapl_joules_while_discharging: f64 = 0.0;
    let mut discharging_secs: f64 = 0.0;
    let mut elapsed_secs: f64 = 0.0;

    // Session-cumulative jiffies per PID (resets only on program restart) so heavy
    // hitters don't disappear between frames just because they idled briefly.
    let mut cumulative: HashMap<u32, u64> = HashMap::new();
    let mut cumulative_total: u64 = 0;
    let leaderboard_n: usize = 8;

    // Print the multi-line setup block ONCE, before entering the alt screen,
    // so it stays visible in the user's normal scrollback and on Ctrl+C they
    // can scroll up to copy the chmod / udev commands.
    if !power.enabled {
        eprint!("{}", power.instructions());
        eprintln!("   Press Enter to continue without wattage, or Ctrl+C to abort.");
        let _ = io::stdin().read_line(&mut String::new());
    }

    let stdout = io::stdout();
    let mut out = stdout.lock();
    // Enter alt screen buffer + hide cursor so scrollback doesn't grow and the
    // terminal is restored on Ctrl+C (most terminals reset DECSET on process exit).
    write!(out, "\x1B[?1049h\x1B[?25l\x1B[H\x1B[2J")?;
    out.flush()?;

    loop {
        thread::sleep(Duration::from_millis(interval_ms));

        let cur_total = total_cpu_jiffies();
        let cur_snap = snapshot();
        let cur_energy_uj = power.read_uj();
        let total_delta = cur_total.saturating_sub(prev_total).max(1);

        // Joules spent across the whole CPU package during the sample.
        let frame_joules = match (prev_energy_uj, cur_energy_uj) {
            (Some(b), Some(a)) => power.joules_between(b, a),
            _ => 0.0,
        };
        let interval_secs = interval_ms as f64 / 1000.0;
        let frame_watts = frame_joules / interval_secs;
        total_joules += frame_joules;
        elapsed_secs += interval_secs;

        // Sample battery draw (only meaningful while on battery).
        let bat_watts = battery.watts_now();
        let discharging = battery.discharging();
        if discharging {
            discharging_secs += interval_secs;
            if let Some(w) = bat_watts {
                bat_joules += w * interval_secs;
                if power.enabled {
                    rapl_joules_while_discharging += frame_joules;
                }
            }
        }

        let mut deltas: HashMap<u32, u64> = HashMap::with_capacity(cur_snap.len());
        for (pid, after) in &cur_snap {
            let d = match prev_snap.get(pid) {
                Some(before) => after.cpu_jiffies.saturating_sub(before.cpu_jiffies),
                None => 0,
            };
            deltas.insert(*pid, d);
        }

        let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
        for (pid, sample) in &cur_snap {
            children.entry(sample.ppid).or_default().push(*pid);
        }

        // Update session cumulative totals.
        cumulative_total = cumulative_total.saturating_add(total_delta);
        for (pid, d) in &deltas {
            if *d > 0 {
                *cumulative.entry(*pid).or_insert(0) += d;
                if frame_joules > 0.0 {
                    let share = *d as f64 / total_delta as f64;
                    *cumulative_joules.entry(*pid).or_insert(0.0) += frame_joules * share;
                }
            }
        }
        // Drop entries for PIDs that no longer exist; they're not actionable.
        cumulative.retain(|pid, _| cur_snap.contains_key(pid));
        cumulative_joules.retain(|pid, _| cur_snap.contains_key(pid));

        let subtree: HashMap<u32, u64> = cur_snap
            .keys()
            .map(|pid| (*pid, subtree_delta(*pid, &deltas, &children)))
            .collect();
        let subtree_cum: HashMap<u32, u64> = cur_snap
            .keys()
            .map(|pid| (*pid, subtree_delta(*pid, &cumulative, &children)))
            .collect();

        // Build map: descendant_pid → collapse_root_pid, so the leaderboard can
        // roll browser helpers up into one entry instead of listing 50 workers.
        let mut collapsed_into: HashMap<u32, u32> = HashMap::new();
        for (root_pid, sample) in &cur_snap {
            if !is_collapse_root(sample) {
                continue;
            }
            let mut stack = vec![*root_pid];
            while let Some(p) = stack.pop() {
                if let Some(kids) = children.get(&p) {
                    for &c in kids {
                        collapsed_into.insert(c, *root_pid);
                        stack.push(c);
                    }
                }
            }
        }

        // Sort siblings by subtree delta desc — busiest branches surface first.
        for kids in children.values_mut() {
            kids.sort_by(|a, b| {
                subtree
                    .get(b)
                    .unwrap_or(&0)
                    .cmp(subtree.get(a).unwrap_or(&0))
                    .then_with(|| a.cmp(b))
            });
        }

        let raw_roots: Vec<u32> = cur_snap
            .iter()
            .filter(|(_, s)| !cur_snap.contains_key(&s.ppid))
            .map(|(pid, _)| *pid)
            .collect();
        let mut roots = flatten_visible(&raw_roots, &cur_snap, &deltas, &children, &subtree);
        roots.sort_by(|a, b| {
            subtree
                .get(b)
                .unwrap_or(&0)
                .cmp(subtree.get(a).unwrap_or(&0))
                .then_with(|| a.cmp(b))
        });

        // Build leaderboard entries: one per process unless it's swallowed by a
        // collapse root; collapse roots get their subtree-aggregated totals.
        let mut board: Vec<(u32, u64, u64)> = Vec::new(); // (pid, cum_jiffies, now_jiffies)
        for (&pid, sample) in &cur_snap {
            if collapsed_into.contains_key(&pid) {
                continue;
            }
            let (cum, now) = if is_collapse_root(sample) {
                (
                    subtree_cum.get(&pid).copied().unwrap_or(0),
                    subtree.get(&pid).copied().unwrap_or(0),
                )
            } else {
                (
                    cumulative.get(&pid).copied().unwrap_or(0),
                    deltas.get(&pid).copied().unwrap_or(0),
                )
            };
            if cum == 0 {
                continue;
            }
            board.push((pid, cum, now));
        }
        board.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let cols = term_cols();
        // Home + clear (in alt screen buffer).
        write!(out, "\x1B[H\x1B[2J")?;
        let session_secs = (cumulative_total as f64) / (cpus as f64 * 100.0); // rough, jiffies are 1/100s on Linux
        let mut bits: Vec<String> = Vec::new();
        if power.enabled {
            // Average wattage over the whole session: total energy / time.
            // The per-frame instantaneous reading is still in the NOW W column
            // of the leaderboard.
            let avg_w = if elapsed_secs > 0.0 {
                total_joules / elapsed_secs
            } else {
                frame_watts
            };
            let total_wh = total_joules / 3600.0;
            bits.push(format!(
                "⚡ RAPL {:.1} W avg · {:.3} Wh ({:.0} J)",
                avg_w, total_wh, total_joules
            ));
        }
        if discharging {
            if let Some(_w) = bat_watts {
                let bat_avg_w = if discharging_secs > 0.0 {
                    bat_joules / discharging_secs
                } else {
                    0.0
                };
                let bat_wh = bat_joules / 3600.0;
                bits.push(format!(
                    "🔋 BAT {:.1} W avg · {:.3} Wh ({:.0} J)",
                    bat_avg_w, bat_wh, bat_joules
                ));
            }
        } else if battery.power_now_path.is_some() {
            bits.push("🔌 on AC".to_string());
        }
        // Show drift only after we've accumulated some data while discharging,
        // and only if both sensors contributed.
        if power.enabled && rapl_joules_while_discharging > 1.0 && bat_joules > 1.0 {
            let drift = (bat_joules - rapl_joules_while_discharging) / bat_joules * 100.0;
            bits.push(format!(
                "Δ {:+.0}% ({:.0} J non-CPU)",
                drift,
                bat_joules - rapl_joules_while_discharging
            ));
        }
        let power_summary = if bits.is_empty() {
            String::new()
        } else {
            format!(" · {}", bits.join(" · "))
        };
        writeln!(
            out,
            "monitor — {} ms · {} CPU(s) · {} procs · ~{:.0}s tracked{} · Ctrl+C to quit",
            interval_ms,
            cpus,
            cur_snap.len(),
            session_secs,
            power_summary
        )?;
        if !power.enabled {
            writeln!(
                out,
                "⚠ Wattage disabled ({}). Run with --help for setup.",
                power
                    .disabled_reason
                    .as_deref()
                    .unwrap_or("RAPL not readable")
            )?;
        }

        // ── Section 1: Session leaderboard ────────────────────────────────
        writeln!(out, "\nSESSION TOP CONSUMERS  (cumulative since program start)")?;
        if power.enabled {
            writeln!(
                out,
                "{:>7}  {:>7}  {:>7}  {:>7}  {:>16}  {}",
                "PID", "AVG%", "NOW%", "NOW W", "TOTAL W (J)", "COMMAND"
            )?;
        } else {
            writeln!(
                out,
                "{:>7}  {:>7}  {:>7}  {}",
                "PID", "AVG%", "NOW%", "COMMAND"
            )?;
        }
        for (pid, cum, now) in board.iter().take(leaderboard_n) {
            let Some(sample) = cur_snap.get(pid) else { continue };
            let avg_pct_core =
                (*cum as f64 / cumulative_total.max(1) as f64) * 100.0 * cpus as f64;
            let now_pct_core = (*now as f64 / total_delta as f64) * 100.0 * cpus as f64;
            let mut label = pretty_cmdline(*pid, sample, &cur_snap);
            if is_collapse_root(sample) {
                let (descendants, _) = count_descendants(*pid, &children, &deltas);
                label = format!("[{}, {} procs] {}", sample.comm, descendants + 1, label);
            }
            let line_prefix = if power.enabled {
                let now_w = if total_delta > 0 {
                    frame_watts * (*now as f64 / total_delta as f64)
                } else {
                    0.0
                };
                let total_j = if is_collapse_root(sample) {
                    // Sum descendants too so the browser line matches the
                    // jiffies aggregation it already uses.
                    let mut s = cumulative_joules.get(pid).copied().unwrap_or(0.0);
                    let mut stack = vec![*pid];
                    while let Some(p) = stack.pop() {
                        if let Some(kids) = children.get(&p) {
                            for &c in kids {
                                s += cumulative_joules.get(&c).copied().unwrap_or(0.0);
                                stack.push(c);
                            }
                        }
                    }
                    s
                } else {
                    cumulative_joules.get(pid).copied().unwrap_or(0.0)
                };
                let total_w = if elapsed_secs > 0.0 {
                    total_j / elapsed_secs
                } else {
                    0.0
                };
                let total_cell = format!("{:.2}W ({:.0}J)", total_w, total_j);
                format!(
                    "{:>7}  {:>6.1}%  {:>6.1}%  {:>6.2}W  {:>16}  ",
                    pid, avg_pct_core, now_pct_core, now_w, total_cell
                )
            } else {
                format!(
                    "{:>7}  {:>6.1}%  {:>6.1}%  ",
                    pid, avg_pct_core, now_pct_core
                )
            };
            let budget = cols.saturating_sub(line_prefix.chars().count()).max(10);
            let label_trunc: String = label.chars().take(budget).collect();
            writeln!(out, "{line_prefix}{label_trunc}")?;
        }

        // ── Section 2: Live tree (this frame) ─────────────────────────────
        writeln!(out, "\nLIVE TREE  (this {} ms sample)", interval_ms)?;
        writeln!(
            out,
            "{:>7}  {:>7}  {:>7}  {}",
            "PID", "%CPU", "%CORE", "TREE / COMMAND"
        )?;

        let header_lines = 6 + leaderboard_n.min(board.len());
        let tree_budget = max_rows.saturating_sub(header_lines).max(5);
        let mut rows = 0usize;
        let last_idx = roots.len().saturating_sub(1);
        for (i, root) in roots.iter().enumerate() {
            if rows >= tree_budget {
                break;
            }
            print_node(
                &mut out,
                *root,
                "",
                i == last_idx,
                true,
                &cur_snap,
                &deltas,
                &subtree,
                &children,
                total_delta,
                cpus,
                cols,
                &mut rows,
                tree_budget,
            )?;
        }
        out.flush()?;

        prev_total = cur_total;
        prev_snap = cur_snap;
        prev_energy_uj = cur_energy_uj;
    }
}
