# avb-streamd: one stream service per interface

Status: shelved (the user, 2026-11-08). Nothing is built. avb-endpointd
(endpointd.md) keeps the streams' data path in its own process instead.

## Why it is shelved

This design came before avb-endpointd. Its reasons, and where each went:

- **Two senders on one queue:** plaa and triib each ran a paced sender
  on the interface's first transmit queue. avb-endpointd runs every
  entity on an interface for the person at the computer in one process,
  with one sender, so this is gone without a second service.
- **Waits on pages:** these move into avb-endpointd as a later step,
  without a service of their own:
  - it locks and touches ahead of time only what the stream threads
    use: their stacks, rings and frame buffers. That fits under the
    8 MiB a user's processes may lock by default, where locking all of
    the daemon's memory would not;
  - the avb package installs the compaction setting below, so the
    kernel leaves locked pages where they are;
  - since Linux 6.4 most page faults take a lock on their own mapping
    rather than the whole process's map, so another thread mapping
    memory no longer holds them up.
- **Frames late while the sender waits for a CPU:** neither a service
  nor locking helps here. The card's launch time does (launch-time.md),
  through avb-endpointd once gptpd keeps the card's clock near
  CLOCK_TAI:
  - the link units, run as root, put the queue in place;
  - avb-netraw sets SO_TXTIME on CLOCK_TAI, which needs CAP_NET_ADMIN,
    before passing the socket on. Giving avb-netraw that capability is
    the user's decision.

What a service would add, and what it would cost:

- **Adds:** realtime priority without RealtimeKit's limits, and its
  memory locked whole.
- **Costs:** a second protocol and a root system service with a polkit
  action of its own; rings shared between processes; another package;
  two processes that can each fail.

The lock-free ring (crates/avb-stream/src/ring.rs, not yet built into
the crate) may still replace FrameBuffer inside one process, as step 1
of the plan below has it, if the bench shows the stream threads waiting
on the audio threads.

Revisit this design if, with avb-endpointd's locking and launch time in
place, the bench still shows the stream threads waiting on pages, or if
more than one person needs streams on one interface.

The design as it stood follows.

## Why

Today each program that sends or receives AVB streams, plaa and
triib-endpointd, runs avb-stream's talker and listener threads inside
its own process. Those threads share that process's memory with
everything else it does, so a send can stall on things that have nothing
to do with streaming:

- a page the kernel is moving to make room (compaction), which the
  thread touches and then waits for. On the bench a talker waited about
  1.1 ms on one, sent a frame 1.04 ms late, and the listener ran dry;
- the process's memory map held by another thread or by the kernel
  merging its memory into huge pages;
- a page swapped out.

Two programs on one interface also each run their own paced sender, at
the same priority, on the same transmit queue.

A service per interface takes the streams' data path out of the
programs: one process that locks all of its memory, runs its sender at
realtime priority, owns the interface's transmit settings and, later,
its launch-time queue.

## What it does, and what it leaves to the programs

It does:

- send and receive AVTP stream frames (AAF, AM824, CRF) on one
  interface, paced on gPTP time from gptpd's record, as avb-stream does
  now;
- recover a stream's media clock and have talkers follow it;
- keep each stream's counters and audio peaks;
- set the interface up for streams: every CPU on the first transmit
  queue, a byte queue floor, EEE and PAUSE off. That is what plaa-link@
  and triib-link@ do today.

It leaves to the programs:

- AVDECC, MSRP, MVRP and MAAP. plaa's entity and triib-endpointd keep
  them, and keep sharing one MRP participant as they do now;
- audio devices. The service never opens a person's audio session; the
  programs keep cpal and PipeWire and move samples to and from it;
- gPTP, which stays gptpd's.

## Shape

- `avb-streamd@<interface>.service`, a system service per interface,
  from the avb repository and released as its own package, as gptpd is
  from gptp's.
- Socket activated: `avb-streamd@.socket` listens on
  `/run/avb-streamd/<interface>.sock` (SOCK_SEQPACKET). The first
  program to connect starts it, so no polkit rule for starting it is
  needed. It exits a minute after its last client leaves.
- Root with a bounded set of capabilities, as gptpd runs: CAP_NET_RAW
  for its sockets, CAP_NET_ADMIN for the link and a launch-time queue,
  CAP_IPC_LOCK, CAP_SYS_NICE. NoNewPrivileges, ProtectSystem=strict,
  ProtectHome, PrivateTmp, RestrictAddressFamilies=AF_UNIX AF_PACKET
  AF_NETLINK.
