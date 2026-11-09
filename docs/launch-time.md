# Launch time

Status: design. It comes through avb-endpointd (endpointd.md). The
separate stream service, avb-streamd (streamd.md), is shelved.

## Why

A talker thread sends each frame when it is due. When the thread is kept
off the CPU, by a busy graphics driver, memory reclaim or anything else
the scheduler runs first, the frame goes out late: on the bench a
talker ready to run waited 1 to 5 ms for a CPU while the desktop was in
use, and a listener with 1 ms of margin ran dry. Locking the stream
threads' memory takes away the waits on pages, not these.

With launch time the thread hands each frame over a little ahead of its
time, stamped with the time it should leave, and the kernel sends it
then. A thread that wakes late, by less than how far ahead it works,
still has every frame leave on time.

## How

- The kernel's fq queue discipline holds a frame until the departure
  time on it (earliest departure time, as TCP's pacing uses), from a
  timer of its own that fires in interrupt context rather than waiting
  for a thread to be scheduled. fq is the default discipline on many
  distributions now, and the link units make sure the stream's queue has
  it.
- The talker's socket asks for departure times on CLOCK_MONOTONIC
  (SO_TXTIME), which needs no privilege, and each frame carries its time
  (SCM_TXTIME): the monotonic time of its slot, the same moment the
  thread sleeps until today.
- The talker wakes `ahead` before a frame's slot instead of at it, and
  sends it then. Frames keep their presentation times; only the moment
  the thread hands them over moves.
- It does so only where the queue holds frames until their times:
  avb-net's `paced_frames(interface)` asks rtnetlink for the interface's
  queue disciplines, without privilege, and says how many frames the
  first transmit queue's fq may hold (its limit, 10000 by default) or
  etf (no limit), or none where a stamped frame would leave at once. The
  talker looks as it starts, and keeps its head start short enough that
  the frames it holds, every stream's and each listener's copy, fill no
  more than half of that.
- fq takes a packet socket's frames each as a flow of its own, as it
  does any unconnected socket's (found on the bench: 16 throttled flows
  for one stream 2 ms ahead), so its per-flow limit never binds, and
  the frames leave in the order of their times, not of a flow's queue.
- A frame handed over after its time has passed is stamped 20 us past
  the moment instead: fq would send a frame already due at once, as a
  new flow, ahead of earlier frames it is just then releasing.
- Its socket's priority moves from 3 to 6. fq's bands take turns by
  bytes, weighted 589824, 196608 and 65536 by default, rather than one
  always first, and its default priority map puts 3 in the last band,
  where streams sit today, and 0 (ATDECC, MRP, and gPTP unless it sets
  one) in the middle one. 6 goes in the first, the most favored. Frames
  held for their time are not ready, so only frames due compete. Six is
  the highest priority a program may set without privilege. The VLAN
  tag's priority code point, which the network sees, is written in the
  frame and does not change.
- A frame still due too late to arrive in time is left out, as now. One
  merely late by less than `ahead` goes out on time.
- A frame fq has no room for comes back as ENOBUFS from the send, which
  the talker counts as a send error, as it counts any other.

`ahead` defaults to 2 ms: it covers most of the waits seen on the bench,
and a source's samples are taken that much earlier, which adds 2 ms to a
live input's latency. A talker may choose another, or none for today's
behavior.

## The queue

- The link units (plaa-link@ and triib-link@ now, avb-link@ with avb-endpointd)
  give the interface's first transmit queue, the one every CPU's frames
  go to, its own fq: `tc qdisc replace dev IF root handle 1: mq`, then
  `tc qdisc replace dev IF parent 1:1 fq flow_limit 2000`. An interface
  with one transmit queue, as many Realtek and USB adapters have, has no
  mq: `tc qdisc replace dev IF root fq flow_limit 2000`.
- Only where the queue is not like that already, compared exactly (kind,
  parent and flow_limit): replacing the root drops whatever every queue
  holds, which would glitch a stream another program sends on the same
  interface each time a unit runs.
- flow_limit 2000 covers a kernel that would take a packet socket's
  frames as one flow: one socket carries a talker group's streams,
  `ahead` of each, and in AVB Lite a unicast copy of each for every
  listener, 8 streams at 8000 frames a second, 2 ms ahead, with 4
  listeners each, being 512 frames, over fq's default of 100 a flow.
- Other queues keep the system's default discipline.
- A driver starting again resets the queue discipline with EEE, PAUSE,
  XPS and the byte queue floor; the programs run their unit again when
  they notice (triib-endpointd checks the link periodically, and can
  check the queue's kind with it). One changed by hand or by a tuning
  daemon goes unnoticed until then.
- If the tap shows frames released in bunches, fq's timer slack, 10 us
  by default, goes lower on the same line.
- Packages depend on iproute2 (iproute on Fedora), for `tc`, which the
  units already use for `ip`.

## What has to be checked first, on this kernel

- that a packet socket's SCM_TXTIME reaches fq as the departure time;
- that fq releases frames on time while the talker's CPU is busy, and
  how far from their time they leave, at the tap, against today's sends;
- that the frames of other flows on the queue (gPTP, ATDECC, MRP) are
  not held up behind stream frames waiting for their time: gPTP's peer
  delay turnaround with 8 streams sending.

## Bench result, 2026-11-07

Through fq on a desktop, one stream 2 ms ahead, its listener's tap (UTC,
lead the time before presentation, then gaps over 500 us, reorders):

