// SPDX-FileCopyrightText: 2026 oxlayer
// SPDX-License-Identifier: AGPL-3.0-or-later

//! oxdesk -- control plane for the two-machine LAN setup.
//!
//! Spec: ../specs/001-control-plane.md
//!
//! Listens ONLY on 127.0.0.1: reads ports, runs SSH and (soon) triggers
//! HID++ channel switches. It is a privileged surface, not a website.
//! See ../specs/008-security.md.

// No console on Windows: this is a daemon, and on Windows it starts from
// a scheduled task in the user's session -- a console app would open a
// black window at every login and every time the GUI started it.
//
// It does not affect the JSON over SSH: the subsystem decides whether a
// console is ALLOCATED, not where stdout goes. When the parent hands over
// a pipe (the SSH case), output still arrives. What is lost is running
// the binary by hand in a cmd and seeing text -- and for that there is
// the log at %LOCALAPPDATA%\oxdesk\oxdesk.log.
#![cfg_attr(windows, windows_subsystem = "windows")]

// First of all: macro_rules has TEXTUAL scope, so `say!` and `gripe!`
// only exist for the modules declared after this one.
#[macro_use]
mod say;

mod agent;
mod changelog;
#[cfg(target_os = "linux")]
mod border;
#[cfg(windows)]
mod border_win;
mod config;
mod disk;
mod disk_clean;
mod edge_state;
mod files;
mod guard;
mod focus;
mod follow;
mod hidpp;
mod hotkey;
#[cfg(windows)]
mod hotkey_win;
mod i18n;
mod clip;
mod clip_files;
mod kvm;
mod logs;
mod legacy;
mod pairing;
mod peers;
mod probe;
mod procs;
mod remote;
mod screen;
mod setup;
mod setup_common;
#[cfg(target_os = "linux")]
mod setup_linux;
#[cfg(target_os = "macos")]
mod setup_mac;
#[cfg(windows)]
mod setup_win;
mod sys;
mod telemetry;
#[cfg(target_os = "linux")]
mod tray;
mod update;
mod wait;

use config::Config;
use probe::Snapshot;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tiny_http::{Header, Request, Response, Server};

const PAGE: &str = include_str!("../static/index.html");
const SETUP: &str = include_str!("../static/setup.html");
/// The OxDesk mark (also the browser's favicon) and the mascot, both
/// embedded like the pages they appear in.
const FAVICON: &[u8] = include_bytes!("../static/favicon.png");
const MASCOT: &[u8] = include_bytes!("../static/mascot.webp");

/// Snapshot cache. It exists because /api/status does network and SSH --
/// measured at 2.09 s in the Python version, against 0.6 ms for /healthz.
/// Reusing it is what keeps the GUI smooth.
pub struct Cache {
    at: Option<Instant>,
    data: Option<Arc<Snapshot>>,
}

/// The cached snapshot, or `None` -- never a fresh one.
///
/// `cached()` refreshes when the TTL expired, and refreshing does network
/// and SSH to three machines. That is right for a panel request and wrong
/// for a keypress: the hotkey would hang for seconds on the gesture whose
/// whole point is to be instant. So this one PEEKS, and the caller decides
/// what to do with nothing.
pub fn snapshot_peek(cache: &Mutex<Cache>) -> Option<Arc<Snapshot>> {
    cache.lock().ok().and_then(|c| c.data.as_ref().map(Arc::clone))
}

fn json_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..]).unwrap()
}
fn html_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap()
}
fn js_header() -> Header {
    Header::from_bytes(&b"Content-Type"[..], &b"application/javascript; charset=utf-8"[..]).unwrap()
}
fn image_header(mime: &str) -> Header {
    Header::from_bytes(&b"Content-Type"[..], mime.as_bytes()).unwrap()
}
fn nostore() -> Header {
    Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap()
}

/// Responding must never take the thread down: a client that gives up
/// midway (a health check with a short timeout, a closed tab) became a
/// BrokenPipeError in the journal in the Python version. Here the error
/// is simply ignored.
fn respond(req: Request, resp: Response<std::io::Cursor<Vec<u8>>>) {
    let _ = req.respond(resp);
}

fn ok_json(req: Request, body: String) {
    respond(
        req,
        Response::from_string(body).with_header(json_header()).with_header(nostore()),
    );
}

fn err_json(req: Request, code: u16, msg: &str) {
    let body = serde_json::json!({ "error": msg }).to_string();
    respond(
        req,
        Response::from_string(body)
            .with_status_code(code)
            .with_header(json_header())
            .with_header(nostore()),
    );
}

fn cached(cfg: &Config, cache: &Mutex<Cache>) -> Arc<Snapshot> {
    let ttl = Duration::from_secs(cfg.refresh_seconds.saturating_sub(2).max(2));
    if let Ok(c) = cache.lock() {
        if let (Some(at), Some(data)) = (c.at, c.data.as_ref()) {
            if at.elapsed() < ttl {
                return Arc::clone(data);
            }
        }
    }
    let fresh = Arc::new(probe::snapshot(cfg));
    if let Ok(mut c) = cache.lock() {
        c.at = Some(Instant::now());
        c.data = Some(Arc::clone(&fresh));
    }
    fresh
}

/// Subcommands before serving.
///
/// `oxdesk` is also its own host's "agent": the peer invokes it over SSH
/// with `devices` / `host-info` / `set-host` and reads one line of JSON.
/// Without this, Windows could not probe Ubuntu the way Ubuntu already
/// probed Windows -- and, without that probe, `focus::plan` BLOCKS the
/// switch, because without someone on the other side able to give the
/// device back, the origin cannot undo (specs/005).
///
/// The format comes from `hidpp::protocol`, shared with `oxdesk-agent`: it
/// is the contract between the machines, and two copies would diverge.
/// Translates the subcommands and flags this binary answered to before
/// they were renamed to English.
///
/// The old names have to keep working because they are not only typed by
/// hand: a peer invokes them **on this machine over SSH** (`kvm-soltar`,
/// `clip-arq-*`), and a peer can be a build older than this one -- the Mac
/// cannot be deployed while it has no screen. Translating on the way in
/// costs one table and keeps the parsing below to a single vocabulary.
pub fn canonical(verb: &str) -> &str {
    match verb {
        "kvm-soltar" => "kvm-release",
        "clip-sonda" => "clip-probe",
        "clip-arq-set" => "clip-file-set",
        "clip-arq-dir" => "clip-file-dir",
        "clip-arq-prontos" => "clip-file-ready",
        // Every spelling of the same question collapses to one token, so
        // the match below needs one arm. Keeping `--version` as itself is
        // what broke it: the arm said `version`, nothing matched, and
        // `oxdesk --version` answered "unknown command" -- which
        // `install.sh` had been hiding behind a `|| true` since before
        // the rename, because the old binary did not accept it either.
        "versao" | "--versao" | "--version" => "version",
        other => other,
    }
}

