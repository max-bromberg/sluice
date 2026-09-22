# Verification findings

The design started from a list of things to establish on a real openSUSE
Tumbleweed machine before writing code. This records what was found and what
each finding changed in the design.

These are facts about the platform — zypper, libzypp, sdbootutil, systemd,
kernel.org — not about any one machine. Every host-specific value is
configuration; `config/profiles/` holds an example of a filled-in profile.

---

## 1. Do zypper locks support version conditions? — **Yes**

`man zypper`, Package Locks Management:

> A lock-spec is formed by `"NAME [OP EDITION]"` … You can optionally restrict
> the lock to match a specific edition or edition range using `=`, `<`, `<=`,
> `>`, `>=` or `!=` followed by the edition.

and `locks(5)` documents the file-level `version` attribute with the same
operators.

**This is the single most consequential finding.** It means a series hold is
expressible as one declarative lock:

```
zypper addlock 'kernel-default >= 7.3'
```

7.2.6 installs normally; 7.3.x is refused. There is no unlock window, so the
unlock-window fallback — remove the locks, install, put them back, and hope
nothing interrupts — is not needed for the ordinary update path at all.

Verified end to end against a scratch `--root`: `zypper addlock --comment …
'kernel-default >= 7.3'` writes `version: >= 7.3` and the comment into the
locks file, and `removelock` with a conditional spec removes only that lock,
leaving a blanket lock on the same name alone — and vice versa.

It is still implemented (`src/gate.rs`), because `promote` and `rollback` do
have to cross the gate deliberately. That path is guarded three ways: `Drop`
covers returns and panics, a signal handler covers SIGINT/SIGTERM, and a
fsynced on-disk journal covers what neither can — SIGKILL, OOM, power loss. The
journal is replayed on the next run, so the system cannot be left silently
unlocked.

## 2. Boot management — **systemd-boot via sdbootutil**

Tumbleweed with systemd-boot manages entries through `/usr/bin/sdbootutil`.

**Every zypper transaction resets the default entry.** The snapshot hook runs
`sdbootutil set-default-snapshot`, which sets `LoaderEntryDefault` to the
*top* entry of the new snapshot — the newest installed kernel. A rollback that
leaves a newer kernel installed is therefore undone by the next `zypper in` of
anything. sluice re-asserts the default after its own transactions and detects
drift from the EFI variable, which is world-readable, so an unprivileged
`status` or `check` can see it.

**Entries are per kernel per snapshot**, named
`<token>-<kernel release>-<snapshot>.conf` (e.g.
`opensuse-tumbleweed-7.2.0-1-default-4.conf`). Choosing a kernel's entry from
another snapshot would boot an old root filesystem, so sluice derives the
target id from the current default by swapping only the kernel release.

sluice reads entries with `bootctl list --json=short` and changes the default
with `bootctl set-default`. It never writes bootloader configuration itself and
never creates entries — that stays with sdbootutil. Note that `bootctl` could
not read the ESP unprivileged (`Permission denied` on `/boot/efi/EFI/systemd`),
so `boot::entries` returns `Ok(None)` rather than an error in that case: an
unprivileged `status` still works, it just says less.

## 3. ESP capacity — **often tight**

A default Tumbleweed install with systemd-boot can have an ESP of around 1 GB,
and every kernel plus initrd (per snapshot, with sdbootutil) lives on it. Two or
three kernels can fill most of it. The warning threshold is configurable
(`boot.esp_warn_free_mb`, 128 MB by default); a profile for a small ESP should
raise it so the warning arrives while there is still room to act.

## 4. `multiversion.kernels` — **explicit versions are accepted**

For example:

```
multiversion.kernels = latest,latest-1,running,7.0.12-1.1
```

An explicit EVR sits alongside the symbolic keywords, so pinning a known-good
kernel there is a supported way to protect it from `purge-kernels`.
`src/boot.rs` rewrites only that one line and refuses to create the setting if
it is absent, rather than inventing configuration.

## 5. The kernel family

A typical desktop has only `kernel-default` installed, sometimes several
versions of it side by side, often under a blanket `kernel-default` lock — the
shape `migrate` is written to adopt. A blanket lock is exactly what strands a
machine several point releases behind inside its own series.

The family is resolved by *shared version* rather than by a name list: the
anchor plus every installed package matching `kernel*` carrying the same EVR.
That rule separates `kernel-default-devel` (family) from
`kernel-firmware-amdgpu` (date-versioned, not family) with no hardcoded
exclusions. KMPs are matched separately, by the `_k<version>_<release>` marker
openSUSE puts in their version field (e.g. `7.1.4_k6.12.8_1`).

## 6. History repositories — **not depended upon**

`https://download.opensuse.org/history/` responds, but the mirror redirector
returns HTTP 200 for paths that do not exist, so a probe cannot distinguish
"this snapshot is available" from "the redirector answered". Rather than build
rollback on an unverified assumption, the local vault (`src/vault.rs`) is the
only rollback guarantee: RPMs are copied when `update` installs a version and
when `mark-good` records one, while they are definitely still in the repository.

RPMs are fetched with `zypper download NAME=VERSION`, not `install
--download-only`: the latter does nothing for a version that is already
installed, which is exactly the version `mark-good` vaults. `download` fetches
regardless of install state and works unprivileged.

