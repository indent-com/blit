# `blit-guest`

`blit-guest` is the Rust SDK for Blit Wasmi extensions. It wraps the five
`blit_v1` imports, performs the normal Blit handshake followed by the private
`EXT_INFO(INIT)` bootstrap, reassembles bounded `S2C_FRAGMENT` sequences, and
only then calls extension code.

```ignore
fn extension(mut blit: blit_guest::Client) -> Result<(), blit_guest::Error> {
    let context = blit.context();
    let _identity = (context.extension_id, context.attempt, &context.args);
    blit.send(&[blit_guest::remote::C2S_PING])?;
    Ok(())
}

blit_guest::entry!(extension);
```

The default `protocol` feature re-exports `blit-remote` as
`blit_guest::remote`, so guests can use every current low-level packet codec.
`Client::bootstrap` consumes the normal initial-state burst without retaining
it; use `Client::bootstrap_with_initial` to fold those packets into application
state one at a time. This keeps bootstrap bounded even when one legal logical
packet reaches the 64 MiB maximum.
It also enables typed native channels and `blit.cli.v1` command providers.
`Client::listen_channel` and `Client::connect_channel` retain message
boundaries and enforce send credit. Channel DATA is returned as one pending
delivery receipt; inspect its payload, then pass it to `Channel::consume` or
`Channel::discard` to send the cumulative ACK. A second receive is rejected
while a receipt is pending, and `Channel::discard_pending` recovers after a
dropped receipt. `CommandProvider` performs this housekeeping after decoding
`INVOKE` and stdin messages. See `examples/command_provider.rs` for a complete
serial provider.

Typed terminal subscriptions keep the connection-global terminal frame window
moving without acknowledging data before it is consumed:

```ignore
let mut terminals = client.terminal_subscriptions();
terminals.subscribe(&mut client, pty_id, 24, 80)?;

let update = terminals.next_update(&mut client)?; // no ACK yet
terminals.apply_update(&mut client, update)?;      // apply, then one ACK
let text = terminals.subscription(pty_id).unwrap().state().get_all_text();
```

Use one `TerminalSubscriptions` value for every typed PTY on a client because
`C2S_ACK` retires the oldest frame across the whole connection. An update may
instead be passed to `discard_update`; the SDK ACKs the deliberate discard and
re-subscribes that PTY so its next update is a full keyframe. Malformed updates
and updates for unknown PTYs are safely discarded, unrelated packets stay in
the client's bounded pending queue, and dropping an update token sends no ACK.
See `examples/terminal_subscription.rs`.

A guest reaches the host only through Blit packets, so an extension that must
observe the machine spawns a native child with the process family
(`blit_guest::remote::process`) and reads its stdout. The `systemd` extension
(`extensions/systemd`) does this end to end: it keeps the unit tables live from
`systemctl`, poked by D-Bus unit signals, and publishes snapshots and deltas as
JSON on the `blit.systemd.v1` channel while serving
`@systemd list|get|watch|status`. It also shows a single-threaded loop that
multiplexes process output, channel subscribers, and command invocations over
one endpoint, which the blocking typed helpers cannot do on their own.

`EventLoop` combines packet dispatch with one-shot monotonic timers. It keeps
callbacks in a guest-side min-heap, passes only the nearest deadline to the host
wait call, gives a ready packet priority over simultaneous timers, and runs all
callbacks due on a deadline wake. Timer callbacks may reschedule, cancel, or
stop the loop. `Client::wait`, `wait_until`, `sleep`, and blocking `recv` remain
available for simpler synchronous extensions.

`default-features = false` leaves an `alloc`-only ABI/bootstrap core for small
guests. The crate itself is `no_std`; native-only test shims use `std` behind a
target gate.

The entry macro also registers the host RNG for the pinned `getrandom` 0.2.17
custom backend. This covers consumers such as `rand` 0.8 with no JavaScript or
WASI adapter. A root crate that exports its entry manually must expand
`blit_guest::register_getrandom!()` exactly once. `getrandom` 0.3 and later use
the build-wide `getrandom_backend` cfg and are intentionally not selected by
this SDK version.

Rust's standard `HashMap` is not entropy-keyed on `wasm32-unknown-unknown`.
For attacker-controlled keys use `blit_guest::collections::HashMap` or
`HashSet`; their `RandomState` is keyed directly from `blit_v1.random`.

The host permits at most 16 MiB per complete packet and 64 KiB per entropy
call. The safe API checks packet sizes, retries `recv` only after the host says
the next packet needs a larger buffer, caps fragment reassembly at 64 MiB, and
chunks arbitrary random fills into host-sized calls. It never exposes raw
linear-memory pointers.