- `mlockall(MCL_CURRENT | MCL_FUTURE)` as it starts, with
  LimitMEMLOCK=infinity, stacks touched ahead of time, and nothing
  allocated while sending.
- Its sender and receivers run round robin at priority 15, set by the
  service itself, the priority the streams' threads ask RealtimeKit for
  today: above gptpd (FIFO 10), below PipeWire's own threads (20).
- Its package installs `/usr/lib/sysctl.d/60-avb-streamd.conf` with
  `vm.compact_unevictable_allowed = 0`, so compaction leaves locked
  pages where they are. Without it the kernel moves even locked pages,
  and locking would not stop the stall above. Kernels built for
  realtime already default to 0.

## Who may use it

- A polkit action, `org.scrambletools.avb-streamd.stream`, allowed for
  the person at the computer (allow_active yes, any and inactive no),
  the same people gptpd's and the link units' rules let in. The service
  checks it for the peer's process (SO_PEERCRED and a pidfd) when a
  client says hello.
- Programs never hand it raw frames. They describe a stream and the
  service builds every frame itself: AVTP only, from the interface's own
  address, to the destinations given (a multicast address, or in AVB
  Lite each listener's unicast one), on the VLAN and priority given.

## Protocol

Binary records on the control socket, each a kind and a length, little
endian. A side skips kinds it does not know, so either can grow.

Streams hang off two objects the client opens first, as triib-endpointd
already arranges them:

- a clock: a media clock recovered from a stream. It lives as long as
  the client keeps it, not as long as one listener, so a talker can go
  on following an input's clock across that input's listener restarting,
  and on gPTP's time while the clock is not recovered;
- a timeline: a rate and the clock it follows, none for gPTP's. Every
  talker stream on one timeline is counted on it, so they present
  together sample for sample: an entity's two 8-channel outputs stay
  aligned, and its CRF stream's timestamps fall on its audio streams'
  samples. Following another clock restarts every stream on the timeline
  together, as one media reset.

Records:

- Hello carries a version and feature bits; the service answers Welcome
  with its own, or Refused with why (not allowed, unknown version).
  Features in version 1: TALKER, LISTENER, RECOVERED_CLOCK, LINK_SETUP.
  LAUNCH_TIME comes later.
- OpenClock, answered with its ID. Its recovered rate (fit's ppm) and
  whether it is recovered sit on its stats page.
- OpenTimeline: rate, and the clock it follows or none. Answered with
  its ID. Follow changes the clock a timeline follows.
- OpenTalker: the timeline and a place on it, stream ID, destinations,
  VLAN, priority, format, channels, max transit time, and its source: a
  ring the client passes, a tone or silence. Places count within a
  timeline, so a client running several entities on one interface, as
  triib-endpointd does, numbers each entity's streams from 0.
- OpenListener: stream ID, destinations, format, channel map, the ring
  it fills, and the clock it recovers into, if any.
- Each Open is answered with Opened and the stream's stats page.
  SetDestinations, Close and CloseClock or CloseTimeline do what they
  say.
- From the service: Stopped with why, and media and clock resets, so a
  program can tell its controllers.

The service sends every timeline's streams, from every client, on one
thread, earliest due first: one realtime sender on one transmit queue,
which a launch-time queue later needs too, as it wants each queue's send
times in order. Timelines on gPTP's clock and timelines following a
recovered clock go out side by side.

A client that goes away, its socket closed, takes its streams, timelines
and clocks with it at once, as its MRP declarations go with it. If the
service stops, clients see the socket close, connect again and open
everything again, which listeners see as a media reset.

## Audio rings

- The client makes each ring, a memfd sealed against shrinking and
  growing, and passes it in OpenTalker or OpenListener. The service
  checks its seals and size, then maps and locks it. A ring outlives its
  streams: an audio device's buffer, made with the device, stays the
  same ring while its talker stops for want of a listener or restarts
  for a new format, so the device's callback never switches rings.
- Samples: interleaved f32 frames, written by one side and read by the
  other, with no lock spanning the two processes, so a client that
  stalls or dies mid-write never holds the sender up. They mean what
  avb-stream's FrameBuffer means now: the program's side runs on its
  audio device's clock, and the service resamples to gPTP's (talker) or
  from it (listener), the ratio steered by the ring's fill.
- A header beside the samples carries what FrameBuffer's steering reads
  now. The device side writes its telemetry under a sequence lock: the
  chunks it has moved, the fill after each, their size, the frames it
  pushed, and when the device plays (CLOCK_MONOTONIC, the same in both
  processes), so a listener's playout stays aligned as
  ListenerStats::playout measures it. Only the reader moves the read
  index, so the steps that drop frames (drop the oldest, hold more, and
  drop when the ring holds four times its margin) are requests the
  writer raises and the reader carries out.
- Depth comes from the client's largest chunk, as an audio server's
  chunk may be 8192 frames (170 ms at 48 kHz), plus the margin kept now.
  The steering target stays where it is today, so a deep ring adds no
  latency.
- Each stream's stats page holds the fields of TalkerStats or
  ListenerStats as atomics, the realtime flag and the listener's playout
  pair among them, and its audio peaks. The client may write one word in
  it, the peaks it has taken, which the service answers by starting them
  afresh, as take_peaks does now. A clock's page holds its ppm and
  whether it is recovered.

## In the programs

- avb-stream gains a client with the shapes it has now (start,
  set_destinations, follow, stats, take_peaks), so plaa's own.rs and
  host.rs and triib-endpointd's runtime.rs change little. It picks the
  service when one answers with a version it knows, and its own threads
  otherwise.
- Device sources and sinks: the program opens the device as now and
  moves samples between its callback and the ring. Shared buffers bridge
  to the ring the same way. Tones and silence also run in the service
  as a stream's source. A program's own tone and analysis threads, as
  triib-endpointd's are, write and read rings like any device, and a
  stall there under- or overruns only their own ring.
- Without the service, or with one too old, a program runs its streams
  itself as today and says so, in Network Audio for plaa and in triib's
  window, since reliability falls back to today's.
- plaa-netraw still opens plaa's raw sockets for AVDECC and MRP; only
  the streams' sockets move to the service.

## Link setup and launch time

- Once the service sets the link up as it starts, and again when the
  driver restarts, programs that reach it no longer run plaa-link@ or
  triib-link@. Those stay for running without the service.
- Launch time, later: the service owns the transmit queue, so it can put
  an ETF queue discipline on it and send each frame with SO_TXTIME at
  its due time. The card then sends it on time even when the sender
  woke late, up to the frame's lead. That covers the stalls the service
  alone does not: a sender ready to run but kept off the CPU by memory
  reclaim or the graphics driver.

## Packaging

- avb repository: crates/avb-streamd (the program), the two units, the
  polkit action and rule, the sysctl file, Arch, Debian and Fedora
  packages, released with debs and rpms as gptpd is.
- plaa's packages and triib's debs and rpms recommend it, so a root
  service and a system-wide setting stay off computers that never
  stream, and each program falls back and says so. triib's Arch package
  bundles it as it bundles gptpd, which makes it a dependency there, and
  its install note says so.
- Installing says what it changes and why: a service per interface that
  starts when a program connects, locks its memory and sends at realtime
  priority; the compaction setting; and that the interface's queue,
  EEE and PAUSE settings change when it starts, the link reconnecting
  once if EEE or PAUSE were on.

## Versions

- avb-stream 0.4: the ring, clocks and timelines as objects, several
  timelines in one sender, and the client.
- avb-streamd 0.1: the service, protocol version 1.

## Testing

- Unit: the protocol's records both ways and skipped unknown kinds; the
  ring under concurrent writer and reader; the resampler as now.
- One host: the service with two clients, one shaped like plaa's and
  one like triib-endpointd's, on a veth pair.
- Bench: plaa's stream to a listener through the service, against
  today's in-process numbers over a loaded stretch: frames late, the
  sender's run delay, and page waits, which should be none.
- Failures: a client killed mid-stream, the service killed and
  restarted, an old client with a new service, and a new client with an
  old service or none.

## Plan

1. avb-stream, still in-process: the lock-free ring and its header
   between audio and stream threads, and clocks and timelines as objects,
   several timelines in one sender. Bench it.
2. avb-streamd: socket, protocol, polkit check, rings, locking, units,
   sysctl, link setup.
3. The client and its fallback in avb-stream; plaa, then
   triib-endpointd, onto it.
4. Packages and install notes, bench runs, releases.

## Decided in review

- Root with a bounded set of capabilities, as gptpd runs. The transmit
  queue's sysfs files are root's, and a dynamic user would also need
  CAP_DAC_OVERRIDE, which grants more.
- One sender per interface, with timelines inside it, as above.
- macOS keeps streams in-process for now: it compresses pages rather
  than moving them, and has no systemd.
- plaa-link@ and triib-link@ stay as long as a program can run without
  the service.
- plaa's and triib's packages recommend the service rather than depend
  on it.

## Open questions

1. Recommended, the service is not on every computer that streams, so
   the in-process path and the link units stay. Revisit once the service
   has run the bench for a while.


