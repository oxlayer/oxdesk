// SPDX-FileCopyrightText: 2026 oxlayer
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The video loop: encoded frame -> QUIC datagrams.
//!
//! Spec: ../specs/010-remote-desktop.md
//!
//! This part is **the same** on Linux and Windows: slice, send, count,
//! and shut down when nobody is watching anymore. It stayed here when
//! Windows became a host, instead of being copied into the second binary,
//! because this is exactly where the expensive bugs lived -- the
//! `send_datagram` that failed silently, the fps count that lied, the exit
//! that hung halfway through tearing down NVENC. Two copies of this
//! diverge, and the wrong copy is always the one nobody is looking at.
//!
//! What is NOT here is what actually differs: where the frame comes from
//! (PipeWire vs DXGI) and how input is applied (D-Bus vs SendInput).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::net::{self, SliceHeader};

/// How long to wait for another client before shutting the host down.
pub const IDLE_S: u64 = 20;

/// How long to wait for the FIRST client before giving up.
///
/// The host starts on demand -- the panel wakes it and opens the viewer
/// right after -- so if nobody connected within half a minute, nobody
/// will. Without this limit, an attempt that failed on the client side
/// left capture and NVENC running on someone else's machine indefinitely,
/// because `accept()` alone waits forever.
pub const FIRST_CLIENT_WAIT_S: u64 = 30;

/// How long the host stays up after its last INPUT client leaves, ready for
/// the next switch.
///
/// Measured on 2026-11-20, with the Ubuntu machine sending to Windows: 8 of
/// 14 switches took ~2.5 s, and in every one of them all but 10-50 ms was
/// waking this host over SSH, because it had quit ~30 s after the previous
/// switch. Staying up for the next one turns those into the warm case
/// (10-14 ms in the same log).
///
/// Windows only, for now. A Linux host holds a RemoteDesktop session while
/// it runs (GNOME shows it as screen sharing), and the Mac host captures the
/// whole time; neither should linger without a reason a person can see. The
/// persistent link (specs/025, step 1) is the answer for all three; this is
/// the stopgap before it. The daemon trusts this number minus a margin
/// (`oxdesk/src/kvm.rs`, `WINDOWS_HOST_FRESH`): keep them in step.
pub const LINGER_AFTER_INPUT_S: u64 = if cfg!(windows) { 600 } else { 0 };

/// Keep waiting for clients, or shut down? While someone is sending input,
/// always wait; after the last one left, wait out the linger.
pub fn keep_waiting(has_input: bool, input_left_ago: Option<Duration>, linger: Duration) -> bool {
    has_input || input_left_ago.is_some_and(|ago| ago < linger)
}

/// Wall-clock nanoseconds.
///
/// Wall clock and not monotonic on purpose: the timestamp travels to the
/// other machine, and end-to-end latency can only be measured if both
/// sides count from the same origin. With NTP on the LAN the error is in
/// milliseconds.
pub fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// The three flags between capture and network.
///
/// Together in a struct and not three loose `Arc<AtomicBool>`: they
/// always travel together, and three boolean parameters in a row in a
/// signature are an invitation to pass them in the wrong order.
///
/// Flags and not a channel because every message here is idempotent: two
/// keyframe requests are worth one, and "exit" said twice is "exit".
///
/// All `SeqCst`, not `Relaxed`: the ORDER between them matters. The
/// network sets `idr` before `client`, and capture reads `client` before
/// `idr` -- with `Relaxed` it could see "there is a client" without
/// seeing "send a keyframe", and then the new client gets a P frame it
/// cannot decode. It is a few writes per second; the ordering is worth
/// more than the nanoseconds.
#[derive(Default)]
pub struct Signals {
    /// The network decided to shut down; whoever actually stops is the
    /// capture, which owns NVENC. Killing from the other side hung
    /// halfway through tearing down the CUDA resources.
    exit: AtomicBool,
    /// The next frame has to be an IDR.
    idr: AtomicBool,
    /// Someone is watching.
    client: AtomicBool,
    /// How many INPUT clients are connected (the KVM over LAN).
    ///
    /// Separate from `client` because they are different roles: whoever
    /// watches wants video, whoever sends input does not. Without this
    /// count, the video loop would end the process for idleness while
    /// someone is still typing through it -- taking the KVM down with it.
    inputs: AtomicUsize,
    /// The captured screen's size, `width << 32 | height`; 0 until capture
    /// has negotiated it. Announced to each input client
    /// (`input::size_line`), so a KVM source can keep the cursor's position
    /// and bring the input home at the edge (spec 024).
    size: std::sync::atomic::AtomicU64,
    /// When the last input client left (the count went to zero), for the
    /// linger (`LINGER_AFTER_INPUT_S`).
    input_left_at: std::sync::Mutex<Option<Instant>>,
}

