# Enable and operate automatic updates (OTA)

What makes a machine download the published build and swap itself in,
without anyone running the deploy. Design and rationale:
[`specs/015`](../../specs/015-ota.md).

## 1. The token (private repositories only)

A private repository needs one: without it the API answers `200` with
`{"message":"Not Found"}` and nothing works. `oxlayer/oxdesk` is public
since 2026-11-15, so a machine that updates from it needs no token, and one
that already has a token keeps working with it.

Create a **fine-grained PAT** at
<https://github.com/settings/personal-access-tokens/new>:

| Field | Value |
|---|---|
| Resource owner | `oxlayer` |
| Repository access | Only select repositories → `ox-desk` |
| Permissions → Repository | **Contents: Read-only** |
| Expiration | whatever you are willing to renew |

Nothing beyond `Contents: Read-only`. Do **not** use the `gh` token: it
has `repo` and `workflow`, i.e. write access to everything — and this
file goes to a laptop that travels.

```bash
install -m600 /dev/null ~/.config/oxdesk/ota-token
printf '%s' 'github_pat_...' > ~/.config/oxdesk/ota-token
```

`deploy.sh` distributes it to Windows and the Mac from here (and does
`chmod 600` on the other side). The wizard **refuses** a token readable
by others.

## 2. Turn on the clock on each machine

In the wizard (`/setup` in the panel; the app also opens it by itself
when something essential is missing), the **Automatic updates (OTA)**
step has a button. It installs the system scheduler — systemd timer,
LaunchAgent, scheduled task — and does a first check in `--check` mode.

## 3. Publish a version for the machines to consume

```bash
git tag v0.3.0 && git push origin v0.3.0
```

Without a tag there is no Release, and without a Release the OTA has
nothing to download — it reports `same` forever. Recipe and cost in
[`specs/013`](../../specs/013-ci-cd.md#publishing-a-version).

## Asking by hand

```bash
oxdesk-ota --check      # only looks, changes nothing
oxdesk-ota --state      # the JSON of the last check
oxdesk-ota              # checks and applies, if there is something new
oxdesk-ota --force      # reinstalls the published one even if it's the same build
```

The earlier names of these flags (`--conferir`, `--estado`, `--forcar`)
are still accepted.

On **Windows** none of them swaps the binary: the Authenticode key
doesn't leave the Ubuntu machine, so there the OTA only checks and
reports. That's expected, and the panel shows "build #N ... is published"
as a permanent pending item until someone runs the deploy.

## When it goes wrong

| Symptom | What it is |
|---|---|
| `GitHub answered: Not Found` | the token is missing, or it doesn't grant `Contents: read` on this repo |
| `GitHub answered: Bad credentials` | invalid or expired token |
| `no package starting with oxdesk-mac-...` | the Release went out without that package (Mac job without a runner?) |
| `the package does not match` | corrupted download; the next round tries again |
| `rolled-back` | the new build didn't answer `/healthz`; the previous one came back. The copy stays in `~/.local/state/oxdesk-ota/anterior/` |
| `the timer is not firing` | the last check was more than 6 h ago — the scheduler went down |

After a boot, the Mac may take **up to an hour** to look: the
LaunchAgent is `RunAtLoad false` on purpose, because installing it must
not apply an update in the middle of the request that installed it.

## See also

- [[deploy]] — the in-house path, which signs and swaps everything
- [[configure-a-machine]] — the wizard where the step lives
