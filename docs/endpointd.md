# avb-endpointd: this computer's entities, for any app

Status: reviewed by triib (2026-11-08). Step 1 is on the `endpointd`
branch here and the `avb-endpointd` branch in triib.

## Why

A person should be able to put this computer on an AVB network as a
talker that any app plays into, as it would into speakers, without plaa
or triib running. Today neither can do that:

- plaa's entity lives inside plaa. Its one stream output sends a copy
  of what plaa itself plays, and only while plaa runs.
- triib's entities run in triib-endpointd, which outlives triib's window
  and already gives each stream a PipeWire device apps play into or
  record from. But only triib starts it, with nothing at login, and it is
  triib's: plaa can't add an entity to it.

Two programs running entities on one interface also each carry the same
machinery: an MRP participant, with plaa's sharing, handoff and
step-aside logic so that the two don't withdraw each other's
declarations; a raw socket helper (plaa-netraw, triib-netraw); a link
unit (plaa-link@, triib-link@); and starting gptpd.

avb-endpointd is triib-endpointd moved into the avb repository and made
neutral: one daemon per interface in the person's session, running every
entity on the computer, whichever app made it. Each stream's audio comes
from or goes to a PipeWire device. plaa and triib become two ways to
configure it.

## What it does, and what it leaves to the programs

It does, as triib-endpointd does now:

- run the computer's entities on one interface from a configuration
  file: ADP, the AEM responder, ACMP, MAAP, MSRP and MVRP in one shared
  participant, CVU SRP for AVB Lite, and gPTP from gptpd;
- send and receive each entity's streams with avb-stream's threads,
  under RealtimeKit, as now (see "The data path");
- give each stream output a PipeWire playback device, and each stream
  input a recording device. A stream can instead use a cpal device, a
  tone or silence;
- keep what controllers change (names, formats, transit times, clock
  sources, mappings, bindings) in the configuration file;
- start gptpd and the link unit on the interface, as both programs do
  now.

It leaves to the programs:

- controllers. plaa's and triib's controllers stay in their apps, as
  neither reserves streams;
- choosing what each app plays into a device or records from it. To the
  daemon, plaa is one more app playing into a device.

## Library and daemon

triib builds from crates.io on Linux, macOS and Windows, and uses the
daemon's types on all three: the configuration, status and meters, and
the entity IDs, in its host, meter, preset and network views. So
`avb-endpointd` is published in two halves:

- **A library** that builds everywhere: the configuration file's types,
  with load and save under the file lock, the status and meters readers,
  the paths, and `entity_id`. triib-store's load, save and paths that
  the daemon uses move with it.
- **The daemon**, Linux only, behind a feature.

The daemon and the apps no longer ship together, so:

- each app's package needs a minimum avb-endpointd version;
- readers take values they don't know as "other", as status's Setup
  does now.

## Shape

### The unit

- `avb-endpointd@<interface>.service` is a systemd user unit template
  in `/usr/lib/systemd/user`. `ExecStart` passes `%i`, the interface's
  name as it is, as gptpd@ and the link units do: `%I` would unescape a
  dash into a slash. The daemon keeps checking that the name is an
  interface it can use. It runs in
  the person's session, so its devices are in that session's PipeWire.
- **Ordering:**
  - `WantedBy=` and `PartOf=graphical-session.target`. default.target
    would also start it under linger and on logins that are only SSH,
    and there polkit refuses gptpd@ and the link units, and RealtimeKit
    refuses real-time priority. A desktop that never reaches the
    graphical session target doesn't start it at login; an app still
    starts it.
  - `After=pipewire.service` and `PartOf=pipewire.service`, so a
    PipeWire restart restarts the daemon and its devices. The entities
    leave the network for that moment. Later, avb-pipewire reconnects
    after a core error instead.
- **Restarts:**
  - `Restart=on-failure` replaces triib's bring_back.
  - Exits that aren't crashes have their own exit status, listed in
    `RestartPreventExitStatus=`, so they don't loop into the start
    limit: another person's daemon holds the interface, no such
    interface, or a configuration file that doesn't parse.
  - Apps run `systemctl --user reset-failed` for the instance before
    starting it.
  - `RestartSteps=` and `RestartMaxDelaySec=` need systemd 254 (Debian
    12 has 252 and warns and ignores them), so the unit uses only a
    fixed `RestartSec=`.