impl Signals {
    pub fn set_size(&self, w: u32, h: u32) {
        self.size.store((u64::from(w) << 32) | u64::from(h), Ordering::SeqCst);
    }

    pub fn size(&self) -> Option<(u32, u32)> {
        let v = self.size.load(Ordering::SeqCst);
        (v != 0).then_some(((v >> 32) as u32, v as u32))
    }

    pub fn should_exit(&self) -> bool {
        self.exit.load(Ordering::SeqCst)
    }

    /// Asks the capture to end the process.
    ///
    /// Public because whoever finds out there is nothing left to do is
    /// the NETWORK, and whoever can stop safely is the capture -- it owns
    /// NVENC and CUDA, and destroying those from the other side hung.
    pub fn shutdown(&self) {
        self.exit.store(true, Ordering::SeqCst);
    }

    /// Someone connected on the input port.
    pub fn input_joined(&self) {
        self.inputs.fetch_add(1, Ordering::SeqCst);
    }

    /// Someone left the input port.
    pub fn input_left(&self) {
        if self.inputs.fetch_sub(1, Ordering::SeqCst) == 1 {
            if let Ok(mut t) = self.input_left_at.lock() {
                *t = Some(Instant::now());
            }
        }
    }

    pub fn has_input(&self) -> bool {
        self.inputs.load(Ordering::SeqCst) > 0
    }

    /// How long ago the last input client left; `None` if none ever did.
    pub fn input_left_ago(&self) -> Option<Duration> {
        self.input_left_at.lock().ok().and_then(|t| t.map(|i| i.elapsed()))
    }

    /// Consumes the keyframe request. Returns `true` once per request --
    /// the caller passes it on to the encoder.
    /// A keyframe was asked for and nobody has produced it yet. For a
    /// capture that only delivers on change (Linux), this is what tells it
    /// to re-encode the last frame instead of waiting for one.
    pub fn idr_pending(&self) -> bool {
        self.idr.load(Ordering::SeqCst)
    }

    pub fn take_idr(&self) -> bool {
        self.idr.swap(false, Ordering::SeqCst)
    }

    /// Is it worth encoding now?
    ///
    /// With nobody watching, encoding is burning GPU on someone else's
    /// machine: the frame would go into a channel nobody drains and be
    /// dropped. Measured on Windows -- 120 frames dropped before the
    /// first client connected.
    pub fn has_client(&self) -> bool {
        self.client.load(Ordering::SeqCst)
    }
}

/// An already encoded frame, waiting for the network.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub captured_at_ns: u64,
}