fn run_cli(args: &[String]) -> Option<std::process::ExitCode> {
    use std::process::ExitCode;
    match args.first().map(String::as_str).map(canonical) {
        None => None,
        Some("serve") => None,
        // Receives the clipboard from another machine, over SSH. Hands it
        // to the local daemon -- see `clip::cli_set` for why it does not
        // write directly.
        Some("kvm-release") => {
            let port = Config::load().map(|c| c.port).unwrap_or(8787);
            Some(crate::kvm::cli_release(port))
        }
        Some("clip-set") => {
            let port = Config::load().map(|c| c.port).unwrap_or(8787);
            Some(crate::clip::cli_set(port))
        }
        // Another machine's panel asking what can be reclaimed here, or
        // asking to clean it (specs/027). JSON on stdout.
        Some("disk-list") | Some("disk-clean") => Some(crate::disk_clean::cli(args)),
        #[cfg(target_os = "linux")]
        Some("clip-probe") => Some(crate::clip_files::cli_probe()),
        // Receives files from another machine. `clip-arq-set` reads a tar
        // from stdin; the other two serve the rsync path, which copies
        // first and notifies afterwards.
        Some("clip-file-set") => {
            let port = Config::load().map(|c| c.port).unwrap_or(8787);
            Some(crate::clip_files::cli_set(port))
        }
        Some("clip-file-dir") => Some(crate::clip_files::cli_dir()),
        Some("clip-file-ready") => {
            let port = Config::load().map(|c| c.port).unwrap_or(8787);
            match args.get(1) {
                Some(d) => Some(crate::clip_files::cli_ready(port, d)),
                None => {
                    eprintln!("clip-file-ready: give the folder");
                    Some(ExitCode::FAILURE)
                }
            }
        }
        // Same SHAPE as `/healthz` on purpose. Whoever asks "is the live
        // daemon the binary on disk?" compares two answers with a single
        // parser, instead of inventing a second way of saying the same
        // thing. See `setup_common::daemon_on_disk`.
        Some("version") => {
            println!(
                "{{\"build\":\"{}\",\"n\":{}}}",
                env!("OXDESK_BUILD"),
                env!("OXDESK_BUILD_N").parse::<u64>().unwrap_or(0)
            );
            Some(ExitCode::SUCCESS)
        }
        Some("-h") | Some("--help") | Some("help") => {
            // stderr: stdout is reserved for the protocol JSON.
            eprintln!(
                "oxdesk [command]

  (no command)   starts the daemon and serves the API on 127.0.0.1
  serve          same, explicit
  version        this binary's build stamp, in the /healthz shape

  kvm-release    gives this machine's input back to itself
                 (used by a peer that wants the input back)
  clip-set       reads stdin and puts it on this machine's clipboard
                 (used by the peers over SSH)

  devices        inventory of this receiver's slots (JSON)
  host-info <slot>
                 number of hosts and current channel (JSON)
  set-host <slot> <host> --confirm
                 switches the channel. 1-based. IRREVERSIBLE: the device
                 leaves this receiver and no longer answers here.

The last three exist so the PEER can probe us over SSH; the output is
one line of JSON. See specs/002-ssh.md."
            );
            Some(ExitCode::SUCCESS)
        }
        Some(_) => {
            // `::hidpp` and not `hidpp`: this crate has a local module with
            // the same name (the adapter in hidpp.rs), which shadows the
            // shared crate here at the root.
            let api = match ::hidpp::hidapi::HidApi::new() {
                Ok(a) => a,
                Err(e) => {
                    println!("{}", ::hidpp::protocol::fail(format!("hidapi: {e}")));
                    return Some(ExitCode::FAILURE);
                }
            };
            match ::hidpp::protocol::run(&api, args) {
                ::hidpp::protocol::Handled::Json(v) => {
                    let ok = v.get("ok").and_then(|b| b.as_bool()) == Some(true);
                    println!("{v}");
                    Some(if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE })
                }
                ::hidpp::protocol::Handled::Unknown => {
                    println!(
                        "{}",
                        ::hidpp::protocol::fail(format!(
                            "unknown command: {:?} -- use --help",
                            args[0]
                        ))
                    );
                    Some(ExitCode::FAILURE)
                }
            }
        }
    }
}

fn main() -> std::process::ExitCode {
    // Before anything else: if a command came, answer and exit. Do not
    // load the config, do not open the port -- the peer only wants one
    // line of JSON.
    // Before anything reads the config: an installation from before the
    // rename keeps its config, identity and pairing (legacy.rs).
    for line in legacy::migrate() {
        say!("legacy: {line}");
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = run_cli(&args) {
        return code;
    }

    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            // Fail loudly: an invalid config does not become a silent
            // default.
            gripe!("oxdesk: {e}");
            std::process::exit(1);
        }
    };

    let addr = format!("127.0.0.1:{}", cfg.port);
    let _ = API_PORT.set(cfg.port);
    let server = match Server::http(&addr) {
        Ok(s) => s,
        Err(e) => {
            gripe!("oxdesk: could not listen on {addr}: {e}");
            std::process::exit(1);
        }
    };
    say!("oxdesk on http://{addr}  (config: {})", Config::path().display());

    let cfg = Arc::new(RwLock::new(cfg));
    let cache = Arc::new(Mutex::new(Cache { at: None, data: None }));

    // Border watch on its own thread; a failure does not take the daemon
    // down.
    sys::spawn_border(Arc::clone(&cfg));
    // "Follow the leader" reconciler, on its own thread.
    follow::spawn(Arc::clone(&cfg));
    // Keeps the peers' state fresh, so the hot path does not wait on SSH.
    peers::spawn(Arc::clone(&cfg));
    // Keeps the local survey warm: the border cannot pay ~360 ms.
    hidpp::spawn_refresher();
    // A KVM that survived the restart is still valid: adopt it, do not
    // kill it. Killing it yanked the input away from whoever was using
    // the other machine every time a deploy restarted the daemon.
    if let Ok(g) = cfg.read() {
        kvm::adopt(&g);
    }

    // Shared clipboard. On Linux a thread with its own D-Bus session
    // watches and publishes; on the others, polling.
    #[cfg(target_os = "linux")]
    {
        let channel = clip::linux::spawn(Arc::clone(&cfg));
        clip::store_channel(channel);
    }
    #[cfg(not(target_os = "linux"))]
    clip::watch(Arc::clone(&cfg));
    // Native tray. Not fatal if it does not come up: without the
    // AppIndicator extension (or without any tray) the icon does not
    // appear and the daemon keeps serving the panel and the API. Taking
    // oxdesk down because there is nowhere to draw an icon would trade an
    // inconvenience for an outage.
    //
    // It does keep asking, though. Giving up at t=0 is what left this
    // machine with no icon for half an hour -- see `wait::retry_delay`.
    #[cfg(target_os = "linux")]
    tray::start_when_drawable(Arc::clone(&cfg));

    // The shortcut (both Ctrl keys by default), with the input here, sends
    // it to the other machine. The other half of the gesture -- bringing it
    // back -- lives in `oxdesk-kvm`, and the two cannot both see a press:
    // whoever holds the grab gets the events alone. See `hotkey`. The Mac
    // cannot be the source, so it has no reader.
    #[cfg(target_os = "linux")]
    hotkey::start(Arc::clone(&cfg), Arc::clone(&cache));
    #[cfg(windows)]
    hotkey_win::start(Arc::clone(&cfg), Arc::clone(&cache));

    // Spec 017: pair with the peers once at startup, so phase C can refuse
    // strangers without refusing our own machines.
    if let Ok(c) = cfg.read() {
        pairing::pair_peers(c.hosts.clone(), screen::local_host_bin());
    }

    let server = Arc::new(server);
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let server = Arc::clone(&server);
            let cfg = Arc::clone(&cfg);
            let cache = Arc::clone(&cache);
            std::thread::spawn(move || {
                for req in server.incoming_requests() {
                    handle(req, &cfg, &cache);
                }
            })
        })
        .collect();

    for w in workers {
        let _ = w.join();
    }
    // We only get here if all threads died: the server went down.
    std::process::ExitCode::FAILURE
}