- **Process:**
  - `KillMode=mixed`, so the daemon ends its own helpers.
  - No hardening that implies NoNewPrivileges, which would strip
    avb-netraw's file capability. Started by systemd, the daemon always
    gets the helper with its capability. A triib started from a
    NoNewPrivileges process doesn't today.
  - It logs to the journal instead of `endpointd.log`.
- **Who starts it:**
  - An app enables and starts it when it adds the interface's first
    entity. A user unit needs no polkit. Enabled, it starts at login
    with no app running.
  - With no entities listed it exits 0, so an enabled unit with nothing
    to run costs nothing.
  - With no configuration file at all, it imports triib's (see
    "Migration") or exits 0. It never invents an entity, as
    first_config does now.

### One interface, one person

- One daemon per interface across people, claimed by the abstract
  socket `@avb-endpointd/<interface>`. A second person's daemon exits
  with its refusal status, saying whose holds the interface.
- While old versions can exist, the daemon binds both
  `@avb-endpointd/<interface>` and `@triib-endpointd/<interface>`. A new
  daemon then won't start beside an old triib-endpointd, and an old one
  won't start beside a new daemon. triib's `elsewhere()` checks both
  names.
- The per-person lock is per interface:
  `$XDG_RUNTIME_DIR/avb-endpointd/<interface>.lock`. Today's single lock
  per person would turn away a second instance of the template.

### Privileges, gPTP and the link

- **No capability on the daemon.** `avb-netraw` holds `cap_net_raw+ep`,
  set at install, and hands the daemon AF_PACKET sockets over a
  socketpair. It replaces plaa-netraw and triib-netraw, which run the
  same broker today. Both apps' controllers use it too.
- **Real-time:** RealtimeKit gives the stream threads round robin
  priority 15, as now.
- **gptpd:** the daemon reads `/run/gptpd/<interface>.sock` and
  `.time`. Where none answers, it starts `gptpd@<interface>.service`
  under gptpd's polkit rule, as both programs do now. polkit passes it
  from the user manager's session slice, as it does triib-endpointd now.
- **Link units:** `avb-link@<interface>.service` and
  `avb-link-reset@<interface>.service` replace plaa-link@, triib-link@
  and triib-link-reset@, with a polkit rule for the active local
  session.
  - At step 1, avb-link@ carries what triib-link@ carries then.
  - The transmit queue's discipline follows the launch-time decision.
    The bench found fq on the first queue sends frames late and out of
    order, and enp2s0 runs a stopgap.
  - The daemon retries the link unit after a refusal, as it retries
    gptpd's start. Today start_link runs once.
  - While the three names can be live, an active avb-, triib- or
    plaa-link@ on the interface counts as already run. Running it again
    would bounce a link whose EEE was on.

## Files

### Configuration

`$XDG_CONFIG_HOME/avb-endpointd/<interface>.toml`, one per interface.
It is triib's `entities.toml` with these changes:

- The interface comes from the file's name. `interface` and the restart
  when it changes go.
- Each entity names its owner and its model (below).
- Every entity carries an explicit `instance`. Today an entry without
  one takes its place in the list, so one app removing an entry above
  another's would change the other's entity ID. The daemon writes the
  instances in as it imports or first reads a file.

Rules for changing it:

- **Locking:** every read, change and write, by either app or the
  daemon, holds an flock on `<interface>.toml.lock` beside the file,
  and only for that edit. This is not the daemon's instance lock in
  `$XDG_RUNTIME_DIR`, which it holds for its whole life. The edit lock
  is a separate file because the write replaces the configuration
  file's inode. Without it, an app writing a stale copy loses another
  writer's change, and the daemon's own check-then-save in
  keep_settings isn't atomic.
- **Writes** are by atomic rename.
- **Change detection:** the daemon watches the folder with inotify,
  which catches writes within one timestamp tick that the mtime check
  misses. It keeps the two-second check as a fallback.
- It applies entity changes in place, as now.

### Status and meters

- **Status:** `$XDG_RUNTIME_DIR/avb-endpointd/<interface>.toml`,
  rewritten every two seconds, as triib's now:
  - the process, gptpd's state and the clock;
  - each entity and stream;
  - each stream's state as plaa shows it now: blocked, waiting for a
    listener or sending, with why when blocked. Blocked covers another
    person's daemon, no PTP clock and gptpd refused.
- **Meters:** `$XDG_RUNTIME_DIR/avb-endpointd/<interface>.meters`,
  written every 50 ms while an app touches `<interface>.meters-wanted`,
  as triib's now.

### MAAP and sharing

