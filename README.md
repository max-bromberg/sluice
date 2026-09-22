# sluice

Series-aware update gating for rolling Linux distributions.

A rolling distribution makes one decision on your behalf that it should not: it
treats a bug-fix release and a new feature series as the same event. `zypper
dup` will move you from kernel 7.2.0 to 7.3.0 as readily as from 7.2.0 to
7.2.6, and only one of those is likely to break your machine.

The usual answer is a blanket package lock, and it is the wrong one. A lock on
`kernel-default` freezes you at the *least* mature point of a series — holding
back exactly the fixes you want, while you wait to decide about the jump you
don't. It is possible to sit six point releases behind on a package you locked
specifically because you were worried about stability.

sluice separates the two:

- **Fixes inside your current series flow automatically.** 7.2.0 → 7.2.6 needs
  no manual step.
- **A new series is held, announced and described.** 7.2 → 7.3 waits for you,
  and sluice shows you the upstream release lineage and your own machine's boot
  record so the decision is an informed one.
- **Promotion is always yours.** sluice offers evidence and a hint. It never
  crosses a feature boundary on its own, under any circumstances.

```
╭ kernel  timeline ──────────────────────── [−] 8d/10col [+]  [◂ less] ●●●○ [more ▸]  [fit] [today] ╮
│                 Aug                     Sep                  today        Oct                    │
│ 7.3                        ┄┄┄⋄┄┄┄┄⋄┄┄┄┄⋄┄┄┄┄◇┄┄│┄┄◌┄┄┄┄┄┄┄┄┄┄┄┄◇                         │
│                               rc1  rc2  rc3  rc4 │  rc5?          7.3?                       │
│▸7.2                        ▶━━━━━○○━━○━━○━━○━◉━━○│┄◌                                         │
│                            ★7.2  7.2.1  7.2.4 7.2.6 7.2.8?                                    │
│ 7.1   ─○───○───○───○──○──○──○──○┤EOL             │                                         │
│ boots ▂▂✖▂▂▂▂▂▂▂▂✖▂▂   ▂▂▂▼▂▂▂▂▂▂▂✖▂▂▂ ▂▂▂▂▂▂ ▂✖                                           │
│────────────────────────────────────────────────────────────────────────────────────────────│
│ ○ kernel 7.2.5   point release · 11 days ago                                                │
│ 558 changes · 3 reverts · touches this machine: amdgpu 30 · nvme 2 · kvm_amd 16              │
│   • drm/amdkfd: Reject zero-sized AQL queue allocations after size halving                  │
╰────────────────────────────────────────────────────────────────────────────────────────────╯
```

## Status

