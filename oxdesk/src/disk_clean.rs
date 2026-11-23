// SPDX-FileCopyrightText: 2026 oxlayer
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Listing and cleaning what rebuilds itself on this machine's system drive.
//!
//! Spec: ../../specs/027-disk-space.md (parts 2 and 3)
//!
//! The rules, each from something that went wrong on 2026-11-18:
//!
//! - **Fixed paths only, never a search.** The list is `places()`. Cleaning
//!   accepts ids from it and nothing else, so no request can name a path.
//! - **Never through a link.** A place whose path is, or passes through, a
//!   junction or symbolic link is reported as moved and not touched. Inside a
//!   place, a link is neither followed nor removed. That evening a cache
//!   folder became a junction to the data drive, and a cleanup that followed
//!   it would have deleted there.
//! - **Only on the system drive.** A place that lives on another drive is
//!   reported as moved: this feature is about the drive that ran out.
//! - **A file in use stays**, counted as left in use, not as a failure.
//! - **The gain is measured on the drive**, free space before and after, and
//!   compared with what was counted.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;

// Only the Windows list exists until part 3 of the spec writes the others.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Group {
    /// Rebuilds at no cost: ticked by default.
    Free,
    /// Rebuilds with a download or a slower first run: unticked.
    Download,
}

/// Which entries inside a place may go.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy)]
enum Rule {
    /// Everything inside the folder; the folder itself stays.
    Everything,
    /// Top-level entries in which nothing was written for `days` days,
    /// except the names in `keep`.
    Untouched { days: u64, keep: &'static [&'static str] },
    /// Top-level files whose name passes the test.
    Files(fn(&str) -> bool),
}

struct Place {
    id: &'static str,
    group: Group,
    label: &'static str,
    paths: Vec<PathBuf>,
    rule: Rule,
}

/// Folders under Temp that belong to another program and hold its data.
/// On 2026-11-18 the biggest folder in Temp (4.5 GB) was an agent's
/// scratch space with three database dumps in it: old, under Temp, and not
/// disposable.
const TEMP_KEEP: &[&str] = &["claude"];

/// OxDesk's own leftovers: the copy an update moves aside
/// (`oxdesk-host.old-222640.exe`) and the previous log (`oxdesk-kvm.log.1`).
/// A copy kept on purpose for rollback (`oxdesk.release-310.exe`) is not one.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_oxdesk_leftover(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    (n.contains(".old-") && n.ends_with(".exe")) || n.ends_with(".log.1")
}

#[cfg(windows)]
fn places() -> Vec<Place> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let (Some(local), Some(home)) = (env("LOCALAPPDATA"), env("USERPROFILE")) else {
        return Vec::new();
    };
    let mut v = vec![
        Place {
            id: "temp",
            group: Group::Free,
            label: "Temporary files untouched for 7 days",
            paths: vec![std::env::temp_dir()],
            rule: Rule::Untouched { days: 7, keep: TEMP_KEEP },
        },
        Place {
            id: "gpu-shaders",
            group: Group::Free,
            label: "GPU shader caches",
            paths: vec![
                local.join(r"NVIDIA\DXCache"),
                local.join(r"NVIDIA\GLCache"),
                local.join("D3DSCache"),
                local.join(r"AMD\DxCache"),
            ],
            rule: Rule::Everything,
        },
        Place {
            id: "oxdesk-leftovers",
            group: Group::Free,
            label: "OxDesk's own leftovers (old binaries, previous logs)",
            paths: vec![local.join("oxdesk")],
            rule: Rule::Files(is_oxdesk_leftover),
        },
    ];
    // Developer caches: where the tool itself says, not the default path. On
    // the machine of 2026-11-18 npm and bun had already moved to the data
    // drive, and the default paths held only old copies.
    let go = ask("go", &["env", "GOCACHE", "GOMODCACHE"]);
    let mut go = go.as_deref().unwrap_or("").lines().map(str::trim).filter(|l| !l.is_empty());
    let (go_build, go_mod) = (go.next().map(PathBuf::from), go.next().map(PathBuf::from));
    let npm = ask("cmd", &["/C", "npm", "config", "get", "cache"]).map(|s| PathBuf::from(s.trim()));
    let dev: [(&str, &str, Option<PathBuf>); 5] = [
        ("go-build", "Go build cache", go_build),
        ("go-modules", "Go module downloads", go_mod),
        ("npm", "npm package cache", npm),
        ("playwright", "Browsers downloaded by Playwright", Some(local.join("ms-playwright"))),
        ("puppeteer", "Browsers downloaded by Puppeteer", Some(home.join(r".cache\puppeteer"))),
    ];
    for (id, label, path) in dev {
        if let Some(p) = path.filter(|p| p.is_absolute()) {
            v.push(Place { id, group: Group::Download, label, paths: vec![p], rule: Rule::Everything });
        }
    }
    v
}

