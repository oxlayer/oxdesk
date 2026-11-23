// SPDX-FileCopyrightText: 2026 oxlayer
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Host on Windows: DXGI -> NVENC -> QUIC.
//!
//! Spec: ../specs/010-remote-desktop.md
//!
//! ## Simpler than the Linux one, and the reason matters
//!
//! On Linux capture and input go through the compositor, and the two
//! Mutter sessions have to be born in the right order, tied by `SessionId`,
//! with the input one belonging to the D-Bus connection that created it.
//! Hence the choreography between threads in that file.
//!
//! Here there is no middleman: DXGI hands over the texture and `SendInput`
//! injects from any thread. So it is one capture loop and one network
//! thread, with no negotiation.
//!
//! ## Needs the interactive session
//!
//! Desktop Duplication does not exist in session 0, and `SendInput` there
//! reaches no desktop at all. Launching this process over SSH does NOT work
//! -- SSH lands in session 0. What wakes it is a scheduled task with `/IT`,
//! the same mechanism that brings up `oxdesk` (see `screen::acordar_host` in
//! oxdesk).

#![cfg(windows)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::capture_win::{CaptureState, Monitor};
use crate::input::Reader;
use crate::input_win::Injector;
use crate::nvenc::NvencEncoder;
use crate::serve::{self, now_ns, EncodedFrame, Signals};

/// Two frames of slack. More than that would be a queue of stale stuff.
const CAPACITY: usize = 2;

/// With a still screen, resend the last frame every so often.
///
/// Without this, a client that connects in front of a motionless screen
/// would be left with a black window: DXGI delivers no frame when nothing
/// changes, and our loop would have nothing to encode. The heartbeat also
/// goes out as an IDR, so it serves as a "start" for whoever just joined.
const HEARTBEAT: Duration = Duration::from_millis(500);

/// How long to wait for a new frame on each turn. One frame at 60 Hz.
const WAIT_MS: u32 = 16;

/// With a client waiting and NO frame captured yet, how often to nudge the
/// screen awake.
///
/// The real case: the machine was left alone, power saving turned the
/// video output off, and DXGI starts returning `Idle` forever --
/// `DuplicateOutput` does not even refuse. Without this the client
/// connected and stayed on a black screen indefinitely, and the host stayed
/// up with nothing to do. Measured: zero frames in eight seconds with the
/// screen off.
const NUDGE: Duration = Duration::from_secs(3);

/// After this, the duplication is recreated instead of just nudging.
///
/// Waking the screen can invalidate a duplication that was born with the
/// output off. Nudging alone would not fix that case, and recreating is
/// cheap compared to leaving the user staring at nothing.
const RECREATE_AFTER: Duration = Duration::from_secs(9);