Early. Both phases of the design are implemented and tested — the kernel gate,
promotion, rollback, boot health, lineage and vault, then Mesa, firmware soak
and bundles — against an in-memory backend, and read-only behaviour has been
exercised on a real Tumbleweed machine. The mutating paths (`update`,
`promote`, `rollback`, `migrate`) have not yet run as root against a real
system. Treat it as something to read and try with `--dry-run` first.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/max-bromberg/sluice/main/install.sh | sh
```

The installer downloads the static binary for your CPU from the latest
release, checks it against the published SHA-256 sums, and starts
`sluice setup`. The script itself installs nothing.

Setup looks at the machine — kernel flavour and versions, a manual kernel lock,
the GPU and its Mesa and firmware packages, the ESP, how each kernel has behaved
here — and asks a handful of questions, each with a default. It then shows the
whole plan, asks for your password once, and:

- installs `sluice` to `/usr/local/bin`
- writes `/etc/sluice/config.toml`, fitted to this machine and commented
- stores a webhook URL root-only, if you gave one
- installs and starts the daily check (and the weekly update, if you asked)
- takes over a manual kernel lock, with the fallback kernel you chose
- records the install history and which kernel each boot ran

Its suggestion for the fallback kernel comes from this machine's own record: a
kernel known to have frozen it is never the default choice.

Run `sluice setup` again at any time to update or reconfigure.
`sudo sluice uninstall` undoes it — timers, locks, the binary — keeping the
configuration, state and vault unless given `--purge`.

To install a particular release: `SLUICE_VERSION=v0.1.0` before `sh`. To
install from source: `cargo build --release && ./target/release/sluice setup`.

## Use

Bare `sluice` opens the dashboard. Everything it does is also a subcommand, so
nothing is trapped behind the interactive UI:

| Command | What it does |
|---|---|
| `sluice` | Interactive dashboard |
| `sluice status` | Version, series, policy, gate state, evidence, per component |
| `sluice update` | Refresh, apply what policy allows, report what is gated |
| `sluice lineage kernel [SERIES]` | Upstream release history for a series |
| `sluice promote kernel` | Cross the gate. Asks first, shows the lineage first |
| `sluice mark-good kernel [VER]` | Vault it, pin it, record it as the rollback target |
| `sluice rollback kernel [--remove]` | Boot the known-good version and hold its series again |
| `sluice health [KERNEL]` | Boot evidence per kernel version |
| `sluice check [--force]` | Headless; notifies about anything needing attention |
| `sluice migrate [--known-good C=V] [--undo]` | Adopt an existing manual lock setup, reversibly |
| `sluice repair` | Restore locks left off by an interrupted run, and the boot default |

`promote`, `mark-good` and `rollback` also take the name of a *bundle* (below).

Read-only commands run unprivileged. Mutating ones need root and say so. The
dashboard runs unprivileged and re-executes itself under `pkexec` or `sudo` for
a single action, showing you the exact argv before it runs.

Every command that changes anything takes `--dry-run`, which needs no root: it
evaluates, and prints what it would lock, install and pin.

## The dashboard

Bare `sluice` opens it. The header carries whatever deserves a glance — a gated
series, a boot default that has moved, a nearly full ESP, a component with no
rollback target — and the overview ends with the machine: ESP use, the default
boot entry, the running kernel.

The **timeline** (`2`, or `l`) is the centre of it. Every series of a tracked
package is a lane in time; this machine is drawn on top of it:

| | |
|---|---|
| `▶` | the version running now (it pulses) |
| `●` | installed |
| `★` `✔` `⚗` | boots by default · known-good · under test |
| `◆` | gated, waiting for your decision |
| `◉` | offered by your repositories, not installed yet |
| `◌` `◇` | releases not out yet, placed by their cadence — always a guess, and drawn as one |
| `▂` `✖` | your boots, and the ones that ended without a shutdown |

Scrub release by release with `←` `→`, move between series with `↑` `↓`, pan
with `⇧←` `⇧→` or by dragging, and zoom from days to years with `+` `−` or the
wheel. `[` and `]` (or the buttons) choose how much to show: at the top level
each release carries a bar for how much changed, coloured by how much of that
touches this machine, and the card below lists the changes that do.

Your own history is on it too. Every version this machine has installed and
removed appears on the machine row (`▼` `▽`), a kernel you once had shows `◍`,
and scrubbing to a kernel lights up the boots that ran it. Each kernel's card
carries its record here — boots, hours, unclean ends, and unclean ends per
100 hours so short and long stints compare fairly — and a `⚠` marks kernels
that have ended a boot uncleanly.

A `↺` marks a release whose changes to this machine's drivers were reverted in
a later release of the series — a regression upstream noticed and backed out.
Reverts are matched to what they undo by commit id (stable backports carry
their upstream id) or by subject, across the series you are on and the ones
waiting for you.

`p` on the timeline (or anywhere) opens a promotion preview before anything
runs: the packages, the rollback target and whether it is vaulted, the gate
before and after, the boot default, what the release did to your drivers and
whether any of it has since been reverted — and anything that blocks it.
`sluice promote` prints the same preview.

"This machine" is worked out, not configured: the drivers actually bound to its
devices (read from sysfs), the modules they rely on, and its mounted
filesystems. A fix to `amdgpu`, `mt7925` or `btrfs` counts; a fix to another
vendor's chip that shares a library does not.

Kernel history comes from kernel.org, Mesa's from its release archive and notes,
firmware's from the linux-firmware tarball directory; anything else published as
a directory of release tarballs works with `lineage = "index"`. Bundles get one
canvas for the whole stack. Fetching happens off the UI thread, and everything
is cached, so the timeline works offline.

## How it decides

| Policy | Behaviour |
|---|---|
| `follow` | No gating. Plain distribution-upgrade behaviour. |
| `hold-series` | Auto-apply within the current `MAJOR.MINOR`; gate series changes. |
| `soak` | Auto-apply once a version has sat unchanged in the repo for N days. |
| `hold` | Never automatic. |

`hold-series` is expressed as version-conditional locks —
`kernel-default >= 7.3`, and the same for every other family member — so point
releases inside 7.2 are never locked at all and there is no unlock window to get
wrong. `soak` locks `> <installed>` until a candidate has settled. See
[docs/verification.md](docs/verification.md#1-do-zypper-locks-support-version-conditions--yes).

A component is a *family*, not a package. `kernel-default` moves in one
transaction with `kernel-devel`, `kernel-syms` and any KMPs built against it,
because a half-moved kernel family does not boot. The family is derived from
the installed set by shared version, so date-versioned firmware never gets swept
in with it. Where one version is built from several sources at different
releases — Mesa is `Mesa` plus `Mesa-drivers`, and ships `libgbm1` and
`libvulkan_radeon` too — `family_sources` groups by source package instead.

The series being held is recorded, not guessed from the newest installed
version: after a rollback, the tested kernel may still be installed, and it is
outside your series rather than in it.

A new series is announced as soon as it appears, even while a fix inside your
series is still being applied.

## Boot health

The failure this was written for leaves nothing in the log: the machine freezes
hard, and the journal simply stops. There is no oops to find and no error to
grep for.

The only durable signal is negative — a boot that ends with none of the shutdown
sequence a deliberate reboot always writes. sluice counts those per kernel
version:

```
7.3.3-1-default        6 boots, 141 h uptime, 0 unclean ends
7.2.0-1-default        1 boot, 13 h uptime, 1 unclean end
```

The detector is pinned to real journal tails — one clean reboot, one genuine
freeze — captured in the test suite. A boot whose journal has been rotated away
is scored *clean*, because treating missing data as evidence of a freeze would
manufacture failures.

Attributing boots to kernels needs the kernel's own messages, which only root
and the `systemd-journal` group can read; unprivileged, sluice says so rather
than guessing.

sluice never marks a kernel good on this. The evidence informs your decision.

`check` reports each unclean end once — boots that ended since the last check —
rather than the whole history every day.

Which kernel a boot ran is only in the kernel's own journal messages, and the
install history only in zypp's root-only log. Runs with root — the `check`
timer, `update`, `sudo sluice health` — record both in `evidence.json` in the
state directory, and accumulate boot records as the journal rotates. Without
them, a boot is attributed by inference (the newest installed kernel is what
sdbootutil boots) only where that is certain, and always marked as inferred;
older boots stay "unknown" rather than being pinned on the wrong kernel.

## Rollback actually works

On a rolling distribution the repository only carries the present. Once
Tumbleweed moves to 7.3, the 7.2.6 RPMs are gone, and with them any ability to
go back.

`update` vaults each version it installs, while the repository still has it,
and `mark-good` pins the one you vouch for in `multiversion.kernels` so
`purge-kernels` cannot remove it at boot (releasing the pin it set on the
previous one, never a pin you added yourself). The vault registers as a zypper
repository at priority 200 — below the distribution's 99 — so it can never win
ordinary resolution and hold you back. `promote` refuses to run if the
known-good version is not vaulted, because promoting without a rollback target
is a one-way trip. Nothing is ever marked known-good automatically.

`rollback` puts you back on the known-good version *and its series*: the gate
returns to `>= 7.3`, a kept 7.3 kernel is frozen above it and shows as gated
again, and `promote` takes it back without downloading anything. `--remove`
uninstalls it instead, and refuses while it is the running kernel. A component
with no series (`follow`, `soak`) is held at the known-good version until you
promote it, or the next update would simply undo the rollback.

### The default boot entry

On openSUSE, every zypper transaction ends with the snapshot hook running
`sdbootutil set-default-snapshot`, which makes the *newest installed kernel* the
default. After a rollback that keeps 7.3 installed, any `zypper in` would
quietly make 7.3 the default again. So for components with `boot_entries =
true`, sluice keeps the default on the version it is holding: it re-asserts it
after every transaction it runs, `status` and `check` read the
`LoaderEntryDefault` EFI variable (no root needed) and warn if something else
moved it, and `sluice repair` puts it back. Entries exist per kernel *per
snapshot*; sluice only ever picks the entry in the snapshot you are booting.

### Bundles

```toml
[bundle.gpu]
components = ["kernel", "mesa", "amdgpu-firmware"]
```

`sluice promote gpu` crosses every gated member in one transaction and records
the whole stack as under test; `mark-good gpu` and `rollback gpu` act on all of
them. Members keep their own policies for everyday updates.

## Configuration

Nothing machine-specific is compiled in. Paths, URLs, thresholds, subsystem
regexes, notification destinations and even the clean-shutdown patterns are
configuration.

`config/sluice.example.toml` documents every option at its default.
`config/profiles/opensuse-tumbleweed-amdgpu.toml` is a real filled-in profile —
a Tumbleweed workstation with an AMD GPU — kept as an illustration of
what a considered configuration looks like.

Search order: `$SLUICE_CONFIG`, `./sluice.toml`,
`$XDG_CONFIG_HOME/sluice/config.toml`, `/etc/sluice/config.toml`. With no file
anywhere, the built-in defaults apply. `sluice show-config` prints what is
actually in effect.

Webhook URLs are never stored in configuration — only the path to a root-only
file containing one — so a config file stays safe to publish. From the root
timer, desktop notifications go to each local graphical login through logind,
delivered as that user on their own session bus.

## Portability

The policy engine, version comparison, lineage parsing and health analysis are
distribution-agnostic. Package operations go through a `PackageBackend` trait
with one implementation: zypper.

The seam is there so the interesting logic can be tested against a mock without
a distribution, not to suggest other backends work. Porting to `pacman` or
`dnf` means implementing that trait and, more importantly, working out what
"series" means there.

## Safety

- Never auto-promotes across a feature series. Hints only.
- Never removes the known-good kernel, and keeps `purge-kernels` from doing it.
- Never leaves locks removed. Guarded by `Drop`, by a signal handler, and by an
  fsynced journal replayed on the next run — so even SIGKILL cannot leave the
  system silently unlocked.
- Never edits bootloader configuration directly; only `bootctl set-default`,
  and only to an entry in the snapshot already being booted.
- Never compiles a kernel. Distribution RPMs and the local vault only.
- Not a replacement for zypper. Unmanaged packages behave exactly as in a plain
  `zypper dup`.

## Development

```sh
git config core.hooksPath .githooks   # privacy check on commit; fmt, clippy, tests on push
cargo test          # the interesting cases are in src/policy.rs and src/gate.rs
cargo clippy --all-targets
cargo fmt --all
```

Building needs a C toolchain for linking (`gcc` or `clang` plus `binutils`).

`scripts/privacy-check.sh` runs in CI and in the hooks. It refuses email
addresses, home paths, network identifiers, webhook tokens and keys anywhere in
the tree, and non-noreply commit identities. Terms specific to one person or
machine go in a git-ignored `local/privacy-denylist`, so they are checked for
without ever being published.

## Releasing

```sh
scripts/release.sh 0.2.0
```

That sets the version, tags `v0.2.0` and pushes it. The Release workflow then
tests, builds static binaries for x86_64 and aarch64, runs the x86_64 one and
the installer against it, and publishes the release with `SHA256SUMS` and
`install.sh`. Running the workflow by hand from the Actions tab does all of
that except publishing.

## Licence

MIT or Apache-2.0, at your option.
