// SPDX-FileCopyrightText: 2026 oxlayer
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Setup measurement and repair, on Ubuntu.
//!
//! Spec: ../../specs/011-onboarding.md
//!
//! The catalog and the decision live in `setup.rs`; here we only measure
//! the world and touch it. Two things the split imposes, and that hold:
//!
//! 1. Every measurement is **cheap and time-limited**. The wizard calls
//!    `measure()` every few seconds; a step that takes 10 s brings the
//!    whole panel down. Where the honest measurement is expensive (NVENC,
//!    SSH), it comes from a cache or from a probe with a timeout.
//! 2. Every repair returns the message of what it **measured
//!    afterwards**, not of what it ordered. "I ran the command" is not a
//!    result ([[../../.claude/rules/verify-what-arrived]]).

use crate::config::Config;
use crate::setup::{State, Check};
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

const BINS: [&str; 6] =
    ["oxdesk", "oxdesk-host", "oxdesk-view", "oxdesk-bridge", "oxdesk-kvm", "oxdesk-ota"];

fn bin_dir() -> PathBuf {
    crate::sys::home_dir().join(".local/bin")
}

pub fn measure(cfg: &Config) -> Vec<Check> {
    vec![
        binaries(),
        crate::setup_common::this_machine_config(cfg),
        daemon(),
        receiver(),
        input_kvm(),
        wayland(),
        crate::setup_common::clipboard(),
        crate::setup_common::peers(cfg),
        nvenc(),
        tray(),
        ota(),
    ]
}

fn ota_token() -> PathBuf {
    crate::sys::home_dir().join(".config/oxdesk/ota-token")
}

fn ota() -> Check {
    crate::setup_common::ota(&bin_dir().join("oxdesk-ota"), &ota_token(), clock())
}

/// The systemd timer that wakes `oxdesk-ota` up.
///
/// `is-active` **and** `is-enabled`: a timer that is active but not
/// enabled dies on the next boot without warning -- the same pair the
/// daemon step checks, for the same reason.
fn clock() -> Result<String, String> {
    let active = Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "oxdesk-ota.timer"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !active {
        return Err("oxdesk-ota.timer is not active".into());
    }
    let enabled = Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", "oxdesk-ota.timer"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !enabled {
        return Err("oxdesk-ota.timer runs now but is not enabled: it will not come up on the next boot".into());
    }
    Ok("systemd timer, hourly".into())
}

fn binaries() -> Check {
    let dir = bin_dir();
    let absent: Vec<&str> = BINS.iter().copied().filter(|b| !dir.join(b).exists()).collect();
    if !absent.is_empty() {
        return Check::missing("binaries", format!("missing: {}", absent.join(", ")));
    }
    // Existing on disk is not being live. See
    // `setup_common::stamp_verdict`: a deploy can install the new file
    // and nobody runs it.
    if let Some(p) = crate::setup_common::daemon_on_disk(&dir.join("oxdesk")) {
        return Check::missing("binaries", p);
    }
    // Being on disk is not the same as being reachable by the process
    // that has to launch it. This is the half that was missing while the
    // panel said `ok` and both Ctrl did nothing.
    let unreachable =
        crate::setup_common::unresolvable(crate::setup_common::DAEMON_LAUNCHES, crate::sys::own_bin);
    if !unreachable.is_empty() {
        return Check::missing(
            "binaries",
            format!(
                "{} is on disk but not beside the running daemon ({}): it would be looked up in \
                 PATH, which a user service does not share with your shell",
                unreachable.join(", "),
                dir.display()
            ),
        );
    }
    Check::ok("binaries", format!("{} binaries in {}", BINS.len(), dir.display()))
}

fn daemon() -> Check {
    let state = crate::sys::unit_state("oxdesk.service");
    let enabled = Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", "oxdesk.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    match (state.as_str(), enabled) {
        ("active", true) => Check::ok("daemon", "active and enabled at login"),
        // Active but not enabled is the treacherous case: it works today
        // and disappears on the next boot. It must weigh as missing.
        ("active", false) => Check::missing("daemon", "active, but NOT enabled: it will not come back at the next login"),
        (other, _) => Check::missing("daemon", format!("unit {other}")),
    }
}