fn invalidate(cache: &Mutex<Cache>) {
    if let Ok(mut c) = cache.lock() {
        c.at = None;
        c.data = None;
    }
}

/// PATCH /api/config -- shallow top-level merge, validate, save,
/// invalidate the cache.
///
/// Shallow merge on purpose: replacing `hosts` whole is explicit and
/// predictable. A deep merge into a list of hosts would be guesswork.
fn patch_config(mut req: Request, cfg: &RwLock<Config>, cache: &Mutex<Cache>) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let incoming: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return err_json(req, 400, &format!("invalid JSON: {e}")),
    };
    let serde_json::Value::Object(patch) = incoming else {
        return err_json(req, 400, "esperado um objeto JSON");
    };

    let merged = {
        let guard = match cfg.read() {
            Ok(g) => g,
            Err(_) => return err_json(req, 500, "corrupt config state"),
        };
        let mut base = match serde_json::to_value(&*guard) {
            Ok(serde_json::Value::Object(m)) => m,
            _ => return err_json(req, 500, "the current config does not serialize"),
        };
        for (k, v) in patch {
            if !base.contains_key(&k) {
                return err_json(req, 400, &format!("unknown field: {k}"));
            }
            base.insert(k, v);
        }
        serde_json::Value::Object(base)
    };

    let next: Config = match serde_json::from_value(merged) {
        Ok(c) => c,
        Err(e) => return err_json(req, 422, &format!("the resulting config is invalid: {e}")),
    };
    if next.port == 0 {
        return err_json(req, 422, "invalid port");
    }
    if next.hosts.is_empty() {
        return err_json(req, 422, "needs at least one host");
    }
    // Save before applying: if the disk refuses, nothing changes in
    // memory.
    if let Err(e) = next.save() {
        return err_json(req, 500, &e);
    }

    let body = match serde_json::to_string(&next) {
        Ok(b) => b,
        Err(e) => return err_json(req, 500, &format!("serialize: {e}")),
    };
    match cfg.write() {
        Ok(mut g) => *g = next,
        Err(_) => return err_json(req, 500, "could not apply it"),
    }
    invalidate(cache);
    ok_json(req, body);
}

/// POST /api/focus -- hands input focus to another machine.
///
/// **Dry-run by default.** Without `"confirm": true` it returns only the
/// plan, because the physical route is irreversible on the origin side:
/// after ChangeHost this receiver no longer reaches the device. See
/// ../specs/005-input-switch.md.
fn post_focus(mut req: Request, cfg: &RwLock<Config>) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let fr: focus::FocusRequest = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return err_json(req, 400, &format!("invalid JSON: {e}")),
    };
    let snap = match cfg.read() {
        Ok(g) => g.clone(),
        Err(_) => return err_json(req, 500, "corrupt config state"),
    };
    // LAN mode: capture here and send over the network. It does not go
    // through the HID++ planner because there is nothing to plan -- no
    // device switches channel, no receiver needs to exist on the other
    // side, and there is no possible partial state (either the child
    // started with both devices, or it did not).
    //
    // And it **does not require `confirm`**. The dry-run exists because
    // ChangeHost is irreversible from the origin: after it this receiver
    // no longer reaches the device. LAN is undone with the two Ctrls or
    // by sending to the local machine, so asking for confirmation would
    // be ceremony with no risk behind it.
    if snap.input.mode == "lan" {
        let response = match kvm::send(&snap, &fr.host) {
            Ok(dest) => serde_json::json!({
                "plan": { "host": fr.host, "route": "lan" },
                "executed": true,
                "results": [format!("input going to {} ({dest})", fr.host)],
            }),
            Err(e) => serde_json::json!({
                "plan": { "host": fr.host, "route": "lan" },
                "executed": false,
                "results": [],
                "error": e,
            }),
        };
        let code = if response.get("error").is_some() { 409 } else { 200 };
        return respond(
            req,
            Response::from_string(response.to_string())
                .with_status_code(code)
                .with_header(json_header())
                .with_header(nostore()),
        );
    }

    let outcome = focus::execute(&snap, &fr);
    let code = if outcome.error.is_some() { 409 } else { 200 };
    match serde_json::to_string(&outcome) {
        Ok(b) => respond(
            req,
            Response::from_string(b)
                .with_status_code(code)
                .with_header(json_header())
                .with_header(nostore()),
        ),
        Err(e) => err_json(req, 500, &format!("serialize: {e}")),
    }
}

/// POST /api/controls/remap -- remaps a button.
///
/// Requires `confirm`: writing to the device changes physical behavior,
/// and a wrong remap leaves the button useless until it is undone.
fn post_remap(mut req: Request) {
    #[derive(serde::Deserialize)]
    struct Body {
        slot: u8,
        /// "0x0053" or decimal.
        cid: String,
        /// Target; "0x0" or "0" restores the default.
        to: String,
        #[serde(default)]
        confirm: bool,
    }
    fn parse_id(s: &str) -> Option<u16> {
        let t = s.trim();
        if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            u16::from_str_radix(h, 16).ok()
        } else {
            t.parse().ok()
        }
    }

    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let b: Body = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return err_json(req, 400, &format!("invalid JSON: {e}")),
    };
    if !b.confirm {
        return err_json(req, 400, "falta confirm: isto muda o comportamento fisico do botao");
    }
    let (Some(cid), Some(to)) = (parse_id(&b.cid), parse_id(&b.to)) else {
        return err_json(req, 400, "invalid cid/to (use 0x0053 or decimal)");
    };
    match hidpp::remap(b.slot, cid, to) {
        Ok(detail) => ok_json(req, serde_json::json!({ "ok": true, "detail": detail }).to_string()),
        Err(e) => err_json(req, 409, &e),
    }
}