pub fn run(port: u16, monitor: u32) -> anyhow::Result<()> {
    // FIRST THING: wake the screen, and keep it awake.
    //
    // With the screen off for power saving there is no video output on,
    // and `DuplicateOutput` refuses all five attempts -- the host died
    // saying "screen locked", which was the wrong diagnosis: there was no
    // password at all, just the monitor off. And it is precisely when the
    // machine has been left alone for a while that you want to reach it
    // from afar.
    //
    // Before ANY DXGI call, including enumerating the output: waking after
    // a call that already failed would not help. The guard lives until the
    // end of the function because the screen going off mid-session would
    // take the duplication down with ACCESS_LOST -- waking at startup is
    // not enough. It is released in `Drop`, so we do not keep the machine
    // awake after the host dies.
    //
    // It does NOT solve a really locked session: there the secure desktop
    // is in front and DXGI does not capture it, by design. Known limit.
    //
    // Keeping it awake is per session since 2026-11-20 (`keep_awake_in_use`),
    // not for the life of the process: the host now lingers for the next
    // switch (`serve::LINGER_AFTER_INPUT_S`), and a guard held for its whole
    // life would keep the monitor on for minutes after the person left. The
    // wake here stays: the host is started because a session is about to
    // begin, and capture has to open with the output on.
    crate::input_win::wake_screen();
    // And the screensaver, which is a DIFFERENT problem from the screen
    // being off: it runs on its own desktop and DXGI refuses to duplicate
    // for whoever is not on the input desktop. `wake_screen()` does not
    // dismiss it, because `SendInput` goes to the calling thread's desktop.
    crate::input_win::end_screensaver();

    // The cursor this machine does not draw by itself. It only appears
    // while someone is sending input -- see `cursor_win`.
    crate::cursor_win::start();

    // ORDER MATTERS: the encoder first, the duplication after.
    //
    // Holding the DXGI duplication while CUDA initialized hung the process
    // -- it stopped at "DXGI: monitor 0" with 0.2 s of CPU and never got
    // past it, while the NVENC probe alone ran at 111 fps in the same
    // session. Opening the monitor (cheap) gives the size without starting
    // to capture; capture starts when there is already somewhere to send.
    let mon = Monitor::open(monitor)?;
    let (w, h) = mon.size();
    // Shared with the network thread: input converts stream pixels into a
    // position on the virtual desktop, and that depends on where this
    // monitor is.
    let origin = Arc::new(Mutex::new(mon.origin()));
    let mut enc = NvencEncoder::new(w, h)?;
    println!("  NVENC ready for {w}x{h}, {:.1} Mbps target", enc.target_mbps());
    let mut screen = mon.duplicate()?;

    let (tx, rx) = tokio::sync::mpsc::channel::<EncodedFrame>(CAPACITY);
    let signals = Arc::new(Signals::default());
    // Known before the first frame: the monitor said it when it opened.
    // A KVM source needs it to keep the cursor's position and come home by
    // the edge (`serve::announce_size`).
    signals.set_size(w, h);
    keep_awake_in_use(Arc::clone(&signals));

    let network = {
        let signals = Arc::clone(&signals);
        let origin = Arc::clone(&origin);
        std::thread::spawn(move || {
            if let Err(e) = run_network(port, rx, Arc::clone(&signals), origin) {
                eprintln!("  network: {e}");
            }
            // The network is done -- by success or by error. Either way
            // there is nobody left to send frames to, and the one who shuts
            // down in the right order is the capture. Without this line, a
            // network error left the process alive forever, capturing for
            // nobody.
            signals.shutdown();
        })
    };

    println!("  host on port {port}; ctrl-c to stop");
    let mut last_send = Instant::now();
    let mut dropped = 0u64;
    // Mute-screen watchdog: since when a client has been waiting without
    // the capture having produced a single frame.
    let mut mute_since: Option<Instant> = None;
    let mut last_nudge: Option<Instant> = None;
    let mut already_recreated = false;
    // Capture failures in a row. With nobody watching (a host lingering for
    // the next switch, the screen locked), retrying every few hundred ms
    // would print three lines a second for as long as the lock lasts.
    let mut failing = 0u32;

    while !signals.should_exit() {
        // With nobody watching, do not encode: the frame would go into a
        // channel nobody drains. `AcquireNextFrame` keeps being called
        // because it is what keeps the duplication alive and detects loss
        // of access -- only NVENC sits idle.
        let watching = signals.has_client();
        if signals.take_idr() {
            enc.request_idr();
        }

        // Mute-screen watchdog.
        //
        // `has_image()` false with a client connected means we never
        // captured anything to send -- the still-screen heartbeat has
        // nothing to resend and the loop would `continue` forever. That is
        // exactly what happened with the screen off: the client sat on
        // black and the host did not complain.
        if watching && !screen.has_image() {
            // Two clocks: the total decides when to recreate, the nudge one
            // decides when to nudge again. With just one, resetting the
            // clock to space out the nudges would wipe the total and the
            // recreate would never come.
            let since = *mute_since.get_or_insert_with(Instant::now);
            let mute_for = since.elapsed();
            let nudge = last_nudge.map_or(mute_for >= NUDGE, |t: Instant| {
                t.elapsed() >= NUDGE
            });

            if mute_for >= RECREATE_AFTER && !already_recreated {
                already_recreated = true;
                println!("  no frame in {}s; recreating the duplication", mute_for.as_secs());
                if let Err(e) = screen.recreate() {
                    eprintln!("  recreate after mute screen: {e}");
                }
            } else if nudge {
                println!("  no frame in {}s; waking the screen", mute_for.as_secs());
                crate::input_win::wake_screen();
                last_nudge = Some(Instant::now());
            }
        } else if screen.has_image() {
            mute_since = None;
            last_nudge = None;
            already_recreated = false;
        }

        // Said on the first failure and then every 60th; retried fast only
        // when someone is waiting for a picture.
        let say_it = failing == 0 || failing % 60 == 0;
        let pause = Duration::from_millis(if watching { 250 } else { 5000 });
        let state = match screen.next_frame(WAIT_MS) {
            Ok(e) => e,
            Err(e) => {
                if say_it {
                    eprintln!("  capture: {e} (failure {} in a row)", failing + 1);
                }
                failing += 1;
                std::thread::sleep(pause);
                continue;
            }
        };

        if failing > 0 && !matches!(state, CaptureState::Lost) {
            println!("  capture back after {failing} failure(s)");
            failing = 0;
        }

        match state {
            CaptureState::Lost => {
                // UAC screen, resolution change, a game taking over the
                // display. Not an error: recreate and carry on. Failing here
                // is normal while the secure desktop is in front, hence the
                // pause before trying again.
                if say_it {
                    println!("  capture lost; recreating");
                }
                match screen.recreate() {
                    Ok(changed) => {
                        if let Ok(mut g) = origin.lock() {
                            *g = screen.origin();
                        }
                        if changed {
                            let (w, h) = screen.size();
                            signals.set_size(w, h);
                            enc = NvencEncoder::new(w, h)?;
                            println!("  NVENC rebuilt for {w}x{h}");
                        }
                    }
                    Err(e) => {
                        if say_it {
                            eprintln!("  recreate: {e} (failure {} in a row)", failing + 1);
                        }
                        failing += 1;
                        std::thread::sleep(pause);
                    }
                }
                continue;
            }
            CaptureState::Idle => {
                if !watching || !screen.has_image() || last_send.elapsed() < HEARTBEAT {
                    continue;
                }
                // Still-screen heartbeat: goes out as an IDR to serve as a
                // start for whoever just connected.
                enc.request_idr();
            }
            CaptureState::New => {
                if !watching {
                    continue;
                }
            }
        }

        let captured = now_ns();
        let encoded = {
            let m = match screen.map_frame() {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("  map: {e}");
                    continue;
                }
            };
            enc.encode(m.px, m.stride)
        };
        match encoded {
            Ok(data) => {
                let keyframe = enc.last_was_idr();
                last_send = Instant::now();
                // `try_send`: if the network cannot keep up, the new frame
                // is dropped HERE instead of becoming a queue. A late frame
                // has no value on a remote screen.
                if tx.try_send(EncodedFrame { data, keyframe, captured_at_ns: captured }).is_err() {
                    dropped += 1;
                    if dropped % 120 == 0 {
                        println!("  {dropped} frames dropped (network slower than the GPU)");
                    }
                }
            }
            Err(e) => eprintln!("  {e}"),
        }
    }

    println!("  capture ended");
    let _ = network.join();
    Ok(())
}

