// SPDX-FileCopyrightText: 2026 oxlayer
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The whole path: check, download, verify, sign, swap, restart, verify
//! again, and roll back if it does not answer.
//!
//! Spec: ../../specs/015-ota.md
//!
//! The order is not arbitrary. **Signing and testing happen in the
//! staging folder, before any swap**: a Mac binary that does not run
//! (`oxdesk-host` links against brew's ffmpeg, and CI compiles
//! against the runner's version) has to be refused *before* it replaces
//! what works, not after. Swapping and then finding out is what requires
//! rolling back; checking first is what avoids needing to.
//!
//! Error strings end up in the state's message, which the panel shows:
//! they stay in Portuguese.

use std::fs;
use std::path::{Path, PathBuf};

use crate::exec;
use crate::plan::{self, Plan, System};
use crate::release::{self, Decision};
use crate::sha256;
use crate::state::{self, State};

/// The repo the build comes from. Overridable through the environment to
/// test against another repo without recompiling.
const REPO: &str = "oxlayer/oxdesk";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Check and report, without touching any binary.
    Check,
    /// Check and, if there is something new, apply it.
    Apply,
    /// Apply even if the stamp is the same (reinstall).
    Force,
}

pub struct Context {
    pub system: System,
    pub plan: Plan,
    pub base: PathBuf,
    pub dir: PathBuf,
    pub install_dir: PathBuf,
    pub mode: Mode,
    pub repo: String,
    pub token: Option<String>,
}

fn base() -> Result<PathBuf, String> {
    #[cfg(windows)]
    let v = std::env::var_os("LOCALAPPDATA");
    #[cfg(not(windows))]
    let v = std::env::var_os("HOME");
    v.map(PathBuf::from).ok_or_else(|| "I do not know where this machine's home is".to_string())
}

/// The uid, taken from the owner of the home directory itself.
///
/// `launchctl kickstart gui/<uid>/...` needs it. It comes from the owner
/// of `$HOME` instead of `libc::getuid` so as not to pull in `libc` as a
/// dependency for one number.
fn uid_of(home: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return fs::metadata(home).map(|m| m.uid()).unwrap_or(0);
    }
    #[cfg(not(unix))]
    {
        let _ = home;
        0
    }
}

fn read_token(s: System, base: &Path) -> Option<String> {
    if let Ok(t) = std::env::var("OXDESK_OTA_TOKEN") {
        if !t.trim().is_empty() {
            return Some(t);
        }
    }
    let p = plan::token_path(s, base);
    fs::read_to_string(p).ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

pub fn context(mode: Mode) -> Result<Context, String> {
    let system = plan::current();
    let base = base()?;
    let plan = plan::plan(system, uid_of(&base));
    let dir = plan::state_dir(system, &base);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let token = read_token(system, &base);
    Ok(Context {
        system,
        install_dir: plan::install_dir(system, &base),
        plan,
        base,
        dir,
        mode,
        repo: std::env::var("OXDESK_OTA_REPO").unwrap_or_else(|_| REPO.to_string()),
        token,
    })
}

fn system_name(s: System) -> &'static str {
    match s {
        System::Linux => "linux",
        System::Mac => "mac",
        System::Windows => "windows",
    }
}

/// What is running here, right now.
///
/// First the live daemon's `/healthz`, which is the only source that does
/// not lie: the file on disk may be new and the process old. If the
/// daemon is down, fall back to the last recorded state -- and if not even
/// that exists, assume `0`, which makes OTA install whatever there is.
/// That is deliberate: a dead daemon with an unknown build is exactly the
/// case where updating is the way out.
fn installed(ctx: &Context) -> (u64, String) {
    let url = format!("http://127.0.0.1:{}/healthz", port(&ctx.base, ctx.system));
    if let Ok(txt) = exec::api(&url, None, 5) {
        if let Some((n, b)) = release::parse_healthz(&txt) {
            return (n, b);
        }
    }
    match state::read(&ctx.dir) {
        Some(e) if e.n_here > 0 => (e.n_here, e.build_here),
        _ => (0, "?".to_string()),
    }
}