/// POST /api/clipboard -- raw text in the body, goes to the local
/// clipboard.
///
/// No JSON on purpose: the body IS the text. Wrapping the clipboard in
/// JSON would force escaping what the person copied, and the clipboard
/// carries exactly the kind of thing that breaks escaping -- quotes,
/// slashes, line breaks, odd bytes.
fn post_clipboard(mut req: Request) {
    let mut text = String::new();
    if req.as_reader().read_to_string(&mut text).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    if text.len() > clip::LIMIT {
        return err_json(req, 413, "text over the limit");
    }
    match clip::apply_local(&text) {
        Ok(()) => ok_json(req, "{\"ok\":true}".into()),
        Err(e) => err_json(req, 500, &e),
    }
}

/// POST /api/clipboard-files {"paths":[...]} -- received files.
///
/// JSON here, unlike the text clipboard, because the body is a LIST:
/// separating paths by line break would fail on the first file with a
/// line break in its name, which is legal on Linux.
fn post_clipboard_files(mut req: Request) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let paths: Vec<String> = v
        .get("paths")
        .and_then(|p| p.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if paths.is_empty() {
        return err_json(req, 400, "send {\"paths\": [...]}");
    }
    match clip_files::apply_local(&paths) {
        Ok(()) => ok_json(req, "{\"ok\":true}".into()),
        Err(e) => err_json(req, 500, &e),
    }
}

/// POST /api/clipboard-files/send {"host":"<name>"} -- sends the
/// pending large selection to ONE destination.
///
/// It exists because above the cap we do not send on our own: 1.4 GB to
/// two machines nobody is looking at wastes someone else's bandwidth and
/// disk. The tray calls this with the chosen host.
fn post_clipboard_send(mut req: Request, cfg: &RwLock<Config>) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let Some(host) = v.get("host").and_then(|h| h.as_str()).map(String::from) else {
        return err_json(req, 400, "send {\"host\": \"<name>\"}");
    };
    let snap = match cfg.read() {
        Ok(g) => g.clone(),
        Err(_) => return err_json(req, 500, "corrupt config state"),
    };
    match clip_files::send_offer(&snap, &host) {
        Ok(d) => ok_json(req, serde_json::json!({ "ok": true, "detail": d }).to_string()),
        Err(e) => err_json(req, 409, &e),
    }
}

/// POST /api/screen {"host": "<name>"} -- launches the screen client.
///
/// Separate from the GET on purpose: the GET answers "can it be clicked?"
/// and the UI disables the button with the reason written beside it. A
/// button that opens nothing is worse than a disabled, explained button.
fn post_transfer(mut req: Request, cfg: &RwLock<Config>) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    let snap = match cfg.read() {
        Ok(g) => g.clone(),
        Err(_) => return err_json(req, 500, "config lock poisoned"),
    };
    match files::transfer(&snap, &field("from"), &field("path"), &field("to"), &field("folder")) {
        Ok(id) => ok_json(req, serde_json::json!({ "id": id }).to_string()),
        Err(e) => err_json(req, 409, &e),
    }
}

fn post_session(mut req: Request, cfg: &RwLock<Config>) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let Some(host) = v.get("host").and_then(|h| h.as_str()).map(String::from) else {
        return err_json(req, 400, "send {\"host\": \"<name>\"}");
    };
    let snap = match cfg.read() {
        Ok(g) => g.clone(),
        Err(_) => return err_json(req, 500, "config lock poisoned"),
    };
    match screen::open_session(&snap, &host) {
        Ok(s) => match serde_json::to_string(&s) {
            Ok(b) => ok_json(req, b),
            Err(e) => err_json(req, 500, &format!("serialize: {e}")),
        },
        Err(e) => err_json(req, 409, &e),
    }
}

fn post_screen(mut req: Request, cfg: &RwLock<Config>) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let Some(host) = v.get("host").and_then(|h| h.as_str()).map(String::from) else {
        return err_json(req, 400, "send {\"host\": \"<name>\"}");
    };
    // Omitting an option means the default: control yes, sound and
    // microphone no. Turning audio on without asking is the kind of
    // surprise that scares people in a meeting.
    let d = screen::Opts::default();
    let flag = |k: &str, fallback: bool| v.get(k).and_then(|x| x.as_bool()).unwrap_or(fallback);
    let opts = screen::Opts {
        control: flag("control", d.control),
        audio: flag("audio", d.audio),
        mic: flag("mic", d.mic),
    };
    let snap = match cfg.read() {
        Ok(g) => g.clone(),
        Err(_) => return err_json(req, 500, "config lock poisoned"),
    };
    match screen::launch(&snap, &host, opts) {
        Ok(line) => {
            ok_json(req, serde_json::json!({ "ok": true, "command": line }).to_string())
        }
        Err(e) => err_json(req, 409, &e),
    }
}

/// POST /api/hidpp/pause {"minutes": N}  (0 resumes)
///
/// Pauses the periodic POLLING of the devices, not channel switching. It
/// serves to tell "our reading disturbs the radio" apart from "the
/// hardware does this on its own" without taking anyone's mouse away to
/// test: with reading paused, if the switch holds, the fault is ours.
fn post_hidpp_pause(mut req: Request) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let min = v.get("minutes").and_then(|x| x.as_u64()).unwrap_or(10).min(60);
    let remaining = hidpp::pause(min);
    match remaining {
        Some(s) => say!("hidpp: polling PAUSED for {}s (diagnostics)", s),
        None => say!("hidpp: polling resumed"),
    }
    ok_json(
        req,
        serde_json::json!({ "ok": true, "paused_s": remaining,
            "note": if remaining.is_some() {
                "polling paused; switching channel still works"
            } else {
                "polling resumed"
            } })
        .to_string(),
    )
}

/// POST /api/follow {"enabled": bool, "leader": "<HID++ name>"}
///
/// Which device leads depends on HOW the person switches machines: over
/// the border it is the mouse that crosses; with the keyboard's
/// Easy-Switch button, the leader has to be the keyboard -- otherwise
/// the reconciler undoes the action they just took. That is why it is a
/// choice, not a constant.
fn post_follow(mut req: Request, cfg: &RwLock<Config>, cache: &Mutex<Cache>) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let current = cfg.read().map(|g| g.input.follow.enabled).unwrap_or(false);
    let on = v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(current);
    let leader = v.get("leader").and_then(|x| x.as_str());
    if let Err(e) = config::save_follow(on, leader) {
        return err_json(req, 500, &e);
    }
    match Config::load() {
        Ok(reloaded) => {
            let leader_name = reloaded.input.follow.leader.clone().unwrap_or_default();
            if let Ok(mut g) = cfg.write() {
                *g = reloaded;
            }
            if let Ok(mut c) = cache.lock() {
                c.at = None;
            }
            say!("follow: {} with leader '{leader_name}'", if on { "ON" } else { "off" });
            ok_json(
                req,
                serde_json::json!({ "ok": true, "enabled": on, "leader": leader_name,
                    "note": if on { "on now; the loop reads the state on every pass" } else { "off now -- the next pass does nothing" } })
                .to_string(),
            )
        }
        Err(e) => err_json(req, 500, &format!("written but not reloaded: {e}")),
    }
}

