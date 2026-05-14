use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

use wattaouille::{PowerSensor, Sample, num_cpus, snapshot, total_cpu_jiffies};

/// The terminal's termios state at startup. We capture it before flipping
/// stdin into non-canonical / non-echo mode and restore it on Ctrl+C so the
/// shell isn't left in raw mode if we crash. `OnceLock` is read-only after
/// the first set, which makes it safe to read from the signal handler thread
/// without locking.
static ORIGINAL_TERMIOS: OnceLock<libc::termios> = OnceLock::new();

/// Disable canonical mode and echo on stdin. Without this, mouse scroll
/// inside the alt screen sends arrow-key escape sequences (`\x1B[A`,
/// `\x1B[B`) which the kernel's tty driver echoes straight back into the
/// alt-screen view as visible garbage.
fn enter_raw_mode() {
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
            return; // not a tty (piped) — nothing to do
        }
        let _ = ORIGINAL_TERMIOS.set(t);
        t.c_lflag &= !(libc::ICANON | libc::ECHO);
        libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
    }
}

/// Put stdin back into whatever mode it was in at startup. Safe to call from
/// the ctrlc handler thread because `OnceLock::get` is lock-free.
fn restore_terminal_mode() {
    if let Some(t) = ORIGINAL_TERMIOS.get() {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t);
        }
    }
}

/// Install a Ctrl+C / SIGTERM handler that restores the terminal state
/// (exits the alt screen and reshows the cursor) before exiting. Without
/// this, SIGINT either tears down the process with the alt screen still
/// active (cursor hidden) or — once we hold a stdout lock in the render
/// loop — deadlocks if the handler also tries to lock stdout. We bypass
/// the Rust mutex by writing the restore sequence directly via libc::write
/// to fd 1; that's a single syscall and is safe to call from the ctrlc
/// crate's handler thread even while main holds the std lock.
fn install_signal_handlers() {
    unsafe extern "C" {
        fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    }
    const RESTORE: &[u8] = b"\x1B[?1049l\x1B[?25h\r\n";
    let _ = ctrlc::set_handler(|| {
        // Termios first (so the user's prompt has echo back when the alt
        // screen exits), then alt-screen + cursor escapes, then exit.
        restore_terminal_mode();
        unsafe {
            let _ = write(1, RESTORE.as_ptr(), RESTORE.len());
        }
        std::process::exit(130);
    });
}


fn cwd_basename(sample: &Sample) -> Option<&str> {
    sample
        .cwd
        .as_deref()
        .and_then(|p| p.rsplit('/').find(|s| !s.is_empty()))
}

fn is_claude_code(sample: &Sample) -> bool {
    let Some(head) = sample.cmdline_args.first() else {
        return false;
    };
    if head.ends_with("/claude.exe") || head.ends_with("/claude") {
        return true;
    }
    // Sometimes invoked indirectly as `node /path/@anthropic-ai/claude-code/cli.js …`
    sample
        .cmdline_args
        .iter()
        .any(|a| a.contains("@anthropic-ai/claude-code"))
}

fn is_smartgit(sample: &Sample) -> bool {
    sample
        .cmdline_args
        .iter()
        .any(|a| a.contains("/smartgit/") || a.ends_with("/smartgit.sh"))
}

fn is_guake(sample: &Sample) -> bool {
    sample.comm == "guake"
        || sample
            .cmdline_args
            .iter()
            .any(|a| a.ends_with("/guake") || a == "guake")
}