/// Check, and apply when appropriate. Always records the state --
/// including when it fails, which is exactly when someone needs to read it.
pub fn run(mode: Mode) -> Result<State, State> {
    let ctx = match context(mode) {
        Ok(c) => c,
        Err(e) => {
            // Without a context there is nowhere to record. Return the bare error.
            let mut st = State::new("?", 0, "?");
            st.message = e;
            return Err(st);
        }
    };

    let (n_here, build_here) = installed(&ctx);
    let mut st = State::new(system_name(ctx.system), n_here, &build_here);

    let r = steps(&ctx, &mut st);
    match r {
        Ok(()) => {
            let _ = state::write(&ctx.dir, &st);
            Ok(st)
        }
        Err(msg) => {
            if st.result == "error" {
                st.message = msg;
            }
            st.when_unix = state::now();
            let _ = state::write(&ctx.dir, &st);
            Err(st)
        }
    }
}

fn steps(ctx: &Context, st: &mut State) -> Result<(), String> {
    // `OXDESK_OTA_API` exists so the whole path -- download, check the sum,
    // extract, swap, restart, check /healthz, roll back -- can be
    // exercised against a local server, without publishing a fake Release
    // and without touching the single source of truth all three machines
    // consume. That is how the rollback was tested.
    let api = std::env::var("OXDESK_OTA_API")
        .unwrap_or_else(|_| "https://api.github.com".to_string());
    let url = format!("{api}/repos/{}/releases/latest", ctx.repo);
    let json = exec::api(&url, ctx.token.as_deref(), 30).map_err(|e| {
        if ctx.token.is_none() {
            format!("{e} -- and there is no token at {}", plan::token_path(ctx.system, &ctx.base).display())
        } else {
            e
        }
    })?;
    // "Not Found" has two causes, and the public repository made the first
    // the likely one: no Release has been published there yet. A private
    // fork with no token is the other, so the message names both.
    let l = release::parse(&json).map_err(|e| {
        if ctx.token.is_none() {
            format!("{e} (no Release published in {} yet; or, for a private repository, \
                     no read-only token at {})",
                ctx.repo, plan::token_path(ctx.system, &ctx.base).display())
        } else {
            e
        }
    })?;
    st.tag = l.tag.clone();

    let asset = release::pick(&l, ctx.system)?.clone();
    st.package = asset.name.clone();

    let decision = release::decide(st.n_here, &st.build_here, &asset.name);
    if let Decision::Update { n, hash } | Decision::Same { n, hash } = &decision {
        st.n_there = *n;
        st.build_there = hash.clone();
    }

    match decision {
        Decision::NoStamp { name } => {
            return Err(format!("package {name} has no -<n>-<hash>; I will not swap blind"));
        }
        Decision::NewerHere { here, there } => {
            st.n_there = there;
            st.result = "newer-here".into();
            st.message = format!("#{here} here, #{there} published; I do not go down a version");
            return Ok(());
        }
        Decision::Same { .. } if ctx.mode != Mode::Force => {
            st.result = "same".into();
            st.message = format!("Release {} is what already runs here", l.tag);
            return Ok(());
        }
        _ => {}
    }

    if ctx.mode == Mode::Check {
        st.result = "available".into();
        st.message = "only checked (--check)".into();
        return Ok(());
    }
    if let Some(reason) = ctx.plan.refusal {
        st.result = "available".into();
        st.message = reason.to_string();
        return Ok(());
    }

    let sums = release::sums(&l)?.clone();
    install(ctx, st, &asset, &sums)
}

// ---------------------------------------------------------------- swap