- **MAAP:** the daemon takes one block of 32 addresses for all its
  talkers, a slot per output, as now. It doesn't keep them across runs.
  Listeners take the destination from ACMP and MSRP.
- **Sharing:** the avb-mrp sharing socket,
  `$XDG_RUNTIME_DIR/avb-mrp-<interface>.sock`, is served as
  triib-endpointd serves it now, for any other program that reserves on
  the port.

```toml
# ~/.config/avb-endpointd/enp2s0.toml
avb_lite = "auto"

[[entity]]
instance = 112            # 0x70: plaa's entity ID, kept
owner = "plaa"
model = "plaa"
name = "plaa on linux-desktop"

[[entity.stream]]
direction = "out"
name = "Playback"
channels = 2
audio = "virtual"

[[entity.stream]]
direction = "in"
name = "Listen"
channels = 2
audio = "virtual"

[[entity]]
instance = 0
owner = "triib"
model = "triib"
name = "Host entity 1"
# streams as triib writes them now
```

### Owners

- Each app adds, changes and removes only the entities it owns. That
  includes restoring a preset: triib's restore replaces only triib's
  entities, not the whole list as now.
- The daemon's own write-back merges into the current file by instance,
  under the lock.
- An entity with no owner was made by hand, and both apps leave it
  alone.
- Each app shows every entity on its interface, so a person sees what
  runs, and marks the ones the other app owns as that app's.

### Models

Each entity's identity in AEM comes from a model file the owning app's
package installs, `/usr/share/avb-endpointd/models/<model>.toml`:

- the vendor and model names;
- the firmware version, from the app's package version;
- the logo images.

A model file gives no entity model ID. The daemon makes every one as it
makes triib's now, `DEVICE_MODELS` with a 24-bit hash whose bit 16 is
always set. The hash covers the streams, the daemon's own revision, and
the model file's names and logos:

- The model ID changes whenever what controllers cache under it does, as
  AEM asks: an edit to an entity's streams, or another model. Controllers
  cache an entity's descriptors by its model ID, so a pinned ID would go
  stale.
- The firmware version stays out of the hash. Controllers read it again
  at each enumeration, as la_avdecc (and so Hive) keeps it with the
  entity's dynamic state, so a new version shows without a new model ID.
  Hashed, every release, and every rebuild whose version names a commit,
  would make each controller read the whole model again for nothing.
- Two entities with the same streams but different models get different
  IDs, though both use the one family.
- All the daemon's model IDs stay in the one registered family, the
  values of the registry's entity model pool (`8C:1F:64:36:C0:xx:xx:xx`)
  with bit 16 set, which `is_device_model` checks. A base for each
  model would need a new pool, and would leave fewer bits for the hash.

Each identity moves out of the daemon's code and into its app's file:

- triib's: today's model name, LOGO and VENDOR_LOGO;
- plaa's: what plaa's entity shows now ("Scramble Tools", "plaa",
  plaa's and the vendor's logos).

plaa's entity therefore takes a new model ID, in place of
`0x8c1f6436c0200005`. The daemon serves its own descriptor tree for
it, which differs from plaa's own.rs, such as in media clock inputs as
clock sources and dynamic input mappings, so controllers must read it
afresh. That costs a read of the new model once. plaa's entity ID
(instance 0x70) stays, and so does every binding by entity ID and
stream index. Its stream ID follows the daemon's rule. When plaa moves
onto the daemon, it tells the ESP session that the model ID and the
stream ID change, and the registry retires plaa's model block.

A model the daemon can't find falls back to a generic one, and the
status says so.

### Entity IDs

The daemon derives IDs as triib-endpointd does:
`mac[0..3] | 0xff | instance | mac[3..6]`. plaa's entity is instance
0x70 today with the same rule, so it keeps its ID. Controllers' saved
bindings to it, the ESP bench's among them, go on working. Its stream
ID follows the daemon's rule. Listeners and controllers bind by entity ID
and stream index, not by stream ID.

## Audio devices

As triib-pipewire makes them now, moved to the avb repository as
`avb-pipewire`:

- **Nodes:** an `Audio/Sink` per stream output and an `Audio/Source`
  per stream input. Each is a follower node with F32 samples, AUX
  channel positions and `node.virtual = true`.
  - The description is "<entity name> <stream name>".
  - `node.name` becomes `avb-<entity id>-out-<n>` or `-in-<n>`, from
    today's `triib-` prefix. It is stable from then on, so WirePlumber
    returns an app's stream to its device after the daemon or PipeWire
    restarts.
  - Step 2 adds `node.network = true`.