/// The Linux and macOS lists are part 3 of the spec.
#[cfg(not(windows))]
fn places() -> Vec<Place> {
    Vec::new()
}

/// A tool's answer, or `None` if it is not installed or took too long.
#[cfg(windows)]
fn ask(prog: &str, args: &[&str]) -> Option<String> {
    use std::process::Stdio;
    let mut c = crate::sys::child(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let t0 = Instant::now();
    loop {
        match c.try_wait() {
            Ok(Some(s)) if s.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if t0.elapsed() > Duration::from_secs(8) => {
                let _ = c.kill();
                let _ = c.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut c.stdout.take()?, &mut out).ok()?;
    Some(out)
}

/// Is this entry a link of any kind: symbolic link, junction, or another
/// reparse point?
fn is_link(md: &std::fs::Metadata) -> bool {
    if md.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    false
}

/// Does `path`, or any folder above it, pass through a link?
fn crosses_link(path: &Path) -> bool {
    let mut p = PathBuf::new();
    for c in path.components() {
        p.push(c);
        if matches!(c, Component::Prefix(_) | Component::RootDir) {
            continue;
        }
        if std::fs::symlink_metadata(&p).is_ok_and(|md| is_link(&md)) {
            return true;
        }
    }
    false
}

/// Is `path` on the system drive? On Windows, by its drive letter against
/// `%SystemDrive%`; elsewhere every path is, until part 3 says otherwise.
fn on_system_drive(path: &Path) -> bool {
    #[cfg(windows)]
    {
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
        let s = path.to_string_lossy();
        return s.len() >= 2 && s[..2].eq_ignore_ascii_case(&sys[..sys.len().min(2)]);
    }
    #[allow(unreachable_code)]
    {
        let _ = path;
        true
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Tally {
    bytes: u64,
    items: u64,
    newest: Option<SystemTime>,
}

impl Tally {
    fn add(&mut self, o: Tally) {
        self.bytes += o.bytes;
        self.items += o.items;
        self.newest = self.newest.max(o.newest);
    }
}

/// Size, file count and newest write under `path`, without following any
/// link. `false` when `deadline` passed before the walk finished.
fn tally(path: &Path, t: &mut Tally, deadline: Option<Instant>) -> bool {
    let Ok(md) = std::fs::symlink_metadata(path) else { return true };
    if is_link(&md) {
        return true;
    }
    t.newest = t.newest.max(md.modified().ok());
    if md.is_file() {
        t.bytes += md.len();
        t.items += 1;
        return true;
    }
    if md.is_dir() {
        if let Ok(rd) = std::fs::read_dir(path) {
            for e in rd.flatten() {
                if deadline.is_some_and(|d| Instant::now() > d) {
                    return false;
                }
                if !tally(&e.path(), t, deadline) {
                    return false;
                }
            }
        }
    }
    true
}

/// The top-level entries of a place that its rule lets go, with their
/// tallies. Links are never among them.
fn chosen(place: &Place, dir: &Path, now: SystemTime) -> Vec<(PathBuf, Tally)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in rd.flatten() {
        let path = e.path();
        let Ok(md) = std::fs::symlink_metadata(&path) else { continue };
        if is_link(&md) {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let pass = match place.rule {
            Rule::Everything => true,
            Rule::Files(test) => md.is_file() && test(&name),
            Rule::Untouched { keep, .. } if keep.iter().any(|k| k.eq_ignore_ascii_case(&name)) => false,
            Rule::Untouched { .. } => true,
        };
        if !pass {
            continue;
        }
        let mut t = Tally::default();
        tally(&path, &mut t, None);
        if let Rule::Untouched { days, .. } = place.rule {
            let cut = now - Duration::from_secs(days * 86_400);
            if t.newest.is_none_or(|n| n >= cut) {
                continue;
            }
        }
        out.push((path, t));
    }
    out
}

#[derive(Debug, Serialize)]
pub struct PlaceReport {
    pub id: &'static str,
    pub group: Group,
    pub label: &'static str,
    pub paths: Vec<String>,
    /// `ok`, `empty`, `absent`, or `moved` (through a link, or on another
    /// drive: not touched).
    pub state: &'static str,
    pub bytes: u64,
    pub items: u64,
    /// The newest write among what would go, `YYYY-MM-DD`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_written: Option<String>,
}

/// A big folder the cleanup will not touch, so the person knows where the
/// rest of the space is.
#[derive(Debug, Serialize)]
pub struct Elsewhere {
    pub path: String,
    pub bytes: u64,
    /// `false` when the walk ran out of time: the size is at least this.
    pub complete: bool,
}

#[derive(Debug, Serialize)]
pub struct Listing {
    pub free_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub places: Vec<PlaceReport>,
    pub elsewhere: Vec<Elsewhere>,
    /// What needs an elevated terminal, for the person to run.
    pub admin: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
}

fn state_of(paths: &[PathBuf]) -> &'static str {
    let present: Vec<&PathBuf> = paths.iter().filter(|p| std::fs::symlink_metadata(p).is_ok()).collect();
    if present.is_empty() {
        "absent"
    } else if present.iter().any(|p| crosses_link(p) || !on_system_drive(p)) {
        "moved"
    } else {
        "ok"
    }
}

fn day(t: SystemTime) -> String {
    let secs = t.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// What could be reclaimed on this machine. Reads only.
pub fn list() -> Listing {
    let (free_bytes, total_bytes) = drive_space().map_or((None, None), |(f, t)| (Some(f), Some(t)));
    let now = SystemTime::now();
    let places = places();
    let mut reports = Vec::new();
    for p in &places {
        let mut state = state_of(&p.paths);
        let mut t = Tally::default();
        if state == "ok" {
            for dir in &p.paths {
                for (_, one) in chosen(p, dir, now) {
                    t.add(one);
                }
            }
            if t.items == 0 {
                state = "empty";
            }
        }
        reports.push(PlaceReport {
            id: p.id,
            group: p.group,
            label: p.label,
            paths: p.paths.iter().map(|x| x.display().to_string()).collect(),
            state,
            bytes: t.bytes,
            items: t.items,
            last_written: t.newest.map(day),
        });
    }
    Listing {
        free_bytes,
        total_bytes,
        elsewhere: elsewhere(&places),
        places: reports,
        admin: if cfg!(windows) { vec!["dism /Online /Cleanup-Image /StartComponentCleanup"] } else { vec![] },
        note: (!cfg!(windows)).then_some("the list for this system is not written yet (specs/027, part 3)"),
    }
}

/// The biggest folders in the user's own areas that are not on the list,
/// within a time budget: a profile of tens of GB takes a while to walk.
fn elsewhere(places: &[Place]) -> Vec<Elsewhere> {
    #[cfg(windows)]
    let roots: Vec<PathBuf> = ["USERPROFILE", "LOCALAPPDATA", "APPDATA"]
        .iter()
        .filter_map(|k| std::env::var_os(k).map(PathBuf::from))
        .collect();
    #[cfg(not(windows))]
    let roots: Vec<PathBuf> = std::env::var_os("HOME").map(PathBuf::from).into_iter().collect();

    let listed: Vec<&PathBuf> = places.iter().flat_map(|p| p.paths.iter()).collect();
    let mut candidates: Vec<PathBuf> = Vec::new();
    for r in &roots {
        let Ok(rd) = std::fs::read_dir(r) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(md) = std::fs::symlink_metadata(&p) else { continue };
            // A root inside another root (AppData under the profile) is
            // walked as its own root, not as one entry.
            if !md.is_dir() || is_link(&md) || roots.contains(&p) {
                continue;
            }
            if roots.iter().any(|r2| r2 != r && r2.starts_with(&p)) {
                continue;
            }
            // A listed place, or a folder that holds one, is counted above.
            if listed.iter().any(|l| l.starts_with(&p)) {
                continue;
            }
            candidates.push(p);
        }
    }
    // Temp's kept folders are exactly the ones a person should hear about.
    for k in TEMP_KEEP {
        candidates.push(std::env::temp_dir().join(k));
    }
    // Several walkers share the list, each taking the next folder. One at a
    // time, the first run on the reference Windows machine spent the whole
    // budget on two folders of small files in the profile and never reached
    // AppData, where 14 GB of installed apps and 6 GB of another program's
    // temp folders were: the two things a person most needed to hear about.
    let deadline = Instant::now() + Duration::from_secs(20);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let found = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..6 {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(p) = candidates.get(i) else { break };
                let mut t = Tally::default();
                let complete = tally(p, &mut t, Some(deadline));
                if t.bytes >= 256 << 20 {
                    if let Ok(mut f) = found.lock() {
                        f.push(Elsewhere { path: p.display().to_string(), bytes: t.bytes, complete });
                    }
                }
            });
        }
    });
    let mut out = found.into_inner().unwrap_or_default();
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    out.truncate(8);
    out
}