/// Accepts a client, streams while it is there, and asks for a stop.
///
/// `open_input` takes the connection and returns the task that applies
/// the events. A closure, not a trait, because the Linux side needs to
/// capture a D-Bus connection that **is not Send** (it runs in a
/// `LocalSet`, with `spawn_local`) while the Windows one uses a normal
/// `tokio::spawn` -- and both return the same `JoinHandle`.
pub async fn serve_video(
    ep: &quinn::Endpoint,
    rx: &mut tokio::sync::mpsc::Receiver<EncodedFrame>,
    signals: &Signals,
    mut open_input: impl FnMut(&quinn::Connection) -> tokio::task::JoinHandle<()>,
) -> anyhow::Result<()> {
    // ONE loop for the first client and for the ones that come back, with
    // different deadlines.
    //
    // It used to be two places: the loop accepted the first one, and
    // after it left there was a separate `accept` just to find out
    // whether someone came back -- which then shut down anyway, refusing
    // whoever had just connected. The log said so out loud ("someone came
    // back, but the connection does not return to the loop") and the
    // symptom was clicking "Ver a tela" twice and the second one failing
    // with "the server refused".
    let mut had_client = false;
    loop {
        // Until the first client, the deadline is generous: the panel
        // wakes the host and opens the viewer right after. After it, the
        // deadline is short -- whoever closed the window is probably done.
        let deadline = if had_client { IDLE_S } else { FIRST_CLIENT_WAIT_S };
        let incoming = match tokio::time::timeout(Duration::from_secs(deadline), ep.accept()).await {
            Ok(Some(e)) => e,
            // Endpoint closed: nothing more to accept.
            Ok(None) => break,
            Err(_) => {
                // VIDEO idleness is not host idleness.
                //
                // With the KVM connected on the input port, nobody needs
                // to be watching -- the person looks at the other
                // machine's physical monitor. Exiting here would kill the
                // process receiving their keystrokes, and the keyboard
                // would snap back on its own in the middle of use.
                // And after the KVM leaves, a while longer for the next
                // switch (`LINGER_AFTER_INPUT_S`).
                if keep_waiting(
                    signals.has_input(),
                    signals.input_left_ago(),
                    Duration::from_secs(LINGER_AFTER_INPUT_S),
                ) {
                    continue;
                }
                if had_client {
                    println!("  nobody came back in {deadline}s; shutting down");
                } else {
                    println!("  nobody connected in {deadline}s; shutting down");
                }
                signals.exit.store(true, Ordering::SeqCst);
                return Ok(());
            }
        };
        had_client = true;
        // A failed handshake is NOT fatal.
        //
        // With `?` here, a single `timed out` -- client dead mid-handshake,
        // lost packet, process ended by SIGTERM -- took down the whole
        // loop. The host stayed ALIVE, serving nobody and not exiting: the
        // next two connection attempts got not even a refusal, just
        // silence. Measured.
        let conn = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  handshake failed ({e}); still waiting");
                continue;
            }
        };
        println!(
            "  connected: {} client {}",
            conn.remote_address(),
            crate::identity::describe_peer(&conn, &crate::identity::authorized(&crate::identity::dir()))
        );
        // The new client has no context: the next frame HAS to be a
        // keyframe, or its window stays black until the GOP's next IDR --
        // up to 2s looking at nothing.
        // Order: `idr` BEFORE `client`. Capture reads `client` first, so
        // by the time it decides to encode, the keyframe request is
        // already visible.
        signals.idr.store(true, Ordering::SeqCst);
        signals.client.store(true, Ordering::SeqCst);

        // Throw away what is left from the previous session.
        //
        // The channel can hold up to `CAPACITY` encoded frames from before
        // the previous client left. For the NEW client they are P frames
        // with no reference: the decoder complains `non-existing PPS 0
        // referenced` and `no frame!` until the IDR arrives. Measured by
        // reconnecting to the Windows host.
        let mut stale = 0;
        while rx.try_recv().is_ok() {
            stale += 1;
        }
        if stale > 0 {
            println!("  dropped {stale} frame(s) from the previous session");
        }
        let Some(max) = conn.max_datagram_size() else {
            eprintln!("  ERROR: the peer does not accept QUIC datagrams; no video will get through");
            continue;
        };
        println!("  max datagram: {max} bytes");

        // Input over a unidirectional stream: losing a "key up" would
        // leave the key stuck on the host, so it cannot be a datagram.
        let input = open_input(&conn);

        let mut frame: u32 = 0;
        let mut sent = 0u64;
        // Nothing goes out before the first keyframe. A GUARANTEE, not an
        // attempt.
        //
        // Requesting an IDR and draining the channel covers the common
        // case, but does not close the race: capture reads "there is a
        // client" at the top of its loop and only sends the frame ~20 ms
        // later, so a P from the previous session can enter the channel
        // AFTER the drain. Measured -- the client logged frame 0 as a P of
        // 6342 bytes, followed by `non-existing PPS 0 referenced`, and
        // the host agreed it had sent a P.
        //
        // Dropping here costs nothing: a P frame with no reference is
        // useless to whoever just arrived, no decoder can use it. And the
        // guarantee stays LOCAL -- it depends on neither flag ordering nor
        // a time window.
        let mut awaiting_key = true;
        let mut bytes = 0u64;
        let mut refused = 0u64;
        let t0 = Instant::now();

        let mut last_request: Option<Instant> = None;
        let mut requests = 0u64;
        loop {
            let c = tokio::select! {
                // The viewer asking for a keyframe (`net::KeyframeWatch`):
                // it lost the one that opened the session, or a frame the
                // following ones reference. On a still screen nothing else
                // would ever come, so the request is the only way out.
                d = conn.read_datagram() => {
                    match d {
                        Ok(d) if d.as_ref() == net::KEYFRAME_REQUEST => {
                            // The viewer already spaces its requests; this
                            // only stops a broken one from turning every
                            // frame into an IDR.
                            if last_request.map_or(true, |t| t.elapsed() >= Duration::from_millis(200)) {
                                last_request = Some(Instant::now());
                                signals.idr.store(true, Ordering::SeqCst);
                                requests += 1;
                                if requests <= 5 || requests % 50 == 0 {
                                    println!("  keyframe requested by the client ({requests})");
                                }
                            }
                            continue;
                        }
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
                r = tokio::time::timeout(Duration::from_millis(200), rx.recv()) => match r {
                    Ok(Some(c)) => c,
                    Ok(None) => break,
                    Err(_) => {
                        if conn.close_reason().is_some() {
                            break;
                        }
                        continue;
                    }
                },
            };

            if awaiting_key {
                if !c.keyframe {
                    continue;
                }
                awaiting_key = false;
            }

            // The first frames of each connection, one by one.
            //
            // The missing half: the client said "got a P before the
            // keyframe" and I had no way of knowing whether the host sent
            // it wrong or the network delivered out of order. With both
            // sides talking, one suspect is left.
            if sent < 3 {
                println!(
                    "  sending frame {frame} {} {} bytes",
                    if c.keyframe { "KEY" } else { "P" },
                    c.data.len()
                );
            }
            let header = SliceHeader {
                frame,
                piece: 0,
                pieces: 0,
                keyframe: c.keyframe,
                captured_at_ns: c.captured_at_ns,
            };
            for slice in net::slice(&c.data, header, max) {
                bytes += slice.len() as u64;
                if let Err(err) = conn.send_datagram(slice) {
                    if !matches!(err, quinn::SendDatagramError::TooLarge) {
                        eprintln!("  datagram refused: {err}");
                    }
                    refused += 1;
                    break;
                }
            }
            frame = frame.wrapping_add(1);
            sent += 1;
            if sent % 120 == 0 {
                let s = t0.elapsed().as_secs_f64();
                println!(
                    "  {sent} frames, {:.1} fps, {:.1} Mbps, {refused} refused",
                    sent as f64 / s,
                    bytes as f64 * 8.0 / s / 1_000_000.0
                );
            }
            if conn.close_reason().is_some() {
                break;
            }
        }
        input.abort();
        signals.client.store(false, Ordering::SeqCst);
        println!("  client left");

        // Back to the top of the loop: with nobody watching, capturing and
        // encoding only burns GPU on someone else's machine, and
        // `has_client` has already turned the encoder off. The short
        // deadline decides whether it is worth staying up -- Wi-Fi
        // dropping for two seconds should not require going back to the
        // panel, and really closing the window should not leave a
        // forgotten process behind.
        println!("  waiting {IDLE_S}s for another client before exiting");
    }
    // Endpoint closed from outside. Ask for a stop anyway: capture is the
    // one that shuts down in the right order, and killing from the network
    // side hung halfway through tearing down NVENC.
    signals.exit.store(true, Ordering::SeqCst);
    Ok(())
}