/// Does any udev rule cover `hidraw` for Logitech devices?
///
/// Scans at FILE level, not line level, and all three directories. The
/// first version of this looked only at `/etc/udev/rules.d` and required
/// `hidraw` and `046d` on the SAME line -- and said "no rule" on a
/// machine where access worked. The real rule (the one from the Solaar
/// package, in `/usr/lib`) uses `GOTO`, so the subsystem and the vendor
/// appear on different lines. Wrong measurement, right machine.
fn hidraw_rule() -> Option<String> {
    for dir in ["/etc/udev/rules.d", "/usr/lib/udev/rules.d", "/lib/udev/rules.d"] {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for e in entries.flatten() {
            let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
            let useful: String = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.starts_with('#') && !l.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            if useful.contains("hidraw")
                && useful.contains("046d")
                && (useful.contains("uaccess") || useful.contains("MODE"))
            {
                return Some(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// (Logitech `hidraw` nodes, how many open).
///
/// Filters by vendor instead of using `sys::hidraw_count`, which counts
/// ALL nodes: on this machine there are four, and one of them is a device
/// from another vendor that does not open -- so the overall count said
/// "3 of 4" and left the impression that a permission was missing
/// somewhere. The vendor comes from `HID_ID` in the uevent
/// (`0003:0000046D:...`), which exists for both USB and Bluetooth.
fn hidraw_logitech() -> (usize, usize) {
    let Ok(entries) = std::fs::read_dir("/dev") else { return (0, 0) };
    let mut total = 0;
    let mut openable = 0;
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(n) = name.to_str() else { continue };
        if !n.starts_with("hidraw") {
            continue;
        }
        let uevent = std::fs::read_to_string(format!("/sys/class/hidraw/{n}/device/uevent"))
            .unwrap_or_default()
            .to_ascii_uppercase();
        let logitech = uevent
            .lines()
            .filter_map(|l| l.strip_prefix("HID_ID="))
            .any(|v| v.split(':').nth(1).is_some_and(|ven| ven.ends_with("046D")));
        if !logitech {
            continue;
        }
        total += 1;
        if std::fs::File::open(e.path()).is_ok() {
            openable += 1;
        }
    }
    (total, openable)
}

/// Is the HID++ receiver accessible NOW, and will it stay so after a
/// replug?
///
/// Two questions in one line on purpose. The ACL alone would answer
/// "works today" and hide that it dies on the next replug; the rule alone
/// would answer "should work" without looking at whether it does.
fn receiver() -> Check {
    let (total, openable) = hidraw_logitech();
    if total == 0 {
        // Zero nodes is absent hardware, not a missing permission. The
        // panel once told the person to "run udevadm" when the receiver
        // was on the other machine -- fixing a permission that was not
        // broken.
        return Check::not_applicable("receiver", "no Logitech hidraw right now: receiver unplugged?");
    }
    let rule = hidraw_rule();
    match (openable > 0, rule) {
        (true, Some(r)) => Check::ok("receiver", format!("{openable} of {total} nodes open; rule {r}")),
        (true, None) => Check::missing(
            "receiver",
            format!("{openable} of {total} nodes open NOW, but no rule covers hidraw+046d: \
                     the ACL disappears on the next replug"),
        ),
        (false, Some(r)) => Check::missing(
            "receiver",
            format!("{r} exists and none of the {total} nodes open: a udevadm trigger is missing"),
        ),
        (false, None) => Check::missing("receiver", format!("none of the {total} /dev/hidraw nodes open")),
    }
}

/// Are the Logitech devices' `event*` nodes readable?
///
/// By vendor in `/sys`, not by name: "MX Master" becomes another name
/// when the device switches channel, and a name filter would measure
/// right today and wrong tomorrow.
fn input_kvm() -> Check {
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return Check::error("input-kvm", "could not read /dev/input");
    };
    let mut total = 0;
    let mut readable = 0;
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(n) = name.to_str() else { continue };
        if !n.starts_with("event") {
            continue;
        }
        let vendor = std::fs::read_to_string(format!("/sys/class/input/{n}/device/id/vendor"))
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if vendor != "046d" {
            continue;
        }
        total += 1;
        if std::fs::File::open(e.path()).is_ok() {
            readable += 1;
        }
    }
    if total == 0 {
        return Check::not_applicable("input-kvm", "no Logitech device in /dev/input right now");
    }
    Check::when(
        "input-kvm",
        readable == total,
        format!("{readable} of {total} Logitech devices readable"),
        format!("only {readable} of {total} Logitech devices are readable"),
    )
}

fn wayland() -> Check {
    match std::env::var("XDG_SESSION_TYPE").unwrap_or_default().as_str() {
        "wayland" => Check::ok("wayland", "Wayland"),
        "" => Check::error("wayland", "XDG_SESSION_TYPE empty: the daemon did not inherit the session environment"),
        other => Check::missing("wayland", format!("{other} session: edge detection and clipboard were never validated here")),
    }
}

fn nvenc() -> Check {
    // The real probe (`oxdesk-probe-nvenc`) initializes CUDA and
    // takes seconds. For the panel the earlier question is enough: is an
    // NVIDIA driver loaded? Without it the probe fails anyway, and with
    // it the screen path has already been exercised for real by use.
    if !std::path::Path::new("/dev/nvidiactl").exists() {
        return Check::not_applicable("nvenc", "no NVIDIA GPU: this machine can watch, not be watched");
    }
    match crate::setup_common::run_timed("nvidia-smi", &["--query-gpu=name", "--format=csv,noheader"], 5) {
        Ok(s) => {
            let name = s.lines().next().unwrap_or("").trim().to_string();
            Check::ok("nvenc", if name.is_empty() { "driver loaded".into() } else { name })
        }
        Err(e) => Check::error("nvenc", format!("nvidia-smi: {e}")),
    }
}

fn tray() -> Check {
    // `pgrep -f` with the pattern the deploy skill uses to kill the
    // process. Matching on the name "electron" alone would catch any
    // Electron app on the machine.
    // `NameHasOwner` and not a `pgrep`: the question is whether something
    // on this session bus will DRAW an item, and the answer is owned by
    // the bus, not by any process name we could guess.
    let watcher = crate::setup_common::run_timed(
        "busctl",
        &[
            "--user",
            "call",
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameHasOwner",
            "s",
            "org.kde.StatusNotifierWatcher",
        ],
        5,
    )
    .map(|o| o.trim().ends_with("true"))
    .unwrap_or(false);
    crate::setup_common::tray_verdict(watcher, crate::tray::is_published())
}

// ------------------------------------------------------------------ repairs

pub fn repair(id: &str, cfg: &Config) -> Result<String, String> {
    match id {
        "config" => crate::setup_common::write_config(),
        "daemon" => {
            // `enable --now` on a unit that already runs is idempotent,
            // and it is what fixes the "active but not enabled" case
            // without killing the daemon answering this request.
            crate::setup_common::run_timed(
                "systemctl",
                &["--user", "enable", "--now", "oxdesk.service"],
                15,
            )?;
            Ok("unit habilitada".into())
        }
        "clipboard" => crate::setup_common::test_clipboard(),
        "tray" => enable_appindicators(),
        "ota" => install_clock(),
        _ => Err(format!("'{id}' has no automatic fix on this machine")),
    }
    .map(|s| {
        let _ = cfg;
        s
    })
}

/// Writes the `oxdesk-ota` service and timer and enables them.
///
/// The unit is **generated here**, not copied from the repo: the wizard
/// has to work on a machine without the checkout (the Mac's case). And
/// `%h` resolves to the home of whoever runs it, so the same text serves
/// any account.
///
/// The first run is `--check`, on purpose: `oxdesk-ota` without
/// arguments may apply a new build and **restart oxdesk** -- which is
/// what is answering this request. The clock applies on the next tick,
/// with nobody waiting for an answer.
fn install_clock() -> Result<String, String> {
    let bin = bin_dir().join("oxdesk-ota");
    if !bin.is_file() {
        return Err(format!("could not find {}: run the deploy", bin.display()));
    }
    let dir = crate::sys::home_dir().join(".config/systemd/user");
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    // `concat!` and not line continuation with `\`: the previous version
    // wrote the file with the source code's indentation inside. systemd
    // swallowed it and it worked, but a unit with leading whitespace is a
    // time bomb.
    let service = concat!(
        "[Unit]\n",
        "Description=oxdesk-ota - checks the published Release and applies it if newer\n",
        // Not a path of this machine: this unit is WRITTEN on the
        // user's disk, and a `file:///home/<someone>/dev/...` there
        // points at a checkout they do not have.
        "Documentation=https://github.com/oxlayer/oxdesk/blob/main/specs/015-ota.md\n",
        "\n",
        "[Service]\n",
        "Type=oneshot\n",
        "ExecStart=%h/.local/bin/oxdesk-ota\n",
    );
    // `OnActiveSec` and not `OnBootSec`: on a machine that has been on
    // for hours, `OnBootSec=3min` has already passed, and `enable --now`
    // fires the service RIGHT AWAY -- that is, the wizard button would
    // apply a new build and restart `oxdesk` in the middle of the request
    // that installed it. That is what happened here in the first version
    // (`list-timers` showed "LAST 495ms ago" right after the POST).
    // Relative to the timer's activation, the first check lands 10 min
    // later -- after installing or booting the machine -- and nobody is
    // waiting for an answer anymore.
    let timer = concat!(
        "[Unit]\n",
        "Description=oxdesk-ota timer\n",
        "\n",
        "[Timer]\n",
        "OnActiveSec=10min\n",
        "OnUnitActiveSec=1h\n",
        "Unit=oxdesk-ota.service\n",
        "\n",
        "[Install]\n",
        "WantedBy=timers.target\n",
    );

    std::fs::write(dir.join("oxdesk-ota.service"), service)
        .map_err(|e| format!("write oxdesk-ota.service: {e}"))?;
    std::fs::write(dir.join("oxdesk-ota.timer"), timer)
        .map_err(|e| format!("write oxdesk-ota.timer: {e}"))?;

    crate::setup_common::run_timed("systemctl", &["--user", "daemon-reload"], 15)?;
    crate::setup_common::run_timed("systemctl", &["--user", "enable", "--now", "oxdesk-ota.timer"], 15)?;

    // Check what stuck, not what I ordered.
    clock()?;
    let first = crate::setup_common::run_timed(&bin.to_string_lossy(), &["--check"], 60);
    Ok(match first {
        Ok(s) => format!("timer habilitado; primeira checagem: {}", s.trim()),
        Err(e) => format!("timer enabled, but the first check failed: {e}"),
    })
}

/// Launches the Electron shell.
///
/// `env -u ELECTRON_RUN_AS_NODE` is not optional and not superstition:
/// with that variable in the environment electron runs as plain node,
/// `app` comes back `undefined` and the process dies in
/// `requestSingleInstanceLock()` -- with a message that says nothing
/// about the variable. I already brought the user's GUI down that way,
/// and the daemon inherits the environment of whoever launched it, so

/// Turns the AppIndicator extension back on, and waits for the icon.
///
/// This replaced `launch_tray`, which started an **Electron** process
/// from `~/dev/oxdesk/gui` -- the tray implementation `tray.rs` had
/// already replaced, at a developer checkout path that does not exist on
/// anybody else's machine. It was a fix for a check that measured the
/// wrong thing, and it could only ever fail.
///
/// What actually goes wrong here is the extension being off, which takes
/// the watcher off the bus. The daemon's own retry loop publishes the
/// icon again within 30 s of the watcher coming back (see
/// `wait::retry_delay`), so this does not touch the daemon at all.
fn enable_appindicators() -> Result<String, String> {
    const EXT: &str = "ubuntu-appindicators@ubuntu.com";
    crate::setup_common::run_timed("gnome-extensions", &["enable", EXT], 10).map_err(|e| {
        format!("could not enable {EXT}: {e} (not GNOME? then enable your desktop's tray by hand)")
    })?;
    // Measured, not announced: enabling is not drawing, and the retry loop
    // needs a moment to notice the watcher. Up to ~8 s covers the first
    // three retries.
    for _ in 0..16 {
        std::thread::sleep(Duration::from_millis(500));
        if tray().state == State::Ok {
            return Ok("the tray is up".into());
        }
    }
    Err(format!("{EXT} is enabled, but no icon appeared yet -- check the log"))
}