#[derive(Debug, Default, Serialize)]
pub struct PlaceDone {
    pub id: String,
    pub deleted_bytes: u64,
    pub deleted_items: u64,
    pub left_in_use_bytes: u64,
    pub state: &'static str,
}

#[derive(Debug, Serialize)]
pub struct CleanReport {
    pub counted_bytes: u64,
    /// Free space after minus before. Can be smaller than counted (others
    /// wrote meanwhile) or larger (others freed).
    pub gained_bytes: Option<i64>,
    pub left_in_use_bytes: u64,
    pub places: Vec<PlaceDone>,
    pub verdict: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unknown: Vec<String>,
}

/// What the two numbers say together. The drive's word is the one that
/// counts; the tally only says what to expect.
fn verdict(counted: u64, gained: Option<i64>) -> &'static str {
    match gained {
        _ if counted == 0 => "nothing was deleted",
        None => "deleted, but the drive's free space could not be measured",
        Some(g) if g as f64 >= counted as f64 * 0.5 => "the drive gained what was deleted",
        Some(_) => "the drive gained much less than was deleted: something wrote to it meanwhile, or files were held open",
    }
}

#[derive(Default)]
struct Removal {
    bytes: u64,
    items: u64,
    left_bytes: u64,
}

/// Deletes `path` bottom-up, file by file, so one file in use costs only
/// itself. Never removes or enters a link.
fn remove(path: &Path, r: &mut Removal) {
    let Ok(md) = std::fs::symlink_metadata(path) else { return };
    if is_link(&md) {
        return;
    }
    if md.is_dir() {
        if let Ok(rd) = std::fs::read_dir(path) {
            for e in rd.flatten() {
                remove(&e.path(), r);
            }
        }
        // Fails while anything stayed inside: that is fine.
        let _ = std::fs::remove_dir(path);
        return;
    }
    let len = md.len();
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut done = std::fs::remove_file(path);
    // Go's module cache is read-only on purpose; on Windows that blocks the
    // delete. Clearing the flag is enough there (on Unix the directory's
    // permission is what matters, and the flag means something else).
    #[cfg(windows)]
    if done.is_err() && md.permissions().readonly() {
        let mut p = md.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        p.set_readonly(false);
        if std::fs::set_permissions(path, p).is_ok() {
            done = std::fs::remove_file(path);
        }
    }
    match done {
        Ok(()) => {
            r.bytes += len;
            r.items += 1;
        }
        Err(_) => r.left_bytes += len,
    }
}

