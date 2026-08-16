# blit's delta against wayland-backend 0.3.15

Unmodified crates.io 0.3.15 apart from one hunk in
`src/rs/server_impl/registry.rs`, in `Registry::disable_global`.

## What

`disable_global` broadcast `wl_registry.global_remove` to every known registry
without consulting the global's `can_view` filter. All three paths that send
`wl_registry.global` do consult it — `check_bind`, the initial registry
enumeration, and `send_global_to_all` — so a filtered global could be withdrawn
from clients it had never been advertised to. The patch adds the same `can_view`
check to the removal loop.

## Why blit hits it

blit publishes one `wl_output` global per toplevel, filtered to its owner
(`can_view(client, g) = client.id() == g.owner`, `crates/compositor/src/imp.rs`).
Closing any window therefore told *every other client* to remove a global it had
never seen. Clients that keep a map keyed by registry name have nothing to
remove: xwayland-satellite unwraps exactly that lookup
(`src/server/mod.rs`, `globals_map.remove(&global).unwrap()`) and panics, so one
unrelated window closing killed the X11 bridge and every X application with it.

Regression test: `crates/compositor/tests/foreign_output_removal.rs`. The
existing `output_global_race.rs` covers the other side — the owner must still be
told.

## Upstream status

Present in 0.3.15 through 0.3.17 (latest at the time of writing); a version bump
does not fix it. Not yet reported upstream.

## Re-applying on upgrade

```
diff -u <upstream>/src/rs/server_impl/registry.rs src/rs/server_impl/registry.rs
```

The hunk is the `if !global.handler.can_view(...) { continue; }` guard inside the
`for registry in self.known_registries` loop.