The vault registers as a plain-directory repository at priority 200. The
distribution repositories sit at 99, and zypper counts *up* for lower
precedence, so the vault can never win ordinary resolution and hold the system
back. `mark-good` warns loudly when a version's RPMs could not be retrieved,
because a known-good version that cannot be reinstalled is not a rollback
target.

## 7. Clean shutdown vs hard reset — **verified against a real freeze**

A journal holding both a deliberate reboot and a hard freeze made this testable
rather than guessed. A clean reboot ends with the shutdown sequence:

```
systemd[…]: Reached target Shutdown.
systemd[…]: Finished Exit the Session.
systemd[…]: Reached target Exit the Session.
```

A hard freeze ends with ordinary activity, then nothing — no oops, no driver
error, no power-management activity. The signal is purely the *absence* of a
shutdown sequence, which is what `health.clean_end_patterns` detects. Both
shapes are in the test suite (`src/health.rs`), so the detector is pinned to
realistic data rather than to an assumption about what systemd logs.

Consequences for the implementation:

- The shutdown markers do not appear on the literal last line — user-manager
  messages follow them — so the tail is scanned, not just the final entry.
- A boot whose journal has been rotated away is scored **clean**, not unclean.
  Treating missing data as evidence of a freeze would manufacture failures.
- These lines come from the user's systemd instance, so clean/unclean works
  unprivileged. Attributing a boot to a kernel needs the kernel's own
  `Linux version …` message, which only root and `systemd-journal` can read.

## 8. regzbot — **no documented machine-readable format**

The regression tracker publishes an HTML UI with no stated stable JSON
endpoint. Scraping it would be a silent-breakage risk on a tool whose job is to
be trustworthy about risk.

regzbot is therefore not consulted at all. The lineage view is built from
kernel.org's published artifacts:

- `releases.json` lists **one entry per active branch** — `7.3-rc4`, `7.2.7`,
  `7.1.13 (iseol)`, the longterm lines — not every release. It gives series
  status and EOL flags, but a series that has aged out is simply absent, so an
  absent series older than every listed stable one is treated as EOL.
- The `v7.x/` directory index lists every `ChangeLog-7.2.N` with its date. That
  is where the point releases and their dates come from.
- Each point release's ChangeLog (14 KB–1 MB) gives its shape; the mainline
  ChangeLog (17–19 MB, the whole merge window) is skipped. Changelog shape — patch count,
revert count, and counts for your own `highlight` regexes — turns out to carry
most of the signal regzbot would have given.

---

## 9. Mesa's package family (found while building Phase 2)

```
Mesa, Mesa-libGL1, Mesa-libEGL1, libgbm1        26.2.2-2.1   from Mesa
Mesa-dri, Mesa-libva, libvulkan_radeon, …       26.2.2-2.2   from Mesa-drivers
Mesa-demo-x, Mesa-demo-egl                      9.0.0-7.5    from Mesa-demo
```

One Mesa version is built from two source packages at *different releases*,
includes libraries outside `Mesa*`, and `Mesa*` also matches an unrelated
upstream. Neither "same name prefix" nor "same version-release" describes it, so
`family_sources` groups a family by source package (`rpm -qa --qf
%{SOURCERPM}`) at the same upstream version.

---

## Design decisions made during implementation

Recorded so the reasoning survives; each was the owner's call.

- **Rollback returns to the known-good series.** The series held is explicit
  state, set by `migrate`, `promote` and `rollback`, rather than the newest
  installed version. A tested kernel kept after rollback is frozen above the
  gate and shows as gated; `--remove` uninstalls it.
- **The boot default is re-asserted, not left to the distribution.** See §2.
- **Nothing is marked known-good automatically.** Only `mark-good` and
  `migrate` record one; `update` vaults what it installs so marking it later is
  possible.
- **Desktop notifications from the root timer** go to each local graphical
  session found through logind, via `setpriv` to that user and their session
  bus.
- **Bundles group the verbs only.** Members keep their own policies; a
  component with no series gets an explicit hold after rollback.
- **The distribution-side changelog is opt-in** (`lineage.repo_changelog`),
  because it means downloading the candidate RPM before deciding to promote.

## Choices that differ from the first design

The first design was a stdlib-only Python CLI. Three things changed, all
deliberately.

**Rust instead of Python.** Stdlib Python was chosen to avoid dependencies
breaking on a rolling distribution. A Rust binary is a stronger version of that
argument: dependencies are resolved at build time and a built binary has none,
so a `zypper dup` cannot break the tool that gates `zypper dup`. It also makes
the TUI possible.

**A TUI, with the CLI intact.** Bare `sluice` opens a ratatui dashboard;
`sluice status|update|lineage|promote|…` remain scriptable and unchanged, and
`check` stays headless for the timer. Both front-ends render the same report
structs from `src/app.rs`, so they cannot drift apart about what is true.

**Nothing machine-specific is compiled in.** The code defaults to a generic
rolling-release machine; a real setup lives in configuration, with an example
in `config/profiles/`. Hardware assumptions, subsystem regexes, ESP thresholds,
webhook destinations and even the clean-shutdown patterns are configuration,
not behavior.