/// Cleans the places named by `ids`, and only places from the list.
pub fn clean(ids: &[String]) -> CleanReport {
    let before = drive_space().map(|(f, _)| f);
    let now = SystemTime::now();
    let places = places();
    let unknown: Vec<String> = ids.iter().filter(|i| !places.iter().any(|p| p.id == *i)).cloned().collect();
    let mut done = Vec::new();
    for p in places.iter().filter(|p| ids.iter().any(|i| i == p.id)) {
        let state = state_of(&p.paths);
        let mut r = Removal::default();
        if state == "ok" {
            for dir in &p.paths {
                for (entry, _) in chosen(p, dir, now) {
                    remove(&entry, &mut r);
                }
            }
        }
        say!(
            "disk: cleaned {} -- {} file(s), {:.2} GB; {:.2} GB left in use ({state})",
            p.id,
            r.items,
            r.bytes as f64 / 1e9,
            r.left_bytes as f64 / 1e9
        );
        done.push(PlaceDone {
            id: p.id.to_string(),
            deleted_bytes: r.bytes,
            deleted_items: r.items,
            left_in_use_bytes: r.left_bytes,
            state,
        });
    }
    let after = drive_space().map(|(f, _)| f);
    let counted: u64 = done.iter().map(|d| d.deleted_bytes).sum();
    let gained = before.zip(after).map(|(b, a)| a as i64 - b as i64);
    let v = verdict(counted, gained);
    say!(
        "disk: cleanup counted {:.2} GB, the drive gained {} -- {v}",
        counted as f64 / 1e9,
        gained.map_or("unknown".to_string(), |g| format!("{:.2} GB", g as f64 / 1e9))
    );
    CleanReport {
        counted_bytes: counted,
        gained_bytes: gained,
        left_in_use_bytes: done.iter().map(|d| d.left_in_use_bytes).sum(),
        places: done,
        verdict: v,
        unknown,
    }
}