fn install(
    ctx: &Context,
    st: &mut State,
    asset: &release::Asset,
    sums: &release::Asset,
) -> Result<(), String> {
    // Staging folder ALWAYS empty: an interrupted download leaves half a
    // file behind, and half a `.tar.gz` with the right name is the kind of
    // thing that passes for "already downloaded".
    let staging = ctx.dir.join("passagem");
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;

    let p_sums = staging.join("SHA256SUMS");
    exec::download(&sums.url, ctx.token.as_deref(), &p_sums, 60)?;
    let txt_sums = fs::read_to_string(&p_sums).map_err(|e| format!("SHA256SUMS: {e}"))?;
    let expected = release::sum_for(&txt_sums, &asset.name)
        .ok_or_else(|| format!("the Release's SHA256SUMS has no line for {}", asset.name))?;

    let p_tar = staging.join(&asset.name);
    let bytes = exec::download(&asset.url, ctx.token.as_deref(), &p_tar, 300)?;

    let data = fs::read(&p_tar).map_err(|e| format!("{}: {e}", p_tar.display()))?;
    let got = sha256::hex(&data);
    if got != expected {
        return Err(format!(
            "the package does not match: expected {expected}, downloaded {got} ({bytes} bytes)"
        ));
    }
    drop(data);

    // The system's `tar`: it exists on all three machines (Windows 10 has
    // shipped `tar.exe` since 1803, and clip_files.rs already uses it on all
    // three).
    let s = exec::run(
        "tar",
        &["-xzf", &p_tar.to_string_lossy(), "-C", &staging.to_string_lossy()],
        120,
    )?;
    if !s.ok {
        return Err(format!("tar failed: {}", first_line(&s.err)));
    }

    let inner = staging.join(asset.name.trim_end_matches(".tar.gz"));
    if !inner.is_dir() {
        return Err(format!("the package has no {} directory", inner.display()));
    }

    let inner_sums = fs::read_to_string(inner.join("SHA256SUMS"))
        .map_err(|e| format!("the package has no inner SHA256SUMS: {e}"))?;
    let items = release::lines(&inner_sums);
    if items.is_empty() {
        return Err("the inner SHA256SUMS lists nothing".into());
    }

    for (sum, name) in &items {
        let f = inner.join(name);
        let d = fs::read(&f).map_err(|e| format!("{name}: {e}"))?;
        let got = sha256::hex(&d);
        if &got != sum {
            return Err(format!("{name} does not match: expected {sum}, got {got}"));
        }
    }

    // The stamp proves this binary is from THIS commit -- the same question
    // `build-binaries.sh` asks before packaging and that `deploy.sh`
    // asks /healthz at the end.
    let daemon = inner.join(ctx.plan.daemon);
    let raw = fs::read(&daemon).map_err(|e| format!("{}: {e}", ctx.plan.daemon))?;
    if !contains(&raw, st.build_there.as_bytes()) {
        return Err(format!(
            "the package's {} does not carry the stamp {}",
            ctx.plan.daemon, st.build_there
        ));
    }
    drop(raw);

    // Sign BEFORE installing and testing: on the Mac the signature is what
    // keeps the *designated requirement* and, with it, the Screen Recording
    // grant. Installing first and signing afterwards would leave a window
    // in which the installed binary is ad-hoc.
    if ctx.plan.signs {
        sign_all(ctx, &inner, &items)?;
    }

    // Test in staging. A binary that does not load is refused here, with
    // the working one still in place.
    for (_, name) in &items {
        check_links(ctx.system, &inner.join(name))?;
    }
    smoke_test_daemon(&daemon)?;

    // --- from here on the machine changes. Backup first.
    let previous = ctx.dir.join("anterior");
    let _ = fs::remove_dir_all(&previous);
    fs::create_dir_all(&previous).map_err(|e| format!("{}: {e}", previous.display()))?;
    let mut saved: Vec<String> = Vec::new();
    for (_, name) in &items {
        let current = ctx.install_dir.join(name);
        if current.is_file() {
            copy(&current, &previous.join(name))?;
            saved.push(name.clone());
        }
    }
    fs::write(previous.join("build"), format!("{} {}\n", st.n_here, st.build_here)).ok();

    for (_, name) in &items {
        copy(&inner.join(name), &ctx.install_dir.join(name))?;
    }

    let restarted = restart(ctx);
    let came_up = restarted.is_ok() && check_healthz(&ctx.base, ctx.system, &st.build_there).is_ok();

    if came_up {
        st.result = "updated".into();
        st.message = format!(
            "{} binary(ies) swapped and /healthz answered {}",
            items.len(),
            st.build_there
        );
        st.n_here = st.n_there;
        st.build_here = st.build_there.clone();
        let _ = fs::remove_dir_all(&staging);
        return Ok(());
    }

    // Roll back. If not even this works, the message has to say where the
    // copy was left -- that is what someone will need at 2 a.m.
    let why = match restarted {
        Err(e) => format!("the restart failed: {e}"),
        Ok(()) => match check_healthz(&ctx.base, ctx.system, &st.build_there) {
            Err(e) => e,
            Ok(()) => "no clear reason".into(),
        },
    };
    let mut restored = true;
    for name in &saved {
        if copy(&previous.join(name), &ctx.install_dir.join(name)).is_err() {
            restored = false;
        }
    }
    let _ = restart(ctx);
    let back = check_healthz(&ctx.base, ctx.system, &st.build_here).is_ok();

    st.result = "rolled-back".into();
    st.message = if restored && back {
        format!("{why}; rolled back to the previous build and it answers")
    } else {
        format!(
            "{why}; the rollback did NOT confirm -- the good copy is at {}",
            previous.display()
        )
    };
    Err(st.message.clone())
}