/// POST /api/hotkey {"enabled": bool, "keys"?: "both-ctrl"|"both-shift"|"both-alt",
/// "mode"?: "rotate"|"ask"} -- the keyboard shortcut (spec 023).
///
/// The readers take a new pair on their next key, without a restart, and
/// the next KVM started gets it as its rescue.
fn post_hotkey(mut req: Request, cfg: &RwLock<Config>) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let current = cfg.read().map(|g| g.input.hotkey.enabled).unwrap_or(true);
    let on = v.get("enabled").and_then(|x| x.as_bool()).unwrap_or(current);
    let keys = v.get("keys").and_then(|x| x.as_str());
    let mode = v.get("mode").and_then(|x| x.as_str());
    if let Err(e) = config::save_hotkey(on, keys, mode) {
        return err_json(req, 400, &e);
    }
    match Config::load() {
        Ok(reloaded) => {
            let h = reloaded.input.hotkey.clone();
            // Read back from the file, not echoed from the request: the
            // response says what the daemon will actually use.
            if keys.is_some_and(|k| k != h.keys) || mode.is_some_and(|m| m != h.mode) || h.enabled != on {
                return err_json(req, 500, "written, but the file does not say what was asked");
            }
            if let Ok(mut g) = cfg.write() {
                *g = reloaded;
            }
            say!("hotkey: {} with keys {} ({})", if on { "ON" } else { "off" }, h.keys, h.mode);
            ok_json(
                req,
                serde_json::json!({ "ok": true, "enabled": h.enabled, "keys": h.keys, "mode": h.mode })
                    .to_string(),
            )
        }
        Err(e) => err_json(req, 500, &format!("written but not reloaded: {e}")),
    }
}

/// POST /api/border {"enabled": bool} -- turns the border KVM on/off.
fn post_border(mut req: Request, cfg: &RwLock<Config>, cache: &Mutex<Cache>) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let Some(on) = v.get("enabled").and_then(|x| x.as_bool()) else {
        return err_json(req, 400, "send {\"enabled\": true|false}");
    };
    if let Err(e) = config::save_border_enabled(on) {
        return err_json(req, 500, &e);
    }
    match Config::load() {
        Ok(reloaded) => {
            if let Ok(mut g) = cfg.write() {
                *g = reloaded;
            }
            if let Ok(mut c) = cache.lock() {
                c.at = None;
            }
            say!("border: border KVM {} from the GUI", if on { "ON" } else { "OFF" });
            ok_json(
                req,
                serde_json::json!({ "ok": true, "enabled": on,
                    "note": if on {
                        "on now; the edge watcher picks it up within a second"
                    } else {
                        "off now -- the next touch of the edge will not switch"
                    } })
                .to_string(),
            )
        }
        Err(e) => err_json(req, 500, &format!("written but not reloaded: {e}")),
    }
}

/// POST /api/layout {"positions": {"<host>": {"col": N, "row": M}}}
///
/// Saves by surgical TOML editing, not by reserializing: the config's
/// comments are the record of why each choice was made, and that is not
/// lost to save two coordinates.
fn post_layout(mut req: Request, cfg: &RwLock<Config>, cache: &Mutex<Cache>) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let Some(obj) = v.get("positions").and_then(|p| p.as_object()) else {
        return err_json(req, 400, "send {\"positions\": {\"<host>\": {\"col\":N,\"row\":M}}}");
    };
    let mut pos = std::collections::BTreeMap::new();
    for (name, c) in obj {
        let (Some(col), Some(row)) = (
            c.get("col").and_then(|x| x.as_i64()),
            c.get("row").and_then(|x| x.as_i64()),
        ) else {
            return err_json(req, 400, &format!("invalid position for '{name}'"));
        };
        pos.insert(name.clone(), config::Cell { col: col as i32, row: row as i32 });
    }
    if let Err(e) = config::save_layout(&pos) {
        return err_json(req, 500, &e);
    }
    // Reload from disk: the file is what counts, and this way the
    // response reflects what was saved, not what we asked for.
    match Config::load() {
        Ok(reloaded) => {
            let edges: Vec<_> = reloaded
                .border_edges_full()
                .into_iter()
                .map(|(e, t, ok)| serde_json::json!({ "edge": e, "target": t, "active": ok }))
                .collect();
            if let Ok(mut g) = cfg.write() {
                *g = reloaded;
            }
            if let Ok(mut c) = cache.lock() {
                c.at = None;
            }
            say!("layout: grid saved; edges now {}", edges.len());
            ok_json(
                req,
                serde_json::json!({ "ok": true, "edges": edges,
                    "note": "layout saved; an edge watcher already running keeps its edges until the daemon restarts" })
                .to_string(),
            )
        }
        Err(e) => err_json(req, 500, &format!("written but not reloaded: {e}")),
    }
}

/// POST /api/processes/kill {"host","pid","force"?,"confirm":true}
///
/// `confirm` is mandatory for the same reason as `/api/focus`: killing a
/// process on someone's machine cannot come from a distracted click, and
/// it has no undo.
fn post_kill(mut req: Request, cfg: &RwLock<Config>) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let (Some(host), Some(pid)) = (
        v.get("host").and_then(|h| h.as_str()).map(String::from),
        v.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32),
    ) else {
        return err_json(req, 400, "send {\"host\": \"<name>\", \"pid\": <n>}");
    };
    let force = v.get("force").and_then(|f| f.as_bool()).unwrap_or(false);
    let confirm = v.get("confirm").and_then(|c| c.as_bool()).unwrap_or(false);
    let snap = match cfg.read() {
        Ok(g) => g.clone(),
        Err(_) => return err_json(req, 500, "config lock poisoned"),
    };
    match procs::kill(&snap, &host, pid, force, confirm) {
        Ok(detail) => {
            ok_json(req, serde_json::json!({ "ok": true, "detail": detail }).to_string())
        }
        Err(e) => err_json(req, 409, &e),
    }
}