/// Network: serves video by datagram and receives input by stream.
///
/// Unlike Linux, here a plain `tokio::spawn` works: `SendInput` has no
/// owning thread nor a D-Bus session to respect.
fn run_network(
    port: u16,
    mut rx: tokio::sync::mpsc::Receiver<EncodedFrame>,
    signals: Arc<Signals>,
    origin: Arc<Mutex<(i32, i32)>>,
) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(async move {
        let listen = crate::bind::listen_addr();
        let ep = crate::net::server((listen, port).into())?;
        // The REAL address, not a hardcoded "0.0.0.0": with --bind overlay
        // the host listens somewhere else, and a log saying 0.0.0.0 would
        // send whoever is diagnosing to look in the wrong place.
        println!("  listening on {listen}:{port}");

        // Input port, next to the video one.
        //
        // The host serves one VIDEO client at a time, and the LAN KVM
        // fought the viewer for that slot. On separate ports, watching and
        // controlling stop competing -- and the machine stops encoding
        // video for a client that never draws anything.
        let input_port = port + 1;
        let ep_in = crate::net::server((listen, input_port).into())?;
        println!("  input on {listen}:{input_port}");
        {
            let signals_in = Arc::clone(&signals);
            let origin_in = Arc::clone(&origin);
            tokio::spawn(async move {
                let signals_size = Arc::clone(&signals_in);
                serve::serve_input(&ep_in, Arc::clone(&signals_in), |conn| {
                    let conn = conn.clone();
                    let origin = Arc::clone(&origin_in);
                    tokio::spawn(serve::announce_size(conn.clone(), Arc::clone(&signals_size)));
                    tokio::spawn(async move { receive_input(conn, origin).await })
                })
                .await;
            });
        }

        serve::serve_video(&ep, &mut rx, &signals, |conn| {
            let conn = conn.clone();
            let origin = Arc::clone(&origin);
            tokio::spawn(async move { receive_input(conn, origin).await })
        })
        .await
    })
}

