<p align="center">
  <img src="assets/oxdesk-banner.webp" alt="OxDesk, your productivity command center" width="560">
</p>

<h1 align="center">OxDesk</h1>

<p align="center">
  <b>All your computers. One desk.</b><br>
  One keyboard, one mouse and one panel for every machine you own:
  switch between them, see their screens, share the clipboard and send
  files, over your own network, with no account and no cloud in the middle.
</p>

<p align="center">
  <a href="#get-started"><b>Get started</b></a> ·
  <a href="https://github.com/oxlayer/oxdesk/releases/latest">Download</a> ·
  <a href="specs/000-overview.md">How it works</a> ·
  <a href="CONTRIBUTING.md">Contribute</a>
</p>

<p align="center">
  <img alt="Licence: AGPL-3.0" src="https://img.shields.io/badge/licence-AGPL--3.0-8b5cf6">
  <img alt="Runs on Linux, Windows and macOS" src="https://img.shields.io/badge/runs%20on-Linux%20%C2%B7%20Windows%20%C2%B7%20macOS-8b5cf6">
  <img alt="Status: alpha" src="https://img.shields.io/badge/status-alpha-f59e0b">
</p>

## Why OxDesk

<img src="assets/oxdesk-mascot.webp" alt="The OxDesk mascot" width="170" align="right">

- **Work across machines as if they were one.** Send your keyboard and
  mouse to another computer with a hotkey, a click in the tray or the edge
  of the screen. With a Logitech receiver, the devices themselves jump
  channel: real hardware switching, not a software mirror.
- **See and drive any of them.** OxDesk has its own remote screen,
  hardware-encoded on each platform and carried over QUIC. No RDP, no VNC,
  no third-party relay.
- **Your clipboard and files follow you.** Copy text on one machine and
  paste it on the next; browse another machine's folders and send files
  across from the panel or the tray.
- **Yours, end to end.** It runs on your network, and every machine is
  trusted by its own key. No sign-up, no subscription, open source.
- **Set up by a wizard, not a wiki.** It checks every prerequisite on the
  machine it runs on, explains what is missing and fixes what it can.
- **In English or Portuguese**, whichever your browser speaks.

## Where it's going

OxDesk aims to be the one hub for your machines, your automations and your
knowledge. These are next, and **not built yet**: watch the repository, or
come and help shape them.

- **AI agents:** run automations and daily tasks with the AI you already
  use (Claude, Codex, Grok and more), from the same panel.
- **A second brain:** Markdown notes with backlinks and a knowledge graph,
  next to the machines they are about.
- **A helper that shows the way:** the mascot speaks up, in balloons that
  tell you what to do next, and becomes the face of the AI agents.

## Get started

You need two or more computers on the same network. On Ubuntu:

```bash
git clone https://github.com/oxlayer/oxdesk.git && cd ox-desk
bash install.sh          # then open http://127.0.0.1:8787/setup
```