| window              | lead min | gaps | reorders |
|---------------------|----------|------|----------|
| 21:23:32-21:24:36   | 1758 us  | 0    | 0        |
| 21:24:36-21:26:46   | 1168 us  | 1    | 0        |
| 21:26:46-21:28:56   | 1431 us  | 2    | 0        |
| 21:28:56-21:31:05   | 642 us   | 459  | 64       |
| 21:31:05-21:33:15   | 404 us   | 688  | 114      |
| 21:33:15-21:35:25   | -312 us  | 247  | 48       |
| 21:35:25-21:37:35   | 1713 us  | 0    | 0        |

While it held, a talker kept off its CPU for 1.8 and 2.6 ms in a second
left no trace on the wire. Then for about 6.5 minutes frames left late,
some past their presentation time, and adjacent ones swapped. fq
releases a held frame from its timer, through the NET_TX softirq, and a
busy CPU's softirq work falls to ksoftirqd, a normal thread waiting
behind the desktop; frames then leave late and together, and fq sends
one stamped with a time already past, a flow it has just made, before
one it is just releasing. A release in software is only as timely as
the CPU's softirqs. The queue went back to fq_codel, which ignores the
stamps, so frames leave as handed over, 2 ms early and in order, and the
listener holds them 2 ms longer.

## In the card (next)

Milan allows no more than 2 ms from send to presentation, so a frame
must leave at its slot, not early, and software can't promise that on a
busy desktop. The Intel I210, and the i225 and i226 after it, can hold a
frame on the card and send it when its PTP hardware clock reaches the
frame's launch time (LaunchTime, on transmit queues 0 and 1), which no
load on the CPU delays.

- The ETF queue discipline with offload turns a frame's SCM_TXTIME into
  the card's launch time. It takes CLOCK_TAI times: it hands a frame to
  the card `delta` before its time on CLOCK_TAI, drops one whose time
  has passed and says so on the socket's error queue. The driver writes
  the time's nanoseconds into the frame's descriptor, and the card
  compares them with its own clock.
- So a frame's time must be the card's clock's time of its slot, which
  gptpd's record already gives (the grandmaster's time as a line against
  the port's hardware clock), and the card's clock must stay near
  CLOCK_TAI, well within `delta`, for ETF to hand frames over in time.
  gptpd lets the clock run free and takes off any adjustment as it
  starts, so keeping it near CLOCK_TAI is gptpd's to do: slewing its
  frequency, never stepping it but at a start or after a reset.
- gptpd keeps its whole model, Pdelay, its lines, its jump check and
  generations, on a virtual free clock: the hardware clock less the
  integral of the frequency it has commanded, which it knows to well
  under a microsecond. A plain servo would trip its jump check (1 us
  over a 100 ms comparison) and bend its lines. Only the record goes
  back to the real, steered clock, which the card launches by. Each
  change is small (about 0.1 ppm), made right after a comparison, so a
  talker carrying a record up to 100 ms old is off by under 10 ns.
  Neighbors see the rate move slowly, as beside any ptp4l with phc2sys.
- PTP_SYS_OFFSET_EXTENDED reads the clock against CLOCK_TAI to tens of
  ns (PRECISE on i225 and i226 where the platform has PTM), so a slow
  loop holds it well within a microsecond. CLOCK_TAI itself steps: with
  the wall clock (an NTP step at boot, a manual set) and by 37 s if
  chrony sets the kernel's TAI offset later. The record says whether the
  clock follows CLOCK_TAI, and within what; on a step gptpd drops that,
  talkers send when due meanwhile, and the card's clock steps only with
  a new generation. While gptpd is grandmaster its time is the card's
  clock, so steering it steers the domain: then it follows CLOCK_TAI
  only very slowly, or not at all.
- The record's new field is a format change (gptp::time's VERSION and
  RECORD_LEN), so readers and gptp's release move with it.
- On igc, turning on any TSN offload (ETF, CBS) switches the card to its
  TSN mode and resets the port, link and clock, so the queue is set up
  before the link comes up. Whether igb resets an I210 the same way is
  to be checked.
- ETF drops frames without a time, so it gets a queue of the streams'
  own: mqprio maps the streams' priorities to queue 0, with ETF offload
  on it, and the rest of the traffic to the other queues.
- A CLOCK_TAI time on SO_TXTIME needs CAP_NET_ADMIN: the helper that
  opens the streams' sockets (plaa-netraw, triib-netraw, or avb-netraw
  with avb-endpointd) sets it before passing the socket on.
- The talker hands each frame over about 1 ms ahead, stamped with its
  slot's time on the card's clock; ETF drops one already late, which the
  talker counts as left out.
- With no gptpd keeping the clock near CLOCK_TAI, or a card without
  LaunchTime, frames go out as today, when due.

## Later: in the card (as first noted)

The Intel I210 and newer cards can send a frame at a time to the
nanosecond themselves (LaunchTime), through the ETF queue discipline
with offload. That needs the card's PTP clock to follow CLOCK_TAI, which
gptpd does not steer, and a queue of the stream's own, as ETF drops
frames without a time. It would take the release off the CPU entirely,
and needs gptpd's side settled first.