/// Friendly labels for processes whose comm alone identifies them, plus a few
/// argv-aware shortcuts that turn long unreadable command lines into a clean
/// human-readable name. Returns None when no rule matches; the caller falls
/// through to the generic basename+args formatting.
fn pretty_known(sample: &Sample) -> Option<String> {
    // /proc/[pid]/comm is truncated at 15 chars (TASK_COMM_LEN-1), so a few of
    // these match prefixes of the original program name.
    let by_comm: &str = match sample.comm.as_str() {
        // Desktop / window manager
        "xfdesktop" => "Xfce desktop",
        "xfce4-panel" => "Xfce panel",
        "xfce4-session" => "Xfce session",
        "xfwm4" => "Xfwm window manager",
        "xfsettingsd" => "Xfce settings daemon",
        "xfce4-power-mana" => "Xfce power manager",
        "xfce4-clipman" => "Xfce clipman",
        "xfce4-screensav" => "Xfce screensaver",
        "xfconfd" => "Xfce config daemon",
        "Thunar" => "Thunar (file manager)",
        "lightdm" => "LightDM",
        "Xorg" => "Xorg",
        // Input methods
        "ibus-daemon" => "IBus daemon",
        "ibus-ui-gtk3" => "IBus UI (gtk3)",
        "ibus-engine-sim" => "IBus engine (simple)",
        "ibus-extension-" => "IBus extension (gtk3)",
        "ibus-x11" => "IBus X11 bridge",
        // Audio
        "pipewire" => "PipeWire",
        "pipewire-pulse" => "PipeWire (pulse compat)",
        "wireplumber" => "WirePlumber",
        "pulseaudio" => "PulseAudio",
        // System bus / journal
        "dbus-daemon" => "dbus-daemon",
        "systemd-journal" => "systemd-journald",
        "systemd-logind" => "systemd-logind",
        "systemd-udevd" => "systemd-udevd",
        // Daemons the user runs
        "dockerd" => "Docker daemon",
        "containerd" => "containerd",
        "redis-server" => "Redis",
        "teamviewerd" => "TeamViewer daemon",
        "scdaemon" => "GnuPG scdaemon",
        "ntp-daemon" => "NTP daemon",
        "warp-taskbar" => "Cloudflare WARP",
        "NetworkManager" => "NetworkManager",
        "ModemManager" => "ModemManager",
        "bluetoothd" => "BlueZ (bluetoothd)",
        "tailscaled" => "Tailscale daemon",
        "wpa_supplicant" => "wpa_supplicant",
        "avahi-daemon" => "Avahi (mDNS)",
        // System service daemons
        "accounts-daemon" => "AccountsService",
        "polkitd" => "polkitd",
        "udisksd" => "UDisks2",
        "upowerd" => "UPower",
        "colord" => "ColorD (color profiles)",
        "rtkit-daemon" => "RTKit",
        "snapd" => "snapd",
        "cupsd" => "CUPS daemon",
        "cups-browsed" => "CUPS browser",
        "smartd" => "S.M.A.R.T daemon",
        "boltd" => "BoltD (Thunderbolt)",
        "xiccd" => "X ICC daemon (color)",
        "yubikey-touch-d" => "YubiKey touch detector",
        // Truncated to 15 chars by /proc — the trailing dash/character is real
        "power-profiles-" => "Power Profiles daemon",
        "xdg-desktop-por" => "XDG Desktop Portal",
        "xdg-document-po" => "XDG Document Portal",
        "xdg-permission-" => "XDG Permission Store",
        "switcheroo-cont" => "switcheroo (GPU offload)",
        "at-spi-bus-laun" => "AT-SPI bus launcher",
        "at-spi2-registr" => "AT-SPI registry",
        // Containers / sandboxes
        "rootlesskit" => "rootlesskit",
        "slirp4netns" => "slirp4netns",
        // Servers I run
        "caddy" => "Caddy",
        "apache2" => "Apache HTTPd",
        "crowdsec" => "CrowdSec",
        "anydesk" => "AnyDesk",
        // Login
        "agetty" => "agetty (TTY login)",
        // Solaar (Logitech)
        "solaar" => "Solaar (Logitech)",
        // Blueman (no path involved here, just the comm)
        "blueman-tray" => "Blueman (tray)",
        "blueman-applet" => "Blueman (applet)",
        // Smartgit launcher script
        "smartgit.sh" => "SmartGit (launcher)",
        "slack" => "Slack desktop",
        "Slack" => "Slack desktop",
        "discord" => "Discord",
        "Discord" => "Discord",
        "code" => "VS Code",
        "Code" => "VS Code",
        "spotify" => "Spotify",
        "thunderbird" => "Thunderbird",
        // Element (Matrix client) on Linux — comm is truncated to 15 chars.
        "element-desktop" => "Element (Matrix)",
        "Element" => "Element (Matrix)",
        _ => "",
    };
    if !by_comm.is_empty() {
        return Some(by_comm.to_string());
    }

    // containerd-shim-runc-v2 — comm is truncated to "containerd-shim". Pull
    // the container ID out of argv and show a short hash so multiple shims
    // are easy to tell apart at a glance.
    if sample.comm.starts_with("containerd-shim") {
        let mut iter = sample.cmdline_args.iter();
        let mut id: Option<&str> = None;
        while let Some(a) = iter.next() {
            if a == "-id" || a == "--id" {
                if let Some(next) = iter.next() {
                    id = Some(next.as_str());
                    break;
                }
            }
        }
        let short: String = id
            .map(|s| s.chars().take(8).collect())
            .unwrap_or_default();
        return Some(if short.is_empty() {
            "containerd-shim".to_string()
        } else {
            format!("containerd-shim ({short})")
        });
    }

    // wrapper-2.0 plugins for the Xfce panel: argv carries the .so path; the
    // plugin's library basename is the human name we want.
    if sample.comm == "wrapper-2.0" {
        let plugin = sample.cmdline_args.iter().find_map(|a| {
            if a.contains("/xfce4/panel/plugins/lib") && a.ends_with(".so") {
                let base = a.rsplit('/').next()?;
                Some(
                    base.trim_start_matches("lib")
                        .trim_end_matches(".so")
                        .to_string(),
                )
            } else {
                None
            }
        });
        return Some(match plugin {
            Some(p) => format!("Xfce panel plugin ({p})"),
            None => "Xfce panel plugin".to_string(),
        });
    }

    // Blueman tray apps: launched as `python3 /usr/bin/blueman-tray`.
    if matches!(sample.comm.as_str(), "python3" | "python") {
        if let Some(blue) = sample
            .cmdline_args
            .iter()
            .find(|a| a.contains("/blueman-"))
        {
            let base = blue.rsplit('/').next().unwrap_or(blue);
            let kind = base.trim_start_matches("blueman-");
            return Some(format!("Blueman ({kind})"));
        }
    }

    // Angular CLI dev server: `node …/ng serve --port=4200 …`
    if sample.comm == "node" || sample.comm == "ng" {
        if sample.cmdline_args.iter().any(|a| a == "ng" || a.ends_with("/ng"))
            && sample.cmdline_args.iter().any(|a| a == "serve")
        {
            let port = sample.cmdline_args.iter().find_map(|a| {
                a.strip_prefix("--port=")
                    .map(String::from)
                    .or_else(|| a.strip_prefix("-p=").map(String::from))
            });
            return Some(match port {
                Some(p) => format!("ng serve (:{p})"),
                None => "ng serve".to_string(),
            });
        }
    }

    None
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
    if is_guake(sample) {
        return "Guake terminal".to_string();
    }
    // Claude Code spawns a fresh `bash -c source <snapshot>.sh && <cmd>` for
    // every tool call. They surface in the leaderboard as identical-looking
    // bash lines; collapse to a single readable label with the cwd.
    if sample.comm == "bash"
        && sample
            .cmdline_args
            .iter()
            .any(|a| a.contains("/.claude/shell-snapshots/"))
    {
        return match cwd_basename(sample) {
            Some(folder) if folder != "/" => format!("Claude shell ({folder})"),
            _ => "Claude shell".to_string(),
        };
    }
    // mysqld / mariadbd: the user runs several instances; the cwd usually
    // points at the data dir, which is the cleanest discriminator.
    if matches!(sample.comm.as_str(), "mysqld" | "mariadbd") {
        return match cwd_basename(sample) {
            Some(folder) if folder != "/" => format!("{} ({folder})", sample.comm),
            _ => sample.comm.clone(),
        };
    }
    if let Some(label) = pretty_known(sample) {
        return label;
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

/// Reads `/sys/class/power_supply/BAT*/power_now` (instantaneous draw in µW)
/// and the AC adapter online flag, so we can compare it to the RAPL package
/// total. RAPL only accounts for the CPU package; the battery sees the full
/// system (display, RAM, NVMe, Wi-Fi, etc.), so BAT will normally exceed RAPL
/// when discharging, and the gap is "rest-of-system" draw.
struct BatterySensor {
    power_now_path: Option<String>,
    status_path: Option<String>,
    energy_now_path: Option<String>,
    energy_full_path: Option<String>,
    ac_online_path: Option<String>,
}

impl BatterySensor {
    fn detect() -> Self {
        let mut power_now_path = None;
        let mut status_path = None;
        let mut energy_now_path = None;
        let mut energy_full_path = None;
        if let Ok(entries) = fs::read_dir("/sys/class/power_supply") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if name.starts_with("BAT") {
                    let pn = format!("/sys/class/power_supply/{name}/power_now");
                    if fs::metadata(&pn).is_ok() && power_now_path.is_none() {
                        power_now_path = Some(pn);
                        status_path = Some(format!("/sys/class/power_supply/{name}/status"));
                        let en = format!("/sys/class/power_supply/{name}/energy_now");
                        if fs::metadata(&en).is_ok() {
                            energy_now_path = Some(en);
                        }
                        let ef = format!("/sys/class/power_supply/{name}/energy_full");
                        if fs::metadata(&ef).is_ok() {
                            energy_full_path = Some(ef);
                        }
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
            energy_now_path,
            energy_full_path,
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

    /// Hours until battery hits empty at the current draw, computed from
    /// `energy_now` / `power_now`. Returns None on AC, when readings are
    /// missing, or when the kernel reports zero draw (briefly possible right
    /// at unplug, before sysfs settles).
    fn time_to_empty_hours(&self) -> Option<f64> {
        if !self.discharging() {
            return None;
        }
        let power_uw: u64 = fs::read_to_string(self.power_now_path.as_ref()?)
            .ok()?
            .trim()
            .parse()
            .ok()?;
        if power_uw == 0 {
            return None;
        }
        let energy_uwh: u64 = fs::read_to_string(self.energy_now_path.as_ref()?)
            .ok()?
            .trim()
            .parse()
            .ok()?;
        Some(energy_uwh as f64 / power_uw as f64)
    }

    /// State of charge as a percentage (energy_now / energy_full), 0–100.
    fn percent(&self) -> Option<f64> {
        let en: u64 = fs::read_to_string(self.energy_now_path.as_ref()?)
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let ef: u64 = fs::read_to_string(self.energy_full_path.as_ref()?)
            .ok()?
            .trim()
            .parse()
            .ok()?;
        if ef == 0 {
            return None;
        }
        Some(en as f64 / ef as f64 * 100.0)
    }
}

/// Reads the panel backlight fraction from /sys/class/backlight/*/brightness.
/// Display draw isn't reported in watts anywhere; we estimate it from the
/// backlight level using the typical envelope of a 14"/15" laptop panel:
/// ~0.5 W at 0% backlight (LCD electronics + always-on backlight floor) and
/// ~3.5 W at 100% (full LED current). Linear interpolation in between.
struct BacklightSensor {
    brightness_path: Option<String>,
    bl_power_path: Option<String>,
    /// `/sys/class/drm/<card-eDP-N>/dpms` for every laptop panel that is
    /// currently `enabled` and `connected`. This is the modern source of
    /// truth — XFCE's blank-screen / xset dpms / DPMS-on-suspend all flip
    /// these without necessarily touching legacy `bl_power`.
    dpms_paths: Vec<String>,
    max_brightness: u64,
}

impl BacklightSensor {
    /// Watts at the lowest backlight level (panel electronics, minimum LED).
    const BASE_W: f64 = 0.5;
    /// Watts at full backlight (LED bar at maximum current).
    const MAX_W: f64 = 3.5;

    fn detect() -> Self {
        let dpms_paths = Self::discover_dpms_paths();
        if let Ok(entries) = fs::read_dir("/sys/class/backlight") {
            for entry in entries.flatten() {
                let p = entry.path().join("brightness");
                let mp = entry.path().join("max_brightness");
                let bp = entry.path().join("bl_power");
                if fs::read_to_string(&p).is_ok() {
                    let max: u64 = fs::read_to_string(&mp)
                        .ok()
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(1);
                    return Self {
                        brightness_path: Some(p.to_string_lossy().to_string()),
                        bl_power_path: if fs::metadata(&bp).is_ok() {
                            Some(bp.to_string_lossy().to_string())
                        } else {
                            None
                        },
                        dpms_paths,
                        max_brightness: max.max(1),
                    };
                }
            }
        }
        Self {
            brightness_path: None,
            bl_power_path: None,
            dpms_paths,
            max_brightness: 1,
        }
    }

    /// Scan /sys/class/drm for laptop panel connectors (eDP / LVDS) that
    /// are enabled + connected, and return their `dpms` file paths.
    fn discover_dpms_paths() -> Vec<String> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir("/sys/class/drm") else {
            return out;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            // Laptop panels: card<N>-eDP-<M> or card<N>-LVDS-<M>.
            if !(name.contains("-eDP-") || name.contains("-LVDS-")) {
                continue;
            }
            let base = entry.path();
            let enabled = fs::read_to_string(base.join("enabled"))
                .map(|s| s.trim() == "enabled")
                .unwrap_or(false);
            let connected = fs::read_to_string(base.join("status"))
                .map(|s| s.trim() == "connected")
                .unwrap_or(false);
            if !(enabled && connected) {
                continue;
            }
            let dpms = base.join("dpms");
            if fs::metadata(&dpms).is_ok() {
                out.push(dpms.to_string_lossy().to_string());
            }
        }
        out
    }

    /// True when the panel is in any kind of off / blanked state. We check
    /// two sources because they don't always agree:
    ///   1. `bl_power` (legacy fbdev): non-zero ⇒ off.
    ///   2. DRM connector `dpms`: anything other than "On" ⇒ off.
    /// Either saying off is treated as off. Missing files mean "no opinion".
    fn is_powered_off(&self) -> bool {
        if let Some(p) = &self.bl_power_path {
            if let Ok(s) = fs::read_to_string(p) {
                if s.trim().parse::<u32>().map(|v| v != 0).unwrap_or(false) {
                    return true;
                }
            }
        }
        for p in &self.dpms_paths {
            if let Ok(s) = fs::read_to_string(p) {
                if !s.trim().eq_ignore_ascii_case("on") {
                    return true;
                }
            }
        }
        false
    }

    /// Backlight level as a 0.0–1.0 fraction.
    fn fraction(&self) -> Option<f64> {
        let cur: u64 = fs::read_to_string(self.brightness_path.as_ref()?)
            .ok()?
            .trim()
            .parse()
            .ok()?;
        Some((cur as f64) / (self.max_brightness as f64))
    }

    /// Returns None when the panel is reported off (don't render the line at
    /// all then) or when no backlight device is exposed.
    fn estimated_watts(&self) -> Option<f64> {
        if self.is_powered_off() {
            return None;
        }
        let f = self.fraction()?.clamp(0.0, 1.0);
        Some(Self::BASE_W + (Self::MAX_W - Self::BASE_W) * f)
    }
}

/// Tracks network interface counters and classifies each interface as
/// wireless or wired. Sums up rx+tx bytes across all non-loopback ifaces.
struct NetSensor {
    ifaces: Vec<NetIface>,
}

#[allow(dead_code)]
struct NetIface {
    name: String, // kept for future per-iface display, currently unused
    rx_path: String,
    tx_path: String,
    carrier_path: String,
    is_wireless: bool,
}

#[derive(Default)]
struct NetReadout {
    rx_bytes_total: u64,
    tx_bytes_total: u64,
    rx_bytes_wireless: u64,
    tx_bytes_wireless: u64,
    wireless_associated: bool,
}

impl NetSensor {
    fn detect() -> Self {
        let mut ifaces = Vec::new();
        let Ok(entries) = fs::read_dir("/sys/class/net") else {
            return Self { ifaces };
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == "lo" {
                continue;
            }
            let base = format!("/sys/class/net/{name}");
            // A wireless interface exposes either a `wireless/` directory or
            // a `phy80211` symlink.
            let is_wireless = fs::metadata(format!("{base}/wireless")).is_ok()
                || fs::metadata(format!("{base}/phy80211")).is_ok();
            ifaces.push(NetIface {
                name: name.to_string(),
                rx_path: format!("{base}/statistics/rx_bytes"),
                tx_path: format!("{base}/statistics/tx_bytes"),
                carrier_path: format!("{base}/carrier"),
                is_wireless,
            });
        }
        Self { ifaces }
    }

    fn read(&self) -> NetReadout {
        let mut out = NetReadout::default();
        for i in &self.ifaces {
            let rx = fs::read_to_string(&i.rx_path)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            let tx = fs::read_to_string(&i.tx_path)
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            out.rx_bytes_total = out.rx_bytes_total.saturating_add(rx);
            out.tx_bytes_total = out.tx_bytes_total.saturating_add(tx);
            if i.is_wireless {
                out.rx_bytes_wireless = out.rx_bytes_wireless.saturating_add(rx);
                out.tx_bytes_wireless = out.tx_bytes_wireless.saturating_add(tx);
                // `carrier` is 1 when the link is up (associated to an AP).
                // operstate stays "up" even when rfkilled, so it can't tell us
                // whether the radio is actually on.
                let linked = fs::read_to_string(&i.carrier_path)
                    .ok()
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .map(|v| v == 1)
                    .unwrap_or(false);
                out.wireless_associated = out.wireless_associated || linked;
            }
        }
        out
    }

    fn has_wireless(&self) -> bool {
        self.ifaces.iter().any(|i| i.is_wireless)
    }
}

/// Estimate Wi-Fi radio power from throughput. There's no kernel-exposed
/// per-radio wattage on consumer hardware; this is a coarse model.
///
///   not associated → 0 W (radio idle/off)
///   associated, idle traffic → 0.7 W (RX listen + occasional beacons)
///   ramps linearly to ~2.5 W at 5 MB/s aggregated rx+tx
fn estimate_wifi_radio_watts(associated: bool, throughput_bytes_per_sec: f64) -> f64 {
    if !associated {
        return 0.0;
    }
    const IDLE_W: f64 = 0.7;
    const HEAVY_W: f64 = 2.5;
    const HEAVY_THRESHOLD_BPS: f64 = 5.0 * 1024.0 * 1024.0; // 5 MB/s
    let f = (throughput_bytes_per_sec / HEAVY_THRESHOLD_BPS).clamp(0.0, 1.0);
    IDLE_W + (HEAVY_W - IDLE_W) * f
}

/// Format a byte rate in human-friendly units.
fn fmt_byte_rate(bps: f64) -> String {
    if !bps.is_finite() || bps < 1.0 {
        return "0 B/s".to_string();
    }
    if bps < 1024.0 {
        format!("{:.0} B/s", bps)
    } else if bps < 1024.0 * 1024.0 {
        format!("{:.1} KB/s", bps / 1024.0)
    } else if bps < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bps / 1024.0 / 1024.0)
    } else {
        format!("{:.2} GB/s", bps / 1024.0 / 1024.0 / 1024.0)
    }
}

/// Format a duration in hours as "Xh Ym".
fn fmt_hours(h: f64) -> String {
    if !h.is_finite() || h < 0.0 {
        return "—".to_string();
    }
    let total_min = (h * 60.0).round() as u64;
    format!("{}h {:02}m", total_min / 60, total_min % 60)
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
  -V, --version         Print version and exit
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
  default; when wattaouille can't read them it prints setup instructions and
  pauses for confirmation, then runs without the W/J columns.

  Pass `--no-power` to simulate the disabled path even when RAPL is
  readable — useful for previewing what a new user sees.

Battery cross-check:
  When discharging, wattaouille also reads `/sys/class/power_supply/BAT*/
  power_now` and shows BAT watts alongside RAPL watts. The drift figure
  (Δ%) is the share of battery energy NOT accounted for by RAPL — that's
  display, RAM, NVMe, Wi-Fi, etc. Plug back in and the BAT readout
  switches to `🔌 on AC`.

Press Ctrl+C to quit."
    );
}

fn main() -> io::Result<()> {
    install_signal_handlers();
    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("wattaouille v{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        let prog = args
            .first()
            .map(|s| s.rsplit('/').next().unwrap_or(s.as_str()))
            .unwrap_or("wattaouille");
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
    let backlight = BacklightSensor::detect();
    let net = NetSensor::detect();
    let mut prev_energy_uj = power.read_uj();
    // Per-subdomain previous readings (core, uncore, dram).
    let mut prev_subdomain_uj: Vec<Option<u64>> =
        power.subdomains.iter().map(|d| d.read_uj()).collect();
    // Per-subdomain cumulative joules for the session.
    let mut subdomain_joules: Vec<f64> = vec![0.0; power.subdomains.len()];
    let mut cumulative_joules: HashMap<u32, f64> = HashMap::new();
    let mut total_joules: f64 = 0.0;
    // Battery vs RAPL drift accounting — only accumulates while discharging.
    let mut bat_joules: f64 = 0.0;
    let mut rapl_joules_while_discharging: f64 = 0.0;
    let mut discharging_secs: f64 = 0.0;
    let mut elapsed_secs: f64 = 0.0;
    // Per-PID I/O delta tracking and session totals.
    let mut prev_io: HashMap<u32, u64> = HashMap::new();
    let mut cumulative_io: HashMap<u32, u64> = HashMap::new();
    let initial_net = net.read();
    let mut net_prev_total: u64 = initial_net.rx_bytes_total + initial_net.tx_bytes_total;
    let mut net_prev_wireless: u64 =
        initial_net.rx_bytes_wireless + initial_net.tx_bytes_wireless;
    let mut display_joules: f64 = 0.0;
    let mut wifi_joules: f64 = 0.0;
    for (pid, sample) in &prev_snap {
        if let Some(b) = sample.io_bytes {
            prev_io.insert(*pid, b);
        }
    }

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
    // Disable canonical mode + echo on stdin AFTER the alt screen is up,
    // so scrolling with the mouse wheel doesn't leak arrow-key escapes
    // into the displayed buffer.
    enter_raw_mode();

    loop {
        thread::sleep(Duration::from_millis(interval_ms));

        let cur_total = total_cpu_jiffies();
        let cur_snap = snapshot();
        let cur_energy_uj = power.read_uj();
        let cur_subdomain_uj: Vec<Option<u64>> =
            power.subdomains.iter().map(|d| d.read_uj()).collect();
        let total_delta = cur_total.saturating_sub(prev_total).max(1);

        // Joules spent across the whole CPU package during the sample.
        let frame_joules = match (prev_energy_uj, cur_energy_uj) {
            (Some(b), Some(a)) => power.joules_between(b, a),
            _ => 0.0,
        };
        // Per-subdomain joules + cumulative.
        let mut frame_subdomain_joules: Vec<f64> = vec![0.0; power.subdomains.len()];
        for (i, dom) in power.subdomains.iter().enumerate() {
            if let (Some(b), Some(a)) = (prev_subdomain_uj[i], cur_subdomain_uj[i]) {
                let j = dom.joules_between(b, a);
                frame_subdomain_joules[i] = j;
                subdomain_joules[i] += j;
            }
        }
        let interval_secs = interval_ms as f64 / 1000.0;
        let frame_watts = frame_joules / interval_secs;
        total_joules += frame_joules;
        elapsed_secs += interval_secs;

        // Network throughput across all non-loopback ifaces.
        let net_now = net.read();
        let net_total_now = net_now.rx_bytes_total + net_now.tx_bytes_total;
        let net_wireless_now = net_now.rx_bytes_wireless + net_now.tx_bytes_wireless;
        let net_total_bps =
            net_total_now.saturating_sub(net_prev_total) as f64 / interval_secs;
        let net_wireless_bps =
            net_wireless_now.saturating_sub(net_prev_wireless) as f64 / interval_secs;
        net_prev_total = net_total_now;
        net_prev_wireless = net_wireless_now;

        // Display + Wi-Fi radio energy estimates accumulated for the session.
        let display_w = backlight.estimated_watts();
        if let Some(w) = display_w {
            display_joules += w * interval_secs;
        }
        // Only show a Wi-Fi figure when the radio is actually doing
        // something: there's a wireless interface AND it has a carrier
        // (rfkilled / down / unplugged → None, line disappears).
        let wifi_w = if net.has_wireless() && net_now.wireless_associated {
            Some(estimate_wifi_radio_watts(true, net_wireless_bps))
        } else {
            None
        };
        if let Some(w) = wifi_w {
            wifi_joules += w * interval_secs;
        }

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

        // Per-PID I/O byte deltas. /proc/[pid]/io is permission-restricted, so
        // many entries will be None — we skip those silently.
        let mut io_deltas: HashMap<u32, u64> = HashMap::new();
        let mut next_prev_io: HashMap<u32, u64> = HashMap::new();
        for (pid, sample) in &cur_snap {
            if let Some(now) = sample.io_bytes {
                next_prev_io.insert(*pid, now);
                if let Some(before) = prev_io.get(pid) {
                    let d = now.saturating_sub(*before);
                    if d > 0 {
                        io_deltas.insert(*pid, d);
                        *cumulative_io.entry(*pid).or_insert(0) += d;
                    }
                }
            }
        }
        prev_io = next_prev_io;
        cumulative_io.retain(|pid, _| cur_snap.contains_key(pid));

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
            // Subdomain "now" watts breakdown. Width-stable: 5 chars for the
            // numeric value (XXX.X / " XX.X" / "  X.X") so the line doesn't
            // jump as values cross 10 W or 100 W between frames.
            let mut sub: Vec<String> = Vec::new();
            for (i, dom) in power.subdomains.iter().enumerate() {
                let w = frame_subdomain_joules[i] / interval_secs;
                sub.push(format!("{} {:>5.1} W", dom.label, w));
            }
            let sub_str = if sub.is_empty() {
                String::new()
            } else {
                format!(" ({})", sub.join(" · "))
            };
            bits.push(format!(
                "⚡ RAPL {:>5.1} W avg{} · {:.3} Wh ({:.0} J)",
                avg_w, sub_str, total_wh, total_joules
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
                let mut s = format!(
                    "🔋 BAT {:>5.1} W avg · {:.3} Wh ({:.0} J)",
                    bat_avg_w, bat_wh, bat_joules
                );
                if let Some(pct) = battery.percent() {
                    s.push_str(&format!(" · {:.0}%", pct));
                }
                if let Some(h) = battery.time_to_empty_hours() {
                    s.push_str(&format!(" · {} left", fmt_hours(h)));
                }
                bits.push(s);
            }
        } else if battery.power_now_path.is_some() {
            let mut s = "🔌 on AC".to_string();
            if let Some(pct) = battery.percent() {
                s.push_str(&format!(" · {:.0}%", pct));
            }
            bits.push(s);
        }
        // Drift accounting — only meaningful once both sensors have logged a
        // non-trivial amount of energy while discharging. Show absolute W avg
        // alongside the % so the user can read the consumption gap as a rate.
        if power.enabled && rapl_joules_while_discharging > 1.0 && bat_joules > 1.0 {
            let non_cpu_j = bat_joules - rapl_joules_while_discharging;
            let drift_pct = non_cpu_j / bat_joules * 100.0;
            let non_cpu_w = if discharging_secs > 0.0 {
                non_cpu_j / discharging_secs
            } else {
                0.0
            };
            let non_cpu_wh = non_cpu_j / 3600.0;
            bits.push(format!(
                "Δ non-CPU {:>+6.1} W avg · {:.3} Wh ({:.0} J · {:+.0}%)",
                non_cpu_w, non_cpu_wh, non_cpu_j, drift_pct
            ));
        }
        // Multi-line header. Line 1: runtime stats + Ctrl+C hint. Line 2:
        // CPU package energy. Line 3 (when on battery): battery + drift.
        // Line 4: non-CPU breakdown (display, Wi-Fi radio, network, drift).
        writeln!(
            out,
            "wattaouille v{} — {} ms · {} CPU(s) · {} procs · ~{:.0}s tracked · Ctrl+C to quit",
            env!("CARGO_PKG_VERSION"),
            interval_ms,
            cpus,
            cur_snap.len(),
            session_secs
        )?;
        if !bits.is_empty() {
            writeln!(out, "{}", bits.join(" · "))?;
        }

        // Non-CPU line: display-watt estimate, Wi-Fi radio estimate, total
        // network throughput. All present-only-when-available so the line
        // doesn't render at all on a headless or wired-only box.
        let mut non_cpu_bits: Vec<String> = Vec::new();
        if let Some(w) = display_w {
            let pct = backlight.fraction().unwrap_or(0.0) * 100.0;
            let total_wh = display_joules / 3600.0;
            // `~` prefix flags this as a model-based estimate, not a sensor read.
            non_cpu_bits.push(format!(
                "🖥 ~{:>5.1} W ({:.0}% bl · ~{:.3} Wh)",
                w, pct, total_wh
            ));
        }
        if let Some(w) = wifi_w {
            let total_wh = wifi_joules / 3600.0;
            non_cpu_bits.push(format!(
                "📡 Wi-Fi ~{:>5.1} W ({} · ~{:.3} Wh)",
                w,
                fmt_byte_rate(net_wireless_bps),
                total_wh
            ));
        }
        if net_total_bps > 1.0 {
            non_cpu_bits.push(format!("📶 net {}", fmt_byte_rate(net_total_bps)));
        }
        if !non_cpu_bits.is_empty() {
            writeln!(out, "{}", non_cpu_bits.join(" · "))?;
        }
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
                "{:>7}  {:>7}  {:>7}  {:>7}  {:>16}  {:>10}  {}",
                "PID", "AVG%", "NOW%", "NOW W", "TOTAL W (J)", "I/O", "COMMAND"
            )?;
        } else {
            writeln!(
                out,
                "{:>7}  {:>7}  {:>7}  {:>10}  {}",
                "PID", "AVG%", "NOW%", "I/O", "COMMAND"
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
            // I/O delta this frame, summed across the subtree for collapse roots.
            let io_now_bps = {
                let raw = if is_collapse_root(sample) {
                    let mut s = io_deltas.get(pid).copied().unwrap_or(0);
                    let mut stack = vec![*pid];
                    while let Some(p) = stack.pop() {
                        if let Some(kids) = children.get(&p) {
                            for &c in kids {
                                s = s.saturating_add(io_deltas.get(&c).copied().unwrap_or(0));
                                stack.push(c);
                            }
                        }
                    }
                    s
                } else {
                    io_deltas.get(pid).copied().unwrap_or(0)
                };
                raw as f64 / interval_secs.max(0.001)
            };
            let io_cell = if io_now_bps >= 1.0 {
                fmt_byte_rate(io_now_bps)
            } else {
                "—".to_string()
            };

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
                    "{:>7}  {:>6.1}%  {:>6.1}%  {:>6.2}W  {:>16}  {:>10}  ",
                    pid, avg_pct_core, now_pct_core, now_w, total_cell, io_cell
                )
            } else {
                format!(
                    "{:>7}  {:>6.1}%  {:>6.1}%  {:>10}  ",
                    pid, avg_pct_core, now_pct_core, io_cell
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
        prev_subdomain_uj = cur_subdomain_uj;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(comm: &str, args: &[&str]) -> Sample {
        Sample {
            comm: comm.to_string(),
            cmdline_args: args.iter().map(|s| s.to_string()).collect(),
            ppid: 0,
            cpu_jiffies: 0,
            cwd: None,
            io_bytes: None,
        }
    }

    fn mk_cwd(comm: &str, args: &[&str], cwd: &str) -> Sample {
        let mut s = mk(comm, args);
        s.cwd = Some(cwd.to_string());
        s
    }

    fn snap(samples: Vec<(u32, Sample)>) -> HashMap<u32, Sample> {
        samples.into_iter().collect()
    }

    // ── cwd_basename ─────────────────────────────────────────────────
    #[test]
    fn cwd_basename_strips_trailing_slash() {
        let s = mk_cwd("x", &["x"], "/mnt/Dev/@wdes/wattaouille/");
        assert_eq!(cwd_basename(&s), Some("wattaouille"));
    }

    #[test]
    fn cwd_basename_handles_root() {
        let s = mk_cwd("x", &["x"], "/");
        assert_eq!(cwd_basename(&s), None);
    }

    #[test]
    fn cwd_basename_none_when_missing() {
        assert_eq!(cwd_basename(&mk("x", &["x"])), None);
    }

    // ── is_claude_code / Claude Code labelling ───────────────────────
    #[test]
    fn detects_claude_exe_path() {
        let s = mk(
            "claude.exe",
            &[
                "/home/u/.nvm/versions/node/v22/lib/node_modules/@anthropic-ai/claude-code/bin/claude.exe",
                "--session-id",
                "abc",
            ],
        );
        assert!(is_claude_code(&s));
    }

    #[test]
    fn detects_claude_via_module_path() {
        let s = mk("node", &["/usr/bin/node", "/path/@anthropic-ai/claude-code/cli.js"]);
        assert!(is_claude_code(&s));
    }

    #[test]
    fn pretty_label_for_claude_with_happy_and_cwd() {
        // Build a snapshot where a Claude process has a Happy ancestor.
        let happy_parent = mk(
            "node",
            &["/usr/bin/node", "/x/.config/yarn/global/node_modules/happy/dist/index.mjs", "claude"],
        );
        let claude = mk_cwd(
            "claude.exe",
            &["/usr/local/bin/claude.exe", "--session-id", "abc"],
            "/mnt/Dev/@wdes/wattaouille",
        );
        let mut s = snap(vec![(1, happy_parent), (2, claude)]);
        s.get_mut(&2).unwrap().ppid = 1;
        let label = pretty_cmdline(2, s.get(&2).unwrap(), &s);
        assert_eq!(label, "Claude Code (Happy: wattaouille)");
    }

    #[test]
    fn pretty_label_for_claude_without_happy_falls_back_to_plain() {
        let claude = mk(
            "claude.exe",
            &["/usr/local/bin/claude.exe", "--session-id", "abc"],
        );
        let s = snap(vec![(1, claude)]);
        let label = pretty_cmdline(1, s.get(&1).unwrap(), &s);
        assert_eq!(label, "Claude Code");
    }

    // ── Happy wrapper detection ──────────────────────────────────────
    #[test]
    fn happy_subcommand_finds_claude() {
        let s = mk(
            "node",
            &["/usr/bin/node", "/x/.config/yarn/global/node_modules/happy/dist/index.mjs", "claude"],
        );
        assert_eq!(happy_subcommand(&s), Some("claude"));
    }

    #[test]
    fn happy_subcommand_finds_daemon() {
        let s = mk(
            "node",
            &["/usr/bin/node", "/x/happy/dist/index.mjs", "daemon", "start-sync"],
        );
        assert_eq!(happy_subcommand(&s), Some("daemon"));
    }

    #[test]
    fn happy_subcommand_none_for_unrelated() {
        let s = mk("node", &["/usr/bin/node", "/some/other/script.js"]);
        assert_eq!(happy_subcommand(&s), None);
    }

    #[test]
    fn pretty_label_for_happy_daemon_ignores_cwd() {
        let s = mk_cwd(
            "node",
            &["/usr/bin/node", "/x/happy/dist/index.mjs", "daemon", "start-sync"],
            "/some/folder",
        );
        let snap = snap(vec![(1, s)]);
        let label = pretty_cmdline(1, snap.get(&1).unwrap(), &snap);
        assert_eq!(label, "Happy daemon");
    }

    #[test]
    fn pretty_label_for_happy_claude_uses_cwd() {
        let s = mk_cwd(
            "node",
            &["/usr/bin/node", "/x/happy/dist/index.mjs", "claude"],
            "/mnt/Dev/@wdes/wattaouille",
        );
        let snap = snap(vec![(1, s)]);
        let label = pretty_cmdline(1, snap.get(&1).unwrap(), &snap);
        assert_eq!(label, "Happy (wattaouille)");
    }

    // ── Browser collapse detection ───────────────────────────────────
    #[test]
    fn collapse_main_browser() {
        let s = mk("librewolf", &["/usr/bin/librewolf", "--sm-client-id", "x"]);
        assert!(is_collapse_root(&s));
    }

    #[test]
    fn dont_collapse_browser_helper_chromium_style() {
        let s = mk("opera", &["/usr/bin/opera", "--type=renderer", "--lang=fr"]);
        assert!(!is_collapse_root(&s));
    }

    #[test]
    fn dont_collapse_browser_helper_firefox_style() {
        let s = mk(
            "librewolf",
            &["/usr/bin/librewolf", "-contentproc", "-childID", "1"],
        );
        assert!(!is_collapse_root(&s));
    }

    #[test]
    fn dont_collapse_random_process() {
        let s = mk("rustc", &["/usr/bin/rustc", "--crate-name", "x"]);
        assert!(!is_collapse_root(&s));
    }

    // ── Specialty labels ─────────────────────────────────────────────
    #[test]
    fn smartgit_label() {
        let s = mk(
            "java",
            &[
                "/usr/share/smartgit/jre/bin/java",
                "-jar",
                "/usr/share/smartgit/lib/bootloader.jar",
            ],
        );
        let snap = snap(vec![(1, s)]);
        assert_eq!(pretty_cmdline(1, snap.get(&1).unwrap(), &snap), "SmartGit");
    }

    #[test]
    fn guake_label() {
        let s = mk("python3", &["/usr/bin/python3", "/usr/bin/guake"]);
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "Guake terminal"
        );
    }

    #[test]
    fn mysqld_label_with_cwd() {
        let s = mk_cwd("mysqld", &["mysqld", "--sql_mode="], "/var/lib/mysql/projA");
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "mysqld (projA)"
        );
    }

    #[test]
    fn mysqld_label_without_cwd_stays_bare() {
        let s = mk("mysqld", &["mysqld"]);
        let snap = snap(vec![(1, s)]);
        assert_eq!(pretty_cmdline(1, snap.get(&1).unwrap(), &snap), "mysqld");
    }

    // ── Backlight estimate ───────────────────────────────────────────
    #[test]
    fn backlight_watts_no_device_returns_none() {
        let bl = BacklightSensor {
            brightness_path: None,
            bl_power_path: None,
            dpms_paths: vec![],
            max_brightness: 100,
        };
        assert_eq!(bl.estimated_watts(), None);
    }

    #[test]
    fn backlight_powered_off_via_bl_power() {
        let dir = std::env::temp_dir();
        let bp = dir.join("wattaouille_test_bl_power_4");
        std::fs::write(&bp, "4\n").unwrap();
        let br = dir.join("wattaouille_test_brightness_4");
        std::fs::write(&br, "1000\n").unwrap();
        let bl = BacklightSensor {
            brightness_path: Some(br.to_string_lossy().to_string()),
            bl_power_path: Some(bp.to_string_lossy().to_string()),
            dpms_paths: vec![],
            max_brightness: 1000,
        };
        assert_eq!(bl.estimated_watts(), None);
    }

    #[test]
    fn backlight_powered_off_via_drm_dpms() {
        // bl_power says On but DRM dpms says Off — the modern
        // (XFCE blank-screen / xset dpms) path. Should still return None.
        let dir = std::env::temp_dir();
        let bp = dir.join("wattaouille_test_bl_power_on");
        std::fs::write(&bp, "0\n").unwrap();
        let br = dir.join("wattaouille_test_brightness_on");
        std::fs::write(&br, "1000\n").unwrap();
        let dpms = dir.join("wattaouille_test_dpms_off");
        std::fs::write(&dpms, "Off\n").unwrap();
        let bl = BacklightSensor {
            brightness_path: Some(br.to_string_lossy().to_string()),
            bl_power_path: Some(bp.to_string_lossy().to_string()),
            dpms_paths: vec![dpms.to_string_lossy().to_string()],
            max_brightness: 1000,
        };
        assert_eq!(bl.estimated_watts(), None);
    }

    #[test]
    fn backlight_on_when_both_say_on() {
        let dir = std::env::temp_dir();
        let bp = dir.join("wattaouille_test_bl_power_both_on");
        std::fs::write(&bp, "0\n").unwrap();
        let br = dir.join("wattaouille_test_brightness_both_on");
        std::fs::write(&br, "500\n").unwrap();
        let dpms = dir.join("wattaouille_test_dpms_on");
        std::fs::write(&dpms, "On\n").unwrap();
        let bl = BacklightSensor {
            brightness_path: Some(br.to_string_lossy().to_string()),
            bl_power_path: Some(bp.to_string_lossy().to_string()),
            dpms_paths: vec![dpms.to_string_lossy().to_string()],
            max_brightness: 1000,
        };
        // 500/1000 = 50% → 0.5 + 3.0*0.5 = 2.0 W
        let w = bl.estimated_watts().unwrap();
        assert!((w - 2.0).abs() < 1e-9);
    }

    // ── Wi-Fi radio estimate ─────────────────────────────────────────
    #[test]
    fn wifi_radio_zero_when_not_associated() {
        assert_eq!(estimate_wifi_radio_watts(false, 1024.0 * 1024.0), 0.0);
    }

    #[test]
    fn wifi_radio_idle_when_associated() {
        let w = estimate_wifi_radio_watts(true, 0.0);
        assert!((w - 0.7).abs() < 1e-6);
    }

    #[test]
    fn wifi_radio_clamps_at_heavy_traffic() {
        // 50 MB/s is way above the 5 MB/s threshold; should clamp to MAX.
        let w = estimate_wifi_radio_watts(true, 50.0 * 1024.0 * 1024.0);
        assert!((w - 2.5).abs() < 1e-6);
    }

    // ── Byte-rate formatter ──────────────────────────────────────────
    #[test]
    fn fmt_byte_rate_units() {
        assert_eq!(fmt_byte_rate(0.0), "0 B/s");
        assert_eq!(fmt_byte_rate(512.0), "512 B/s");
        assert_eq!(fmt_byte_rate(2.0 * 1024.0), "2.0 KB/s");
        assert_eq!(fmt_byte_rate(3.5 * 1024.0 * 1024.0), "3.5 MB/s");
    }

    // ── Slack and friends ────────────────────────────────────────────
    #[test]
    fn slack_label() {
        let s = mk("slack", &["/usr/bin/slack"]);
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "Slack desktop"
        );
    }

    #[test]
    fn element_desktop_label() {
        let s = mk("element-desktop", &["/usr/bin/element-desktop"]);
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "Element (Matrix)"
        );
    }

    #[test]
    fn claude_shell_with_cwd() {
        let s = mk_cwd(
            "bash",
            &[
                "bash",
                "-c",
                "source /home/u/.claude/shell-snapshots/snapshot-bash-1234.sh && cargo build",
            ],
            "/mnt/Dev/@wdes/wattaouille",
        );
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "Claude shell (wattaouille)"
        );
    }

    #[test]
    fn claude_shell_without_cwd() {
        let s = mk(
            "bash",
            &[
                "bash",
                "-c",
                "source /home/u/.claude/shell-snapshots/snapshot-bash-x.sh && ls",
            ],
        );
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "Claude shell"
        );
    }

    // ── Battery time formatter ───────────────────────────────────────
    #[test]
    fn fmt_hours_basic() {
        assert_eq!(fmt_hours(2.25), "2h 15m");
        assert_eq!(fmt_hours(0.5), "0h 30m");
        assert_eq!(fmt_hours(0.0), "0h 00m");
    }

    #[test]
    fn fmt_hours_invalid() {
        assert_eq!(fmt_hours(f64::NAN), "—");
        assert_eq!(fmt_hours(-1.0), "—");
    }

    // ── flatten_visible: 0%-CPU middlemen are hidden ─────────────────
    #[test]
    fn flatten_promotes_busy_grandchild() {
        // init (0%) → wrapper (0%) → busy_leaf (1%)
        let init = mk("init", &["/sbin/init"]);
        let wrapper = mk("bash", &["/bin/bash"]);
        let busy = mk("rustc", &["rustc"]);
        let mut snap = snap(vec![(1, init), (2, wrapper), (3, busy)]);
        snap.get_mut(&2).unwrap().ppid = 1;
        snap.get_mut(&3).unwrap().ppid = 2;
        let mut deltas = HashMap::new();
        deltas.insert(1, 0u64);
        deltas.insert(2, 0u64);
        deltas.insert(3, 100u64);
        let mut children = HashMap::new();
        children.insert(0, vec![1]);
        children.insert(1, vec![2]);
        children.insert(2, vec![3]);
        let subtree: HashMap<u32, u64> = snap
            .keys()
            .map(|p| (*p, subtree_delta(*p, &deltas, &children)))
            .collect();
        let visible = flatten_visible(&[1], &snap, &deltas, &children, &subtree);
        assert_eq!(visible, vec![3]);
    }

    #[test]
    fn flatten_keeps_collapse_root_even_at_zero_own_cpu() {
        // librewolf (0% own) → contentproc (5%): the librewolf line should
        // still surface so the browser collapse summary fires.
        let lw = mk("librewolf", &["/usr/bin/librewolf"]);
        let helper = mk("librewolf", &["/usr/bin/librewolf", "--type=renderer"]);
        let mut snap = snap(vec![(1, lw), (2, helper)]);
        snap.get_mut(&2).unwrap().ppid = 1;
        let mut deltas = HashMap::new();
        deltas.insert(1, 0u64);
        deltas.insert(2, 500u64);
        let mut children = HashMap::new();
        children.insert(0, vec![1]);
        children.insert(1, vec![2]);
        let subtree: HashMap<u32, u64> = snap
            .keys()
            .map(|p| (*p, subtree_delta(*p, &deltas, &children)))
            .collect();
        let visible = flatten_visible(&[1], &snap, &deltas, &children, &subtree);
        assert_eq!(visible, vec![1]);
    }

    // ── pretty_known: desktop / daemon labels ────────────────────────
    fn pretty_solo(s: Sample) -> String {
        let snap = snap(vec![(1, s)]);
        pretty_cmdline(1, snap.get(&1).unwrap(), &snap)
    }

    #[test]
    fn xfce_desktop_labels() {
        assert_eq!(
            pretty_solo(mk("xfdesktop", &["xfdesktop", "--display", ":0.0"])),
            "Xfce desktop"
        );
        assert_eq!(
            pretty_solo(mk("xfce4-panel", &["xfce4-panel", "--display", ":0.0"])),
            "Xfce panel"
        );
        assert_eq!(
            pretty_solo(mk("xfwm4", &["xfwm4", "--display", ":0.0"])),
            "Xfwm window manager"
        );
    }

    #[test]
    fn ibus_labels() {
        let s = mk("ibus-daemon", &["ibus-daemon", "--daemonize", "--xim"]);
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "IBus daemon"
        );
    }

    #[test]
    fn pipewire_labels() {
        let s = mk("pipewire-pulse", &["/usr/bin/pipewire-pulse"]);
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "PipeWire (pulse compat)"
        );
    }

    #[test]
    fn dockerd_label() {
        let s = mk("dockerd", &["dockerd", "--config-file=/etc/docker/daemon.json"]);
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "Docker daemon"
        );
    }

    #[test]
    fn containerd_shim_with_id() {
        // comm is truncated to "containerd-shim"; argv carries the long id.
        let s = mk(
            "containerd-shim",
            &[
                "containerd-shim-runc-v2",
                "-namespace",
                "moby",
                "-id",
                "abcdef1234567890deadbeef",
                "-address",
                "/run/foo",
            ],
        );
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "containerd-shim (abcdef12)"
        );
    }

    #[test]
    fn containerd_shim_without_id() {
        let s = mk("containerd-shim", &["containerd-shim-runc-v2", "--help"]);
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "containerd-shim"
        );
    }

    #[test]
    fn xfce_panel_plugin_named_from_so() {
        let s = mk(
            "wrapper-2.0",
            &[
                "wrapper-2.0",
                "/usr/lib/x86_64-linux-gnu/xfce4/panel/plugins/libwhiskermenu.so",
                "28",
                "23068679",
                "whiskermenu",
            ],
        );
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "Xfce panel plugin (whiskermenu)"
        );
    }

    #[test]
    fn blueman_tray_label() {
        let s = mk("python3", &["/usr/bin/python3", "/usr/bin/blueman-tray"]);
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "Blueman (tray)"
        );
    }

    #[test]
    fn ng_serve_label_with_port() {
        let s = mk(
            "node",
            &[
                "/usr/bin/node",
                "/usr/bin/ng",
                "serve",
                "--port=4200",
                "--host=0.0.0.0",
            ],
        );
        let snap_ = snap(vec![(1, s)]);
        assert_eq!(
            pretty_cmdline(1, snap_.get(&1).unwrap(), &snap_),
            "ng serve (:4200)"
        );
    }

    #[test]
    fn flatten_drops_dead_subtree() {
        // Both 0%; no descendants busy either. Should disappear entirely.
        let a = mk("a", &["a"]);
        let b = mk("b", &["b"]);
        let mut snap = snap(vec![(1, a), (2, b)]);
        snap.get_mut(&2).unwrap().ppid = 1;
        let mut deltas = HashMap::new();
        deltas.insert(1, 0u64);
        deltas.insert(2, 0u64);
        let mut children = HashMap::new();
        children.insert(0, vec![1]);
        children.insert(1, vec![2]);
        let subtree: HashMap<u32, u64> = snap
            .keys()
            .map(|p| (*p, subtree_delta(*p, &deltas, &children)))
            .collect();
        let visible = flatten_visible(&[1], &snap, &deltas, &children, &subtree);
        assert!(visible.is_empty());
    }
}