/// GET /api/setup -- what this machine still lacks.
///
/// Measures on demand, and does NOT join the panel's refresh cycle: the
/// `peers` step opens SSH to each host, and a panel repeating that every
/// 10 s would be a storm of connections to answer a question that changes
/// once a month. The page shows the measurement time and a button.
fn get_setup(req: Request, cfg: &RwLock<Config>) {
    let snap = match cfg.read() {
        Ok(g) => g.clone(),
        Err(_) => return err_json(req, 500, "config lock poisoned"),
    };
    let steps = setup::local_catalog();
    let checks = setup::measure(&snap);
    let summary = setup::summarize(&steps, &checks);

    // Joins catalog and check in a single object: the page should not
    // have to match two lists by id to draw one row.
    let rows: Vec<serde_json::Value> = steps
        .iter()
        .map(|p| {
            let m = checks.iter().find(|m| m.id == p.id);
            serde_json::json!({
                "id": p.id,
                "title": p.title,
                "why": p.why,
                "fix": p.fix,
                "essential": p.essential,
                "depends_on": p.depends_on,
                "state": m.map(|m| m.state).unwrap_or(setup::State::Error),
                "detail": m.map(|m| m.detail.clone()).unwrap_or_else(|| "not measured".into()),
                "waiting_on": setup::blocked_by(p, &checks),
            })
        })
        .collect();

    let me = snap.hosts.iter().find(|h| h.is_local());
    let body = serde_json::json!({
        "system": setup::System::this_machine(),
        "machine": me.map(|h| h.name.clone()),
        "role": me.map(|h| h.role.clone()),
        "build": env!("OXDESK_BUILD"),
        "measured": sys::hhmmss(),
        "steps": rows,
        "summary": summary,
    });
    match serde_json::to_string(&body) {
        Ok(b) => ok_json(req, b),
        Err(e) => err_json(req, 500, &format!("serialize: {e}")),
    }
}

/// POST /api/setup/fix {"id":"<step>"}
///
/// Returns 409, not 500, when the repair does not solve it: it is not a
/// server defect, it is the normal "I opened the window, now it is up to
/// you" case. The page shows the message as an instruction, not as an
/// error.
/// A disk listing (`ids` is `None`) or cleaning on `host`: in this process
/// for this machine, and for another one over the same path as the
/// clipboard (`oxdesk disk-list` / `disk-clean`, which print JSON).
fn disk_answer(req: Request, cfg: &RwLock<Config>, host: Option<String>, ids: Option<Vec<String>>) {
    let snap = match cfg.read() {
        Ok(g) => g.clone(),
        Err(_) => return err_json(req, 500, "config lock poisoned"),
    };
    let h = match host.as_deref() {
        None => snap.hosts.iter().find(|h| h.is_local()),
        Some(n) => snap.hosts.iter().find(|h| h.name == n),
    };
    let Some(h) = h else {
        return err_json(req, 404, "that machine is not in the config");
    };
    if h.is_local() {
        let body = match &ids {
            None => serde_json::to_string(&disk_clean::list()),
            Some(i) => serde_json::to_string(&disk_clean::clean(i)),
        };
        return match body {
            Ok(b) => ok_json(req, b),
            Err(e) => err_json(req, 500, &format!("serialize: {e}")),
        };
    }
    let sub = match &ids {
        None => "disk-list".to_string(),
        Some(i) => format!("disk-clean {}", i.join(",")),
    };
    // What a real answer has: a listing has `places`, a cleanup a `verdict`.
    // An OxDesk from before this feature also answers JSON, an error object
    // ("unknown command"), so being JSON proves nothing.
    let must = if ids.is_none() { "places" } else { "verdict" };
    match remote::run(h, &remote::oxdesk_command(h, &sub), 8) {
        // The last line that is a JSON object: the shell around it may add
        // warnings before it.
        Ok(out) => {
            let v = out
                .lines()
                .rev()
                .find(|l| l.trim_start().starts_with('{'))
                .and_then(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok());
            match v {
                Some(v) if v.get(must).is_some() => ok_json(req, v.to_string()),
                Some(v) if v.get("error").is_some() => err_json(
                    req,
                    502,
                    &format!(
                        "{}'s OxDesk does not have this yet (it answered: {}); update it first",
                        h.name,
                        v["error"].as_str().unwrap_or("an error")
                    ),
                ),
                _ => err_json(req, 502, &format!("{} sent no answer this panel can read", h.name)),
            }
        }
        Err(e) => err_json(req, 502, &format!("{}: {e}", h.name)),
    }
}

fn post_setup_repair(mut req: Request, cfg: &RwLock<Config>) {
    let mut body = String::new();
    if req.as_reader().read_to_string(&mut body).is_err() {
        return err_json(req, 400, "unreadable body");
    }
    let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    let Some(id) = v.get("id").and_then(|i| i.as_str()) else {
        return err_json(req, 400, "send {\"id\": \"<step>\"}");
    };
    // Only ids from THIS machine's catalog. Without this, any id would
    // fall straight into the platform module's `match` -- which refuses
    // today, but nothing would guarantee that for the next step added.
    if !setup::local_catalog().iter().any(|p| p.id == id) {
        return err_json(req, 404, &format!("'{id}' is not a step on this machine"));
    }
    let snap = match cfg.read() {
        Ok(g) => g.clone(),
        Err(_) => return err_json(req, 500, "config lock poisoned"),
    };
    say!("setup: repairing '{id}'");
    match setup::repair(id, &snap) {
        Ok(detail) => {
            say!("setup: '{id}' ok -- {detail}");
            ok_json(req, serde_json::json!({"ok": true, "detail": detail}).to_string())
        }
        Err(e) => {
            say!("setup: '{id}' not solved -- {e}");
            err_json(req, 409, &e)
        }
    }
}

/// A request header's value, case-insensitively by name.
fn header(req: &Request, name: &'static str) -> Option<String> {
    req.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str().to_string())
}

/// The port this daemon serves on, for `guard::check`: our own panel's
/// origin is `http://127.0.0.1:<this port>`.
static API_PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();