/// Tells a KVM client the size of the screen its input lands on.
///
/// On a stream of its own, opened by us, so a client from before it simply
/// never accepts it. Waits up to 3 s for capture to have negotiated the
/// size -- a host woken for this very connection is still starting.
///
/// Every host calls it. Until 2026-11-18 only the Linux one did, so a KVM
/// source sending to Windows or to the Mac never learned the size, never
/// tracked the cursor, and had no way home by the edge: the only way back
/// was the rescue keys. The future is `Send`, so the Windows and Mac hosts
/// can `tokio::spawn` it outside a `LocalSet`.
pub async fn announce_size(conn: quinn::Connection, signals: std::sync::Arc<Signals>) {
    let mut size = None;
    for _ in 0..30 {
        size = signals.size();
        if size.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let Some((w, h)) = size else {
        eprintln!("  input: no screen size to announce yet");
        return;
    };
    let Ok(mut s) = conn.open_uni().await else { return };
    if s.write_all(crate::input::size_line(w, h).as_bytes()).await.is_ok() {
        let _ = s.finish();
        println!("  input: announced the screen size, {w}x{h}");
    }
}

/// Input port, separate from the video one.
///
/// `port + 1`. It exists because the host serves **one video client at a
/// time**, in sequence -- and the LAN KVM competed for that slot with the
/// viewer.
///
/// The symptom was exactly what the user described: they clicked to send
/// input to the Windows machine, the keyboard and mouse vanished from
/// here (the grab took them), and "on windows it doesn't seem to have
/// moved". It had moved: the host log recorded every `MouseRel`, and
/// measuring the cursor inside the interactive session it moved 210 px
/// for 200 requests. What did not work was **seeing**: the KVM took the
/// single slot and the viewer could not connect, so the window they were
/// looking at froze.
///
/// Splitting the ports solves two problems at once:
///
/// - watching and controlling stop competing;
/// - the target machine stops encoding video for a client that will
///   never draw anything. For gaming on Windows with the KVM, that was
///   GPU burned on behalf of nobody.
///
/// Several input clients at once are allowed and harmless -- the events
/// interleave, which is the same thing that happens with two physical
/// keyboards on the same machine.
pub async fn serve_input(
    ep: &quinn::Endpoint,
    signals: std::sync::Arc<Signals>,
    mut open_input: impl FnMut(&quinn::Connection) -> tokio::task::JoinHandle<()>,
) {
    while let Some(incoming) = ep.accept().await {
        if signals.should_exit() {
            break;
        }
        let conn = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                // A bad handshake does not take the loop down: the next
                // client deserves the same chance. Same decision as in
                // `serve_video`.
                eprintln!("  input: handshake failed: {e}");
                continue;
            }
        };
        println!(
            "  input connected: {} client {}",
            conn.remote_address(),
            crate::identity::describe_peer(&conn, &crate::identity::authorized(&crate::identity::dir()))
        );
        signals.input_joined();
        let task = open_input(&conn);

        // Decrement on EXIT, through a watcher of its own.
        //
        // Without this the count only goes up, and `has_input()` stays
        // true forever -- the host would never again exit for idleness
        // and would stay up holding the GPU after everyone left. The
        // watcher waits for the reading task to finish, which is exactly
        // when the stream closed.
        // `tokio::spawn` and not `spawn_local`: the Windows host runs in a
        // current_thread runtime WITHOUT a LocalSet, and `spawn_local`
        // panics outside one. The future here is Send (a JoinHandle and an
        // Arc of atomics), so the plain spawn works on both hosts.
        let s = std::sync::Arc::clone(&signals);
        tokio::spawn(async move {
            let _ = task.await;
            s.input_left();
            println!("  input disconnected");
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The linger keeps the host for the next switch, and only for a while.
    #[test]
    fn the_host_waits_out_the_linger_after_the_last_switch() {
        let ten_min = Duration::from_secs(600);
        assert!(keep_waiting(true, None, Duration::ZERO), "someone is typing through it");
        assert!(keep_waiting(false, Some(Duration::from_secs(25)), ten_min), "25 s after the last switch");
        assert!(!keep_waiting(false, Some(Duration::from_secs(601)), ten_min), "past the linger");
        assert!(!keep_waiting(false, None, ten_min), "nobody ever sent input: the old rules apply");
        assert!(!keep_waiting(false, Some(Duration::from_secs(1)), Duration::ZERO), "no linger: as before");
    }

    /// The moment the count reaches zero is what the linger counts from;
    /// a second client leaving while another stays does not start it.
    #[test]
    fn the_linger_starts_when_the_last_input_client_leaves() {
        let s = Signals::default();
        assert_eq!(s.input_left_ago(), None);
        s.input_joined();
        s.input_joined();
        s.input_left();
        assert_eq!(s.input_left_ago(), None, "one is still connected");
        s.input_left();
        assert!(s.input_left_ago().is_some_and(|d| d < Duration::from_secs(1)));
        assert!(!s.has_input());
    }

    /// The viewer's keyframe request reaches the encoder flag, over a real
    /// QUIC connection. Without it a lost keyframe on a still screen left
    /// the tab black for good (2026-11-07).
    #[tokio::test]
    async fn a_keyframe_request_from_the_viewer_raises_the_idr_flag() {
        let dir = std::env::temp_dir().join(format!("oxdesk-serve-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let host = crate::identity::load_or_create(&dir.join("host")).unwrap();
        let ep = net::server_with("127.0.0.1:0".parse().unwrap(), host, dir.join("host")).unwrap();
        let addr = ep.local_addr().unwrap();
        let signals = std::sync::Arc::new(Signals::default());
        let (_tx, mut rx) = tokio::sync::mpsc::channel::<EncodedFrame>(2);

        let s2 = std::sync::Arc::clone(&signals);
        let server = tokio::spawn(async move {
            let _ = serve_video(&ep, &mut rx, &s2, |_| tokio::spawn(async {})).await;
        });

        let cli = net::client_with("127.0.0.1:0".parse().unwrap(), None, None).unwrap();
        let conn = cli.connect(addr, "oxdesk-stream").unwrap().await.unwrap();

        // Connecting already asks for one; consume it, as the capture would.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !signals.take_idr() {
            assert!(Instant::now() < deadline, "connecting did not ask for a keyframe");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!signals.idr_pending());

        conn.send_datagram(bytes::Bytes::from_static(net::KEYFRAME_REQUEST)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !signals.idr_pending() {
            assert!(Instant::now() < deadline, "the request did not reach the flag");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Anything else on that connection is ignored.
        assert!(signals.take_idr());
        conn.send_datagram(bytes::Bytes::from_static(b"x")).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!signals.idr_pending(), "a stray datagram must not ask for an IDR");

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
