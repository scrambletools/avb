# avb

Rust crates for AVB and Milan: discovering, describing and connecting
IEEE 1722.1 entities, reserving streams, raw Ethernet on Linux, macOS and
Windows, and sending and receiving AVTP audio on gPTP time. They are the
networking under [triib](https://github.com/scrambletoolsllc/triib), an
ATDECC controller, and [plaa](https://github.com/scrambletools/plaa), a
media player, and are meant for other apps with
very different constraints too: a blocking command line tool, an async
daemon, a GUI, or firmware on a microcontroller.

| Crate | What it does |
|---|---|
| `atdecc` | IEEE 1722.1-2021 with Milan 1.3, MAAP and AVB Lite's vendor unique messages: frame codecs, controller state machines (discovery, AECP commands, enumeration, ACMP, unsolicited notifications), entity state machines (AEM responder, Milan listener binding, ACMP talker) and the entity model as data |
| `avb-mrp` | IEEE 802.1Q MRP (applicant, registrar, LeaveAll, periodic timers), MSRP and MVRP, and a port's participant shared between programs on one computer |
| `avb-net` | Raw Ethernet per operating system: interfaces, frames, multicast membership, the PTP hardware clock, link settings |
| `avb-stream` | AVTP audio: AAF and AM824 talkers and listeners paced on gPTP time, CRF media clock streams, and audio to and from the computer's devices through cpal |

## Rules for the protocol crates

`atdecc`, `avb-mrp` and `avb-net` keep to these:

1. **Sans-I/O core.** No sockets, threads, clock or runtime in the protocol
   code. The caller feeds received frames and the current time
   (`handle_frame(now, bytes)`, `handle_timeout(now)`) and drains frames
   to send, events and the next deadline (`poll_transmit`, `poll_event`,
   `poll_timeout`). Commands return an ID and complete as an event. This
   fits a blocking loop, tokio, embassy or a firmware main loop alike.
2. **`no_std`.** Codecs need neither std nor alloc: fixed-size PDUs decode
   into plain values, variable ones as views over the received bytes, and
   both encode into the caller's buffer. Reserved values are kept, so a
   frame decodes and encodes back unchanged. State machines need `alloc`
   only. The `std` feature (default) adds the `avb-net` transports and a
   small blocking driver for simple apps.
3. **No dependencies beyond each other.** `avb-net` depends only on what
   its platform needs (`libc` on Linux and macOS, `windows-sys` on
   Windows). No serde, no logging framework, no async runtime.
4. **Never panic on input.** Malformed frames are errors.
5. **Checked in CI**: each crate tested on its own, and built with
   `--no-default-features` for `riscv32imac-unknown-none-elf` (the
   ESP32-C6).

`avb-stream` is for computers: it needs std, threads and an audio device.

## Using them

They are not on crates.io yet. Depend on this repository, pinned by
revision:

```toml
[dependencies]
atdecc = { git = "https://github.com/scrambletools/avb", rev = "<commit>" }
```

To change them together with an app that uses them, point the app at a
local checkout from its own git-ignored `.cargo/config.toml`:

```toml
[patch."https://github.com/scrambletools/avb"]
atdecc = { path = "../avb/crates/atdecc" }
avb-mrp = { path = "../avb/crates/avb-mrp" }
avb-net = { path = "../avb/crates/avb-net" }
avb-stream = { path = "../avb/crates/avb-stream" }
```

## Platform needs

- **Linux:** raw Ethernet needs `CAP_NET_RAW`, but only to open a
  socket. Rather than granting it to the app, which then runs as a
  process others may not look into (the desktop portals refuse it file
  dialogs and screen sharing; debuggers and crash reports pass it by),
  ship a tiny helper whose `main` calls `avb_net::broker::serve()`, grant
  the capability to that alone (`sudo setcap cap_net_raw+ep <helper>`),
  and at start `install` a `Broker::start(<helper>)`: every raw socket
  `avb-net` and `avb-stream` open then comes from the helper, passed over
  a Unix socket. The `netraw` example is such a helper. gPTP comes from
  ptp4l; the stream crates read its time from the interface's PTP
  hardware clock, which must be readable.
- **macOS:** raw Ethernet needs read and write access to `/dev/bpf*`.
- **Windows:** raw Ethernet needs [Npcap](https://npcap.com).

## Building

```sh
cargo test --workspace
cargo build -p avb-net -p atdecc -p avb-mrp --no-default-features --target riscv32imac-unknown-none-elf
```

On Linux, `avb-stream` builds against ALSA's headers (`libasound2-dev`,
`alsa-lib`).

## License

MIT or Apache-2.0, at your option.