fn handle(req: Request, cfg: &RwLock<Config>, cache: &Mutex<Cache>) {
    let path = req.url().split('?').next().unwrap_or("").to_string();
    let method = req.method().as_str().to_string();

    // Before any route: no web page gets to drive this API, and none gets
    // to read it through a rebound name (see `guard`).
    let port = API_PORT.get().copied().unwrap_or(8787);
    let (host, origin) = (header(&req, "Host"), header(&req, "Origin"));
    if let Err(why) = guard::check(&method, host.as_deref(), origin.as_deref(), port) {
        say!("api: refused {method} {path} -- {why}");
        return err_json(req, 403, why);
    }

    if path == "/api/config" && method == "PATCH" {
        return patch_config(req, cfg, cache);
    }
    if path == "/api/kvm/release" && method == "POST" {
        return match kvm::release() {
            Ok(()) => ok_json(req, "{\"ok\":true}".into()),
            Err(e) => err_json(req, 500, &e),
        };
    }
    // Free up space (specs/027): what can be reclaimed on a machine, and
    // cleaning the places the person ticked. Listing reads only.
    if path == "/api/disk/list" && method == "GET" {
        let q = req.url().split_once('?').map(|(_, q)| q.to_string()).unwrap_or_default();
        let host = q
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == "host")
            .map(|(_, v)| files::percent_decode(v));
        return disk_answer(req, cfg, host, None);
    }
    if path == "/api/disk/clean" && method == "POST" {
        // `as_reader` is a `dyn Read`: its methods need no import.
        let mut req = req;
        let mut body = String::new();
        if req.as_reader().read_to_string(&mut body).is_err() {
            return err_json(req, 400, "could not read the request");
        }
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
        let host = v.get("host").and_then(|h| h.as_str()).map(String::from);
        let ids: Vec<String> = v
            .get("ids")
            .and_then(|i| i.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        // Ids only, never paths, and only the list's own: anything else is
        // refused here, before it reaches a shell on another machine.
        if ids.is_empty() || ids.iter().any(|i| !i.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')) {
            return err_json(req, 400, "give the ids to clean, as listed");
        }
        return disk_answer(req, cfg, host, Some(ids));
    }
    // "Check for updates" in the trays: asks the updater, installs nothing.
    if path == "/api/update/check" && method == "POST" {
        return match serde_json::to_string(&update::check()) {
            Ok(b) => ok_json(req, b),
            Err(e) => err_json(req, 500, &format!("serialize: {e}")),
        };
    }
    if path == "/api/setup/fix" && method == "POST" {
        return post_setup_repair(req, cfg);
    }
    if path == "/api/setup" && method == "GET" {
        return get_setup(req, cfg);
    }
    if path == "/api/clipboard" && method == "POST" {
        return post_clipboard(req);
    }
    if path == "/api/clipboard-files" && method == "POST" {
        return post_clipboard_files(req);
    }
    if path == "/api/clipboard-files/send" && method == "POST" {
        return post_clipboard_send(req, cfg);
    }
    if path == "/api/screen" && method == "POST" {
        return post_screen(req, cfg);
    }
    if path == "/api/session" && method == "POST" {
        return post_session(req, cfg);
    }
    if path == "/api/files/transfer" && method == "POST" {
        return post_transfer(req, cfg);
    }
    if path == "/api/processes/kill" && method == "POST" {
        return post_kill(req, cfg);
    }
    if path == "/api/layout" && method == "POST" {
        return post_layout(req, cfg, cache);
    }
    if path == "/api/border" && method == "POST" {
        return post_border(req, cfg, cache);
    }
    if path == "/api/hotkey" && method == "POST" {
        return post_hotkey(req, cfg);
    }
    if path == "/api/follow" && method == "POST" {
        return post_follow(req, cfg, cache);
    }
    if path == "/api/hidpp/pause" && method == "POST" {
        return post_hidpp_pause(req);
    }
    if path == "/api/focus" && method == "POST" {
        return post_focus(req, cfg);
    }
    if path == "/api/controls/remap" && method == "POST" {
        return post_remap(req);
    }

    // Short config snapshot for the read routes: do not hold the lock
    // during network calls or during respond.
    let snap_cfg = match cfg.read() {
        Ok(g) => g.clone(),
        Err(_) => return err_json(req, 500, "corrupt config state"),
    };

    match path.as_str() {
        // Cheap on purpose: does NOT touch the network. The Electron health
        // check hit /api/status with an 800 ms timeout and always failed.
        // `build` exists so the deploy can check that the three machines
        // run the SAME commit. The Mac has already fallen behind and the
        // user noticed before I did.
        "/healthz" => ok_json(
            req,
            serde_json::json!({
                "ok": true,
                "build": env!("OXDESK_BUILD"),
                // Incremental: says WHICH machine is behind, not just that
                // they differ.
                "n": env!("OXDESK_BUILD_N").parse::<u64>().unwrap_or(0),
            })
            .to_string(),
        ),

        // Where input is going, and through which mechanism.
        //
        // The tray NEEDS this. It only showed "Bring it here" when some
        // device appeared offline -- a signal that worked for HID++, where
        // the device vanishes from our receiver when it switches channel.
        // In LAN mode the devices NEVER leave here, which is the point:
        // they stay online and the item would never appear. The user
        // would have no way back through the menu, depending on
        // remembering the two Ctrls.
        "/api/kvm" => {
            let mode = match cfg.read() {
                Ok(g) => g.input.mode.clone(),
                Err(_) => return err_json(req, 500, "config lock poisoned"),
            };
            ok_json(
                req,
                serde_json::json!({ "mode": mode, "target": kvm::target() }).to_string(),
            )
        }

        "/api/telemetry" => {
            ok_json(req, telemetry::summary_json(50));
            return;
        }
        "/api/status" | "/api/hosts" => {
            let snap = cached(&snap_cfg, cache);
            match serde_json::to_string(&*snap) {
                Ok(b) => ok_json(req, b),
                Err(e) => err_json(req, 500, &format!("serialize: {e}")),
            }
        }

        "/api/logs" => {
            // ?lines=N, capped inside logs::tail.
            let n = req
                .url()
                .split_once("lines=")
                .and_then(|(_, v)| v.split('&').next())
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or_else(logs::default_lines);
            match serde_json::to_string(&logs::tail(n)) {
                Ok(b) => ok_json(req, b),
                Err(e) => err_json(req, 500, &format!("serialize: {e}")),
            }
        }

        "/api/files" => {
            // ?host=<name>&path=<path>. `path` comes percent-encoded: it
            // carries `/`, `\\`, `:` and spaces, and none of them can be
            // mistaken for a query separator.
            let q = req.url().split_once('?').map(|(_, q)| q.to_string()).unwrap_or_default();
            let param = |k: &str| -> Option<String> {
                q.split('&')
                    .filter_map(|kv| kv.split_once('='))
                    .find(|(name, _)| *name == k)
                    .map(|(_, v)| files::percent_decode(v))
            };
            let snap = match cfg.read() {
                Ok(g) => g.clone(),
                Err(_) => return err_json(req, 500, "config lock poisoned"),
            };
            // No `host`: this machine, which is the common case of opening
            // the panel and just looking.
            let host = param("host").unwrap_or_else(|| {
                snap.hosts
                    .iter()
                    .find(|h| h.is_local())
                    .map(|h| h.name.clone())
                    .unwrap_or_default()
            });
            let path = param("path").unwrap_or_default();
            match serde_json::to_string(&files::list(&snap, &host, &path)) {
                Ok(b) => ok_json(req, b),
                Err(e) => err_json(req, 500, &format!("serialize: {e}")),
            }
        }

        "/api/files/transferencias" => match serde_json::to_string(&files::transfers()) {
            Ok(b) => ok_json(req, b),
            Err(e) => err_json(req, 500, &format!("serialize: {e}")),
        },

        "/api/screen" => {
            let snap = match cfg.read() {
                Ok(g) => g.clone(),
                Err(_) => return err_json(req, 500, "config lock poisoned"),
            };
            match serde_json::to_string(&screen::plans(&snap)) {
                Ok(b) => ok_json(req, b),
                Err(e) => err_json(req, 500, &format!("serialize: {e}")),
            }
        }

        // ?lang=pt-BR: the entries in the language the panel shows.
        "/api/changelog" => {
            let lang = i18n::lang_of(req.url());
            match serde_json::to_string(&changelog::entries_for(lang)) {
                Ok(b) => ok_json(req, b),
                Err(e) => err_json(req, 500, &format!("serialize: {e}")),
            }
        }

        // The translator and its catalogs (specs/022), loaded by both pages
        // in <head>, before their own scripts.
        "/i18n.js" => respond(
            req,
            Response::from_string(i18n::SCRIPT).with_header(js_header()).with_header(nostore()),
        ),
        "/i18n/catalogs.js" => respond(
            req,
            Response::from_string(i18n::catalogs_js()).with_header(js_header()).with_header(nostore()),
        ),

        "/api/layout" => {
            let snap = match cfg.read() {
                Ok(g) => g.clone(),
                Err(_) => return err_json(req, 500, "config lock poisoned"),
            };
            let hosts: Vec<_> = snap
                .hosts
                .iter()
                .map(|h| {
                    serde_json::json!({
                        "name": h.name,
                        "local": h.is_local(),
                        "os": format!("{:?}", h.os()).to_lowercase(),
                        "layout": h.layout.map(|c| serde_json::json!({"col": c.col, "row": c.row})),
                    })
                })
                .collect();
            let edges: Vec<_> = snap
                .border_edges_full()
                .into_iter()
                .map(|(e, t, ok)| serde_json::json!({ "edge": e, "target": t, "active": ok }))
                .collect();
            let body = serde_json::json!({
                "hosts": hosts,
                "edges": edges,
                "border_enabled": snap.input.border.enabled,
                // What the watcher is doing, not what the config asks for:
                // see `edge_state`.
                "border_state": {
                    "state": edge_state::get().0,
                    "detail": edge_state::get().1,
                },
                "input_mode": snap.input.mode,
                "kvm_target": kvm::target(),
                "hotkey": {
                    "enabled": snap.input.hotkey.enabled,
                    "keys": snap.input.hotkey.keys,
                    "mode": snap.input.hotkey.mode,
                    // The Mac cannot be the KVM's source, so it has no reader.
                    "available": cfg!(any(target_os = "linux", windows)),
                    "choices": config::CHORDS.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
                },
                "os": std::env::consts::OS,
                "follow_enabled": snap.input.follow.enabled,
                "follow_leader": snap.input.follow.leader,
                "hidpp_paused_s": hidpp::pause_remaining_s(),
                "devices": snap.devices.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
            });
            ok_json(req, body.to_string())
        }

        "/api/processes" => {
            let q = req.url().split_once('?').map(|(_, q)| q.to_string()).unwrap_or_default();
            let snap = match cfg.read() {
                Ok(g) => g.clone(),
                Err(_) => return err_json(req, 500, "config lock poisoned"),
            };
            let host = q
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == "host")
                .map(|(_, v)| files::percent_decode(v))
                .unwrap_or_else(|| {
                    snap.hosts
                        .iter()
                        .find(|h| h.is_local())
                        .map(|h| h.name.clone())
                        .unwrap_or_default()
                });
            match serde_json::to_string(&procs::list(&snap, &host)) {
                Ok(b) => ok_json(req, b),
                Err(e) => err_json(req, 500, &format!("serialize: {e}")),
            }
        }

        "/api/controls" => match serde_json::to_string(&hidpp::controls()) {
            Ok(b) => ok_json(req, b),
            Err(e) => err_json(req, 500, &format!("serialize: {e}")),
        },

        "/api/devices" => match serde_json::to_string(&hidpp::survey()) {
            Ok(b) => ok_json(req, b),
            Err(e) => err_json(req, 500, &format!("serialize: {e}")),
        },

        "/api/config" => match serde_json::to_string(&snap_cfg) {
            Ok(b) => ok_json(req, b),
            Err(e) => err_json(req, 500, &format!("serialize: {e}")),
        },

        // A browser asks for /favicon.ico on its own when a page declares
        // no icon; both names get the same PNG.
        "/favicon.png" | "/favicon.ico" => respond(
            req,
            Response::from_data(FAVICON.to_vec()).with_header(image_header("image/png")).with_header(nostore()),
        ),
        "/mascot.webp" => respond(
            req,
            Response::from_data(MASCOT.to_vec()).with_header(image_header("image/webp")).with_header(nostore()),
        ),

        "/setup" | "/setup.html" => respond(
            req,
            Response::from_string(SETUP).with_header(html_header()).with_header(nostore()),
        ),

        "/" | "/index.html" => {
            // Rendered per request: if refresh_seconds changes via PATCH,
            // the page reflects it without restarting the daemon.

            let page = PAGE.replace("__REFRESH__", &snap_cfg.refresh_seconds.to_string());
            respond(
                req,
                Response::from_string(page).with_header(html_header()).with_header(nostore()),
            )
        }

        _ => err_json(req, 404, "not found"),
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    /// `oxdesk --version` has to work, and it is the spelling a person
    /// types first. It answered "unknown command" until this test
    /// existed: the table renamed the flag and the match arm did not
    /// follow.
    #[test]
    fn every_spelling_of_version_reaches_the_same_arm() {
        for v in ["version", "versao", "--versao", "--version"] {
            assert_eq!(canonical(v), "version", "{v} must route to the version arm");
        }
    }

    /// A peer invokes these ON THIS MACHINE over SSH, and the peer runs
    /// its own build. Dropping a name would make the clipboard and the
    /// input handback fail against a machine that is merely behind.
    #[test]
    fn the_subcommands_a_peer_may_still_send_are_understood() {
        for (old, new) in [
            ("kvm-soltar", "kvm-release"),
            ("clip-sonda", "clip-probe"),
            ("clip-arq-set", "clip-file-set"),
            ("clip-arq-dir", "clip-file-dir"),
            ("clip-arq-prontos", "clip-file-ready"),
            ("versao", "version"),
            ("--versao", "version"),
        ] {
            assert_eq!(canonical(old), new);
            assert_eq!(canonical(new), new, "translating twice must be a no-op");
        }
    }

    #[test]
    fn an_unrelated_verb_is_left_alone() {
        for v in ["serve", "clip-set", "devices", "set-host", "--help", ""] {
            assert_eq!(canonical(v), v);
        }
    }
}