/// Free space a user can write, and the size, of the system drive.
#[cfg(windows)]
pub fn drive_space() -> Option<(u64, u64)> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, avail: *mut u64, total: *mut u64, free: *mut u64) -> i32;
    }
    let root = format!("{}\\", std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()));
    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
    // SAFETY: a NUL-terminated wide string and three valid out pointers.
    let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut avail, &mut total, &mut free) };
    (ok != 0).then_some((avail, total))
}

#[cfg(target_os = "linux")]
pub fn drive_space() -> Option<(u64, u64)> {
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a valid out pointer.
    let ok = unsafe { libc::statvfs(c"/".as_ptr(), &mut s) };
    (ok == 0).then(|| (s.f_bavail as u64 * s.f_frsize as u64, s.f_blocks as u64 * s.f_frsize as u64))
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn drive_space() -> Option<(u64, u64)> {
    None
}

/// `oxdesk disk-list` and `oxdesk disk-clean <id,id>`: the same answers as
/// the API, as JSON on stdout, for another machine's panel over SSH.
pub fn cli(args: &[String]) -> std::process::ExitCode {
    let json = match args.first().map(String::as_str) {
        Some("disk-list") => serde_json::to_string(&list()),
        Some("disk-clean") => {
            let ids: Vec<String> = args
                .get(1)
                .map(|s| s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
                .unwrap_or_default();
            if ids.is_empty() {
                eprintln!("disk-clean: give the ids to clean, comma-separated (see disk-list)");
                return std::process::ExitCode::FAILURE;
            }
            serde_json::to_string(&clean(&ids))
        }
        _ => return std::process::ExitCode::FAILURE,
    };
    match json {
        Ok(s) => {
            println!("{s}");
            std::process::ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("disk: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oxdesk_leftovers_are_the_moved_aside_copies_and_old_logs_only() {
        assert!(is_oxdesk_leftover("oxdesk-agent.old-000958.exe"));
        assert!(is_oxdesk_leftover("oxdesk-host.old-222640.exe"));
        assert!(is_oxdesk_leftover("oxdesk-kvm.log.1"));
        assert!(!is_oxdesk_leftover("oxdesk.exe"), "the live binary");
        assert!(!is_oxdesk_leftover("oxdesk-host.release-310.exe"), "kept on purpose for rollback");
        assert!(!is_oxdesk_leftover("oxdesk-host.log"), "the current log");
        assert!(!is_oxdesk_leftover("config.toml"));
    }

    #[test]
    fn the_verdict_believes_the_drive() {
        assert_eq!(verdict(0, Some(0)), "nothing was deleted");
        assert_eq!(verdict(8_060, Some(8_470)), "the drive gained what was deleted");
        assert!(verdict(8_000, Some(100)).starts_with("the drive gained much less"));
        assert!(verdict(8_000, Some(-50)).starts_with("the drive gained much less"));
        assert!(verdict(10, None).contains("could not be measured"));
    }

    #[test]
    fn dates_are_civil_days() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_795_000_000);
        assert_eq!(day(t), "2026-11-18");
        assert_eq!(day(SystemTime::UNIX_EPOCH), "1970-01-01");
        assert_eq!(day(SystemTime::UNIX_EPOCH + Duration::from_secs(951_782_400)), "2000-02-29");
    }

    /// A scratch tree of its own, removed at the end.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Scratch {
            let p = std::env::temp_dir().join(format!("oxdesk-disk-test-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            // Canonical: macOS's temp folder sits under /var, which is a
            // link, and every place under it would read as moved.
            Scratch(std::fs::canonicalize(&p).unwrap())
        }
        fn file(&self, rel: &str, bytes: usize) -> PathBuf {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, vec![b'x'; bytes]).unwrap();
            p
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn place(rule: Rule, dir: &Path) -> Place {
        Place { id: "t", group: Group::Free, label: "t", paths: vec![dir.to_path_buf()], rule }
    }

    /// Everything inside goes, and the place itself stays.
    #[test]
    fn everything_empties_the_folder_and_keeps_it() {
        let s = Scratch::new("everything");
        s.file("a/b/c.bin", 1000);
        s.file("d.bin", 500);
        let p = place(Rule::Everything, &s.0);
        let mut r = Removal::default();
        for (e, _) in chosen(&p, &s.0, SystemTime::now()) {
            remove(&e, &mut r);
        }
        assert_eq!((r.bytes, r.items), (1500, 2));
        assert!(s.0.is_dir(), "the place itself stays");
        assert_eq!(std::fs::read_dir(&s.0).unwrap().count(), 0);
    }

    /// Recent entries and kept names stay; only what nobody wrote to for
    /// the whole window goes.
    #[test]
    fn untouched_spares_the_recent_and_the_kept() {
        let s = Scratch::new("untouched");
        s.file("recent/x.bin", 10);
        s.file("claude/dump.sql", 10);
        s.file("old.bin", 10);
        let p = place(Rule::Untouched { days: 7, keep: &["claude"] }, &s.0);
        // Judged from 30 days ahead: everything is old except by name.
        let later = SystemTime::now() + Duration::from_secs(30 * 86_400);
        let mut names: Vec<String> = chosen(&p, &s.0, later)
            .into_iter()
            .map(|(e, _)| e.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["old.bin", "recent"], "kept by name: claude");
        // Judged now: nothing is old enough.
        assert!(chosen(&p, &s.0, SystemTime::now()).is_empty());
    }

    /// A link inside a place is neither followed nor removed, and what it
    /// points to survives. On 2026-11-18 a cache folder became a junction to
    /// the data drive.
    #[cfg(unix)]
    #[test]
    fn a_link_inside_a_place_is_not_followed() {
        let outside = Scratch::new("outside");
        let precious = outside.file("precious.bin", 100);
        let s = Scratch::new("withlink");
        s.file("junk.bin", 10);
        std::os::unix::fs::symlink(&outside.0, s.0.join("moved")).unwrap();
        let p = place(Rule::Everything, &s.0);
        let mut r = Removal::default();
        for (e, _) in chosen(&p, &s.0, SystemTime::now()) {
            remove(&e, &mut r);
        }
        assert_eq!((r.bytes, r.items), (10, 1));
        assert!(precious.exists(), "the link's target survives");
        assert!(s.0.join("moved").symlink_metadata().is_ok(), "the link itself stays");
    }

    /// A place reached through a link is not touched at all.
    #[cfg(unix)]
    #[test]
    fn a_place_through_a_link_is_moved() {
        let real = Scratch::new("real");
        real.file("cache/x.bin", 10);
        let s = Scratch::new("linkparent");
        std::os::unix::fs::symlink(&real.0, s.0.join("go")).unwrap();
        assert_eq!(state_of(&[s.0.join("go").join("cache")]), "moved");
        assert_eq!(state_of(&[real.0.join("cache")]), "ok");
        assert_eq!(state_of(&[real.0.join("nowhere")]), "absent");
    }
}