/// Reads a connection's input stream and injects until it closes.
///
/// One for both ports: the viewer sends input over the video connection,
/// the KVM sends it over the input port, and both deserve the same
/// treatment. Duplicating this would invite the two copies to diverge --
/// and one of them is the one the user types with.
async fn receive_input(conn: quinn::Connection, origin: Arc<Mutex<(i32, i32)>>) {
    let Ok(mut recv) = conn.accept_uni().await else { return };
    // From here on the arrow on this desktop is ours: whoever is driving
    // has to see where the pointer is, and with no mouse installed Windows
    // draws nothing.
    let _arrow = CursorWhileDriving::new();
    let _in_use = InUse::new();
    let mut injector = Injector::new(origin);
    let mut reader = Reader::default();
    let mut buf = vec![0u8; 4096];
    let mut n = 0u64;
    let mut held = crate::input::Held::default();
    while let Ok(Some(read)) = recv.read(&mut buf).await {
        reader.feed(&buf[..read]);
        while let Some(e) = reader.next_event() {
            held.note(&e);
            injector.apply(e);
            n += 1;
            // The first ones, one by one: "no error" is weak proof that the
            // input arrived. After that only the aggregate, so as not to
            // flood the log at 60 Hz.
            if n <= 8 {
                println!("  input #{n}: {e:?}");
            } else if n % 500 == 0 {
                println!("  {n} input events applied");
            }
        }
    }
    // The stream ended -- cleanly or not. Release what the client left
    // held (`input::Held`).
    let left = held.release_all();
    if !left.is_empty() {
        for e in &left {
            injector.apply(*e);
        }
        println!("  input: released {} key(s)/button(s) the client left held", left.len());
    }
}

/// Keeps the desktop arrow on for as long as the input connection lives.
///
/// A guard and not two calls: `receive_input` leaves through several doors
/// (stream closed, read error, the connection dropping), and the one that
/// forgot to turn it off would leave an arrow stuck on someone's screen
/// forever. Turning it ON lives in `new` for the same reason -- so there is
/// no way to raise the counter without something that lowers it.
/// Sessions applying input on this machine right now.
static IN_USE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Marks a session as using this machine, for as long as it lives.
struct InUse;

impl InUse {
    fn new() -> Self {
        IN_USE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        InUse
    }
}

impl Drop for InUse {
    fn drop(&mut self) {
        IN_USE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Holds the display awake while a session uses this machine (someone
/// typing through the KVM, or watching), and lets it sleep otherwise.
///
/// A thread of its own because `SetThreadExecutionState` belongs to the
/// thread that calls it. When a session starts on a host that lingered,
/// the screen may have gone off or the screensaver may be up: both are
/// dealt with then, as they were at startup.
fn keep_awake_in_use(signals: Arc<Signals>) {
    std::thread::spawn(move || {
        let mut held: Option<crate::input_win::ScreenAwake> = None;
        while !signals.should_exit() {
            let want = IN_USE.load(std::sync::atomic::Ordering::SeqCst) > 0 || signals.has_client();
            if want && held.is_none() {
                held = Some(crate::input_win::ScreenAwake::new());
                crate::input_win::wake_screen();
                let n = crate::input_win::end_screensaver();
                println!("  session started: keeping the display awake{}", if n > 0 { " (screensaver ended)" } else { "" });
            } else if !want && held.is_some() {
                held = None;
                println!("  no session: the display may sleep again");
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
}

struct CursorWhileDriving;

impl CursorWhileDriving {
    fn new() -> Self {
        crate::cursor_win::driving(true);
        CursorWhileDriving
    }
}

impl Drop for CursorWhileDriving {
    fn drop(&mut self) {
        crate::cursor_win::driving(false);
    }
}
