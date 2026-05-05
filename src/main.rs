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

    Some(Sample {
        comm,
        cmdline_args,
        ppid,
        cpu_jiffies: utime + stime,
    })
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
        return if ancestor_via_happy(pid, snap) {
            "Claude Code (Happy)".to_string()
        } else {
            "Claude Code".to_string()
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
  -h, --help            Show this help and exit

Columns (leaderboard):
  PID     Process ID (or the collapse-root PID for browser subtrees)
  AVG%    Cumulative CPU usage since start, as % of one core
  NOW%    This frame's CPU usage, as % of one core
Columns (live tree):
  PID     Process ID
  %CPU    Share of total system CPU during the sample
  %CORE   `top`-style percent of one core (= %CPU × num_cpus)
  TREE    `[<browser>, N procs]` = whole browser subtree folded.

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

    let cpus = num_cpus();
    let mut prev_total = total_cpu_jiffies();
    let mut prev_snap = snapshot();

    // Session-cumulative jiffies per PID (resets only on program restart) so heavy
    // hitters don't disappear between frames just because they idled briefly.
    let mut cumulative: HashMap<u32, u64> = HashMap::new();
    let mut cumulative_total: u64 = 0;
    let leaderboard_n: usize = 8;

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
        let total_delta = cur_total.saturating_sub(prev_total).max(1);

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
            }
        }
        // Drop entries for PIDs that no longer exist; they're not actionable.
        cumulative.retain(|pid, _| cur_snap.contains_key(pid));

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
        writeln!(
            out,
            "monitor — {} ms · {} CPU(s) · {} procs · ~{:.0}s tracked · Ctrl+C to quit",
            interval_ms,
            cpus,
            cur_snap.len(),
            session_secs
        )?;

        // ── Section 1: Session leaderboard ────────────────────────────────
        writeln!(out, "\nSESSION TOP CONSUMERS  (cumulative since program start)")?;
        writeln!(
            out,
            "{:>7}  {:>7}  {:>7}  {}",
            "PID", "AVG%", "NOW%", "COMMAND"
        )?;
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
            let line_prefix = format!(
                "{:>7}  {:>6.1}%  {:>6.1}%  ",
                pid, avg_pct_core, now_pct_core
            );
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
    }
}