- **Lifetime:** a device exists while its entity runs, whether or not a
  listener is connected, so an app can pick it at any time.
  - With no listener connected, what plays into a playback device is
    dropped past four times its margin.
  - With no talker bound, a recording device gives silence and counts
    an underrun.
  - A talker that starts drops what waited.
- **Clock:** the device runs on PipeWire's graph clock. The stream side
  converts to gPTP's clock and the stream's rate, the ratio steered by
  the buffer's fill, as now, with a 10 ms margin.
- **Rate changes, from step 2:**
  - Each device runs at one fixed rate, the graph's `default.clock.rate`
    (normally 48 kHz).
  - When a controller changes a stream's rate, the daemon restarts only
    that talker or listener, whose shared source or sink already
    converts rates. Apps playing into the device stay connected.
  - Today a rate change drops and recreates every device.
  - The conversion interpolates cubically with no low-pass filter. That
    is fine for 48 kHz up to 96, but would alias taking a 96 kHz device
    down to a 48 kHz stream. That is one more reason to keep the devices
    at the graph's rate.

## plaa on the daemon

On Linux, "Send what plays" in plaa's Network Audio settings now does
this:

- **The entity:** plaa adds or updates its entity in the interface's
  file, owned by "plaa", named from `avb_name` ("plaa on <host>"), with
  plaa's model. Its streams are a 2-channel output and a 2-channel
  input, both virtual, offering the formats plaa offers now. Then plaa
  starts and enables the unit.
- **The talker's audio:** the copy of the media window, which plaa now
  pushes into its in-process buffer, plaa plays into its entity's
  playback device as a second output stream. Other apps can play into
  the same device, and PipeWire mixes them.
- **The input:** plaa records from its entity's recording device into a
  player window, as `Listened` does now. Pausing the window unbinds the
  input as now, through plaa's controller.
- **Status:** plaa reads the status file for its entity's state, gptpd's
  notes and failures. It shows them where it shows its own now.
- **Turning it off:** plaa removes its entity. With no entities left the
  daemon exits, and plaa disables the unit.

What leaves plaa on Linux, as the in-process entity goes (see
"Decided in review"):

- the entity: own.rs, own/lite.rs, sound.rs and reservation.rs;
- its MRP participants and the sharing client;
- plaa-netraw, which avb-netraw replaces;
- plaa-link@ and the gptpd starting logic.

plaa's controller stays, with its controller ID (0xfc in the instance
octet). It finds its own entity by entity ID.

On a Mac nothing changes: plaa plays into the Mac's own entity as now.
On Windows there is no entity, as decided.

## triib on the daemon

- host.rs writes the interface's file under the lock, owning its
  entities as "triib", and starts the unit instead of spawning
  triib-endpointd. It reads status and meters at the new paths.
- **What goes:**
  - triib's "entities file" setting and move_entities, since the path
    is fixed per interface;
  - triib-endpointd, triib-pipewire, triib-netraw, triib-link@ and
    triib-link-reset@;
  - the spawn: process_group(0), the reaping thread and the log file.
- **What stays:** what triib's window offers for its entities.

## Migration

- **triib's entities:** the first time a person's daemon finds no file
  for an interface, it imports `$XDG_DATA_HOME/triib/entities.toml` for
  that interface. It marks the entities as triib's and pins their
  instances.
- **plaa's entity:** the first time plaa finds the daemon, it adds its
  entity from its settings (`avb_interface`, `avb_talker`, `avb_name`).
- **Device names:** the devices' names change from `triib-` to `avb-`,
  so WirePlumber won't return apps' streams to them the first time. An
  app that played into a triib device picks the new one again once.
- **Old processes:** the daemon binds both claim names while old
  versions can exist (see "One interface, one person"). plaa's entity
  steps aside from MRP on its own MAC as now, so an old plaa and a new
  daemon don't fight.

## The data path

The stream threads stay in the daemon, with no second daemon for them
(avb-streamd is shelved; `streamd.md` says why). Two later steps make
their timing firmer:

- **Locked pages:** the daemon locks (mlock) and touches ahead of time
  only what the stream threads use, their stacks, rings and frame
  buffers, so a page fault never holds one up. That stays under the
  8 MiB a user unit may lock (systemd's DefaultLimitMEMLOCK, 8388608).
  The package sets `vm.compact_unevictable_allowed = 0`, so compaction
  leaves locked pages alone. This waits on the pages being counted.
- **Launch time** (`launch-time.md`), once gptpd's follow-tai keeps the
  PTP hardware clock near CLOCK_TAI: the link units, as root, put mqprio
  and ETF on the interface, and avb-netraw sets SO_TXTIME with
  CLOCK_TAI on a stream's socket before passing it over, as a clock
  other than CLOCK_MONOTONIC takes CAP_NET_ADMIN. Giving avb-netraw
  `cap_net_admin` is the user's decision.

The link units stay as they are.

## Packaging

- **The avb repository** gains:
  - `crates/avb-endpointd`, the library on crates.io and the daemon
    behind its feature, and `crates/avb-pipewire` (from triib-endpointd
    and triib-pipewire);
  - the `avb-netraw` helper;
  - the user unit, the link units and their polkit rule;
  - Arch, Debian and Fedora packages, released as gptpd is.
- **Dependencies:** the package depends on gptpd, rtkit and PipeWire. It
  sets avb-netraw's capability as it installs.
- **What installing says:**
  - a user service runs this computer's AVB entities when an app turns
    one on, and at each desktop login after;
  - a helper may open raw network sockets;
  - starting the link unit changes the interface's transmit settings,
    EEE and PAUSE, and the link reconnects once if EEE or PAUSE were on.
- **plaa and triib** each install their model files and depend on
  avb-endpointd, as plaa's AVB on Linux already needs gptpd.

## Testing

- **Unit:**
  - two owners and the daemon changing one file under the lock;
  - instances pinned on import;
  - a missing model;
  - model IDs: the same streams under two models, and an edit to the
    streams, each give a new ID, and a new firmware version doesn't;
  - importing triib's file;
  - stable node names.
- **One host:**
  - plaa and triib each own an entity on one daemon;
  - pw-play or mpv plays into plaa's device, and a listener on a veth
    pair receives it;
  - two interfaces with two instances of the template;
  - the daemon is killed: systemd restarts it, the devices come back,
    and WirePlumber returns the app's stream;
  - PipeWire is restarted: the daemon restarts with it;
  - a refusal (another person's claim) doesn't loop;
  - an SSH-only login doesn't start it;
  - the person logs out: entities depart, with ADP's departing and
    MSRP's leave.
- **Bench, with the ESP session:** esp-synth listens to plaa's entity
  through the daemon while mpv plays into its device, for three hours.
  Compare frames late, starves and gaps with plaa's in-process numbers.
- **Old with new:** an old plaa with the daemon, an old triib-endpointd
  (another person's) beside the daemon, and triib before and after its
  move.

## Plan

1. **Move without changing behaviour.** In the avb repository:
   - triib-endpointd and triib-pipewire become avb-endpointd (library
     and daemon) and avb-pipewire;
   - avb-netraw, the user unit template and the link units;
   - the step 1 changes above: the lock per interface, the interface
     from `%i`, no invented first entity, both claim names, restart
     statuses, the link retry, and the three link names.

   triib then moves onto it, in triib's session, when the user asks.
2. **Owners, models, pinned instances, the file lock and inotify.** Also
   devices at a fixed rate, `node.network`, and migration.
3. **plaa onto it.** The copy of what plays goes into the device, and
   the input is recorded from its device. The in-process entity and its
   MRP code come out on Linux. plaa's README and PLAN catch up: the
   PLAN's AVB section is already behind the code.
4. **Packages, releases and the bench run.**

## Decided in review

- One daemon per interface, as a user unit template. This matches the
  claim, the entity IDs taken from the MAC, gptpd@ and the link units.
- The configuration file stays the apps' interface, with the lock,
  pinned instances and inotify. There is no control socket yet: the
  daemon would still keep a file, and the status file reports outcomes.
  Revisit when an app needs an answer at once.
- An app enables the unit when it adds an entity. A package enabling it
  for every person would run AVB for accounts that never asked.
- The configuration lives in `$XDG_CONFIG_HOME/avb-endpointd/`
  (`~/.config`), not triib's data folder, as a file a person may edit
  or back up (the user, 2026-11-08).
- avb-streamd is shelved: the streams' data path stays in avb-endpointd,
  with locked pages and launch time as later steps (the user,
  2026-11-08).
- plaa on Linux runs its entity only through avb-endpointd, and its
  packages depend on it (the user, 2026-11-08). The in-process entity,
  its MRP sharing and step-aside code, and plaa-netraw go.
  - This supersedes the 2026-10-21 ruling's preference for shared code
    that each program runs in its own process.
  - Its point stands: plaa needs a neutral avb package, not triib.