fn first_line(s: &str) -> String {
    s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("no message").to_string()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|j| j == needle)
}

/// Copies preserving the execute bit, and **deleting the destination first**.
///
/// Writing over a running binary fails with `ETXTBSY` on Linux. Deleting
/// first works because whoever is running keeps the old inode open --
/// and it is what makes the restart pick up the new file.
fn copy(from: &Path, to: &Path) -> Result<(), String> {
    if let Some(p) = to.parent() {
        fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
    }
    let _ = fs::remove_file(to);
    fs::copy(from, to).map_err(|e| format!("{} -> {}: {e}", from.display(), to.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(to, fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

fn sign_all(ctx: &Context, inner: &Path, items: &[(String, String)]) -> Result<(), String> {
    let script = plan::signer(ctx.system, &ctx.base)
        .ok_or_else(|| "this system has no signer".to_string())?;
    if !script.is_file() {
        return Err(format!(
            "{} does not exist; without it the binary lands ad-hoc and the Mac loses Screen Recording",
            script.display()
        ));
    }
    for (_, name) in items {
        let target = inner.join(name);
        let s = exec::run(&script.to_string_lossy(), &[&target.to_string_lossy()], 90)?;
        if !s.ok {
            return Err(format!("assinar {name}: {}", first_line(&s.err)));
        }
        // `sign.sh` is deliberately non-fatal (it does not bring anything
        // down on its own). The one that turns it into a failure is whoever
        // checks -- and here the failure matters: an ad-hoc binary installed
        // means a black screen on the next request.
        let v = exec::run("codesign", &["-dvvv", &target.to_string_lossy()], 30)?;
        let all = format!("{}{}", v.out, v.err);
        if all.contains("adhoc") || !all.contains("Authority=") {
            return Err(format!("{name} still has no identity after signing; run create-identity.sh on the Mac"));
        }
    }
    Ok(())
}

/// Does the binary find the libraries it depends on, on this machine?
///
/// That is the real, known risk of the Mac package: `oxdesk-host` and
/// `-view` link against brew's ffmpeg, CI compiles against the runner's
/// version, and a different major does not run. Check the linkage instead
/// of executing: some of these binaries open a window, and a window
/// popping up on its own in the middle of someone's session is worse than
/// the problem.
fn check_links(s: System, bin: &Path) -> Result<(), String> {
    let name = bin.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match s {
        System::Mac => {
            let r = exec::run("otool", &["-L", &bin.to_string_lossy()], 30)?;
            let missing: Vec<String> = r
                .out
                .lines()
                .map(str::trim)
                .filter_map(|l| l.split(" (").next())
                .filter(|p| p.starts_with("/opt/") || p.starts_with("/usr/local/"))
                .filter(|p| !Path::new(p).exists())
                .map(|p| p.to_string())
                .collect();
            if !missing.is_empty() {
                // A diagnosis with the fix. The Mac package links against
                // the RUNNER's ffmpeg dylibs (measured on the #126 package:
                // libavcodec.63, libavutil.61, libswscale.10,
                // libavformat.63, libavdevice.63), and a different major
                // does not load. Without this line the symptom would be a
                // refusal with no cause.
                return Err(format!(
                    "{name} needs {} and it is not here: the package was linked against the runner's ffmpeg. \
                     Fix: `brew upgrade ffmpeg` on the Mac, or run the deploy, which builds against the ffmpeg FROM HERE",
                    missing.join(", ")
                ));
            }
            Ok(())
        }
        System::Linux => {
            let r = exec::run("ldd", &[&bin.to_string_lossy()], 30)?;
            let missing: Vec<&str> =
                r.out.lines().filter(|l| l.contains("not found")).map(str::trim).collect();
            if !missing.is_empty() {
                return Err(format!("{name}: biblioteca ausente -- {}", missing.join("; ")));
            }
            Ok(())
        }
        System::Windows => Ok(()),
    }
}

/// Does the package's daemon run?
///
/// `--help` opens no port and loads no config (see `run_cli` in
/// `oxdesk/src/main.rs`), so it is safe to run with the real daemon up.
/// What we look for is a **loader** error, not the exit code: the help
/// goes to stderr on purpose, and exiting non-zero is not a symptom.
fn smoke_test_daemon(bin: &Path) -> Result<(), String> {
    let r = exec::run(&bin.to_string_lossy(), &["--help"], 20)
        .map_err(|e| format!("the package's daemon did not run: {e}"))?;
    let all = format!("{}{}", r.out, r.err);
    for signal in [
        "error while loading shared libraries",
        "Library not loaded",
        "Symbol not found",
        "dyld",
        "cannot execute",
        "Exec format error",
    ] {
        if all.contains(signal) {
            return Err(format!("the package's daemon does not load: {}", first_line(&all)));
        }
    }
    Ok(())
}

fn restart(ctx: &Context) -> Result<(), String> {
    // Before asking for the restart, clear out of the way whoever the
    // supervisor does not know about. See `plan::Plan::clear`: without
    // this an orphan `oxdesk` holds the port, `/healthz` answers the old
    // build and we roll back an update that was correct.
    //
    // A failure here is ignored on purpose: `pkill` returns 1 when it
    // found nobody, which is the normal case.
    if let Some((prog, args)) = ctx.plan.clear.split_first() {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let _ = exec::run(prog, &args, 15);
    }
    let argv = &ctx.plan.restart;
    let (prog, args) = argv.split_first().ok_or_else(|| "no restart command".to_string())?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let s = exec::run(prog, &args, 60)?;
    if !s.ok {
        return Err(format!("{prog}: {}", first_line(&s.err)));
    }
    Ok(())
}

/// Is the live daemon the build we just installed?
///
/// Tries for ~25 s: `systemctl restart` returns before the process is
/// serving, and so does `launchctl kickstart`. Asking only once would
/// fail an update that was correct.
fn check_healthz(base: &Path, s: System, expected: &str) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{}/healthz", port(base, s));
    let mut last = String::from("did not answer");
    for _ in 0..13 {
        std::thread::sleep(std::time::Duration::from_millis(2000));
        match exec::api(&url, None, 4) {
            Ok(t) => match release::parse_healthz(&t) {
                Some((_, b)) if b == expected => return Ok(()),
                Some((n, b)) => last = format!("/healthz answers #{n} {b}, expected {expected}"),
                None => last = "/healthz answered something I cannot read".into(),
            },
            Err(e) => last = e,
        }
    }
    Err(format!("/healthz did not confirm {expected} in ~26 s: {last}"))
}

/// The daemon's port, read from this machine's config.
///
/// Hardcoding 8787 would make OTA think the daemon is dead on a machine
/// that only changed port -- and a "dead" daemon is exactly the case in
/// which it installs without checking anything. Reads the `port = N`
/// line instead of pulling in `toml` as a dependency for one number.
fn port(base: &Path, s: System) -> u16 {
    if let Some(p) = std::env::var("OXDESK_PORT").ok().and_then(|p| p.parse::<u16>().ok()) {
        return p;
    }
    // On Windows the config does NOT sit next to the binaries: `oxdesk`
    // reads it from `%APPDATA%\oxdesk` (Roaming), and the binaries live in
    // `%LOCALAPPDATA%\oxdesk`. Looking in the same place for both would
    // never find it -- and fall back to the hardcoded 8787, which is the
    // bug this function exists to avoid. Checked against the wizard's
    // output there: `%APPDATA%\oxdesk\config.toml`.
    let cfg = match s {
        System::Linux | System::Mac => base.join(".config").join("oxdesk").join("config.toml"),
        System::Windows => std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| base.to_path_buf())
            .join("oxdesk")
            .join("config.toml"),
    };
    fs::read_to_string(cfg)
        .ok()
        .and_then(|t| {
            t.lines()
                .map(str::trim)
                // Stop at the first table. The config has `port = 22`
                // inside each `[[hosts]]` (the SSH port), and a naive scan
                // would pick 22 on any machine that omits the daemon's
                // port -- making OTA conclude "daemon dead" and install
                // without checking anything.
                .take_while(|l| !l.starts_with('['))
                .find(|l| l.starts_with("port"))
                .and_then(|l| l.split('=').nth(1))
                .and_then(|v| v.trim().trim_matches('"').parse::<u16>().ok())
        })
        .unwrap_or(8787)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_stamp_inside_arbitrary_bytes() {
        let bin = b"\x7fELF\x02\x01...oxdesk 2f1409d em http://127.0.0.1\x00more junk";
        assert!(contains(bin, b"2f1409d"));
        assert!(!contains(bin, b"deadbee"));
        assert!(!contains(bin, b""), "an empty needle matches nothing");
        assert!(!contains(b"ab", b"abc"), "needle larger than the haystack");
    }

    /// The extracted folder is the package name without `.tar.gz` -- that
    /// is how `build-binaries.sh` packages (`tar -C dist <basename>`).
    #[test]
    fn the_package_folder_is_the_name_without_extension() {
        let name = "oxdesk-mac-arm64-unsigned-124-abc1234.tar.gz";
        assert_eq!(name.trim_end_matches(".tar.gz"), "oxdesk-mac-arm64-unsigned-124-abc1234");
    }

    #[test]
    fn copy_deletes_the_destination_first() {
        let d = std::env::temp_dir().join(format!("oxdesk-ota-cp-{}", state::now()));
        fs::create_dir_all(&d).unwrap();
        let a = d.join("a");
        let b = d.join("b");
        fs::write(&a, "novo").unwrap();
        fs::write(&b, "velho e mais longo").unwrap();
        copy(&a, &b).unwrap();
        assert_eq!(fs::read_to_string(&b).unwrap(), "novo");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&b).unwrap().permissions().mode() & 0o777, 0o755);
        }
        fs::remove_dir_all(&d).ok();
    }

    /// The port comes from the config, and a missing config falls back to
    /// 8787 -- never to zero or an error, because this runs with nobody
    /// watching.
    #[test]
    fn the_port_comes_from_the_config() {
        let d = std::env::temp_dir().join(format!("oxdesk-ota-porta-{}", state::now()));
        let cfg = d.join(".config").join("oxdesk");
        fs::create_dir_all(&cfg).unwrap();
        fs::write(cfg.join("config.toml"), "# comentario\nport = 9191\nfollow = true\n").unwrap();
        assert_eq!(port(&d, System::Linux), 9191);
        fs::write(cfg.join("config.toml"), "no port here\n").unwrap();
        assert_eq!(port(&d, System::Linux), 8787);
        // Each host's SSH `port = 22` must not become the daemon's port
        // when the daemon's is not written.
        fs::write(
            cfg.join("config.toml"),
            "follow = true\n\n[[hosts]]\nname = \"windows\"\nport = 22\n",
        )
        .unwrap();
        assert_eq!(port(&d, System::Linux), 8787);
        assert_eq!(port(Path::new("/does/not/exist"), System::Linux), 8787);
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn first_line_skips_empty_ones() {
        assert_eq!(first_line("\n\n  a real error\nanother"), "a real error");
        assert_eq!(first_line(""), "no message");
    }
}