Then the wizard takes over. For Windows and macOS, and the order to bring
machines in, see [`docs/new-computer.md`](docs/new-computer.md); the full
requirements are [below](#requirements).

> **Alpha.** OxDesk runs every day on a three-machine reference
> deployment (Ubuntu, Windows, macOS), where every number in
> [`specs/`](specs/) was measured. Outside that shape, expect rough edges,
> and tell us about them.

## Shortcuts

| Do this | What happens |
|---|---|
| Hold **both Ctrl** keys together (left and right) | The keyboard and mouse go to the next machine. Do it again and they come back |
| Push the mouse past the screen edge toward a machine placed beside this one in **Layout** | They go to that machine (with **Switch at the screen edge** on) |
| On the other machine, push **firmly** past the edge that faces home | They come back, at the height they left at (an Ubuntu target, for now) |
| Tray › **Send to *machine*** · **Bring it to *this machine*** | The same, by click |
| Tray › **Check for updates** | Says whether this machine runs the newest release |

- **The pair is yours to choose.** Both Ctrl, both Shift or both Alt, in
  **Settings › Switching machines**. A pair of the same key, left and right,
  is held by no program and typed by no text, so it never fires by accident.
- **The way back never depends on the network.** The pair is read on the
  machine your keyboard is plugged into, so it brings the input home even
  if the other machine or the Wi-Fi is gone.
- **"Firmly" means about 40 pixels past the edge.** A scroll bar on that
  edge is safe to reach for.
- **Today a Linux or Windows machine can send its keyboard and mouse; a Mac
  can receive them.** The edge works on the machine the devices are
  plugged into, and Settings says, in words, what it is doing right now.

## The gap it fills

Almost everything here exists somewhere in open source. One thing does
not: **deciding _how_ to switch computers** — move the pointer across the
LAN, or make the keyboard and mouse themselves jump Easy-Switch channel to
the other machine's receiver.

| Project | Switches the device channel | Knows the network |
|---|---|---|
| Solaar | yes (HID++ 0x1814) | no |
| Deskflow / Barrier | no | yes |
| GSConnect / KDE Connect | no | talks, but neither |

No existing project combines the three. oxdesk does; the rest of it is
the shell that makes that usable. The full argument is in
[`specs/000-overview.md`](specs/000-overview.md).

## How it works

| Piece | What it does |
|---|---|
| [`oxdesk/`](oxdesk/) | The daemon and the panel: HTTP API on `127.0.0.1:8787`, machine switching, the setup wizard, clipboard and files |
| [`stream/`](stream/) | Remote screen and LAN KVM: capture, hardware encode, QUIC transport, input injection |
| [`hidpp/`](hidpp/) | HID++ 2.0 over raw HID — Easy-Switch channel switching. Useful on its own, and MIT for that reason |
| [`ota/`](ota/) | Downloads a published build, verifies it, signs it locally and swaps itself |
| [`gui/`](gui/) | Electron shell: window and tray, no logic |
| [`agent/`](agent/), [`winput/`](winput/) | The Windows side: CLI over SSH, and input injection |

Each machine runs the same install and is both host and client: it serves
its own screen and opens the others'. Trust is per machine, by key —
authorizing A to see B does not authorize B to see A.

Capture and encode use what each platform actually has: PipeWire with
Mutter's ScreenCast API on Linux, Desktop Duplication on Windows,
AVFoundation on macOS; NVENC where there is an NVIDIA GPU, VideoToolbox on
the Mac.

## Requirements

This is not a tool you can try on one laptop:

- **Two or more computers on the same LAN**, reachable by mDNS or a
  tailnet name. Never by IP — [`specs/007`](specs/007-device-routing.md)
  records the failures behind that rule.
- **Linux:** GNOME on Wayland. Capture goes through Mutter's private
  ScreenCast and RemoteDesktop APIs, so other compositors are not
  supported yet.
- **Windows:** OpenSSH Server, and the screen host runs from a scheduled
  task with an interactive token — a service lands in session 0, where
  there is no desktop to duplicate.
- **macOS:** Homebrew's `ffmpeg@7`, plus Screen Recording and
  Accessibility granted by hand. Those grants belong to the binary, so a
  rename makes macOS ask again.
- **Hardware channel switching** needs a Logitech Unifying receiver and
  devices that speak HID++ 2.0. Everything else works without it; only
  that feature does not.
- **NVENC** needs an NVIDIA GPU. Without one the screen path costs more
  CPU.

## Install

On Ubuntu, one script checks the build dependencies (and prints the `apt`
line for whatever is missing), builds the six binaries, installs them in
`~/.local/bin` and starts the daemon as a user service. Then open the
wizard — it checks every prerequisite on the machine it runs on, explains
what is missing and fixes what it can:

```bash
bash install.sh          # then open http://127.0.0.1:8787/setup
```

Repeat on each machine. [`docs/new-computer.md`](docs/new-computer.md) has
the order of operations and [`specs/011-onboarding.md`](specs/011-onboarding.md)
the design. Automatic updates are optional:
[`specs/015-ota.md`](specs/015-ota.md).

## Documentation

The specs are the real documentation: each says what was built, what was
measured, and what failed on the way.

- [`specs/000-overview.md`](specs/000-overview.md) — the gap and the shape
  of the product
- [`specs/005-input-switch.md`](specs/005-input-switch.md) — switching
  machines, HID++ and the LAN KVM
- [`specs/010-remote-desktop.md`](specs/010-remote-desktop.md) — the screen
  stack end to end
- [`specs/008-security.md`](specs/008-security.md) — keys, trust, and what
  never leaves a machine
- [`specs/018-open-source.md`](specs/018-open-source.md) — the plan for
  this repository being public, and what the pre-publish audit found

This file used to be a 901-line runbook about a Deskflow-based setup that
no longer exists, with IP addresses, a MAC address and three machines'
hostnames in it. Every lesson in it already lives in the specs above —
Deskflow alone is discussed in eight of them — so it was deleted rather
than anonymized. It remains in the history the release tags reach:
`git show v0.2.8:README.md`.

## Contributing

Read [`CLAUDE.md`](CLAUDE.md) first: it indexes the project's rules, each
of which records a past failure. Three are not negotiable — nothing
shipped identifies a person or a machine, no credential lives in source,
and everything is in English. [`CONTRIBUTING.md`](CONTRIBUTING.md) has
the rest.

A measurement beats an opinion. If you change behaviour, say what you ran
and what it printed.

## License

[AGPL-3.0-or-later](LICENSE), except [`hidpp/`](hidpp/), which is
[MIT](hidpp/LICENSE) so other projects can use the protocol work.

The macOS packages link Homebrew's FFmpeg, which is GPL-3.0-or-later.
[`THIRD-PARTY.md`](THIRD-PARTY.md) records that, and why it is what decides
the license of the whole distribution.
