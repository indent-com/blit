# Design RFCs

Per-subsystem design documents. Each one records the shape of a feature,
the alternatives considered, and the wire additions it introduces. The
consolidated opcode tables live in [../protocol.md](../protocol.md).

| Document                     | Subsystem                                                            |
| ---------------------------- | -------------------------------------------------------------------- |
| [fs-watch.md](fs-watch.md)   | Filesystem state sync — staged snapshots, diffs, churn bounds        |
| [fs-write.md](fs-write.md)   | Filesystem writes from the client                                    |
| [fs-search.md](fs-search.md) | Filename search (`FS_SEARCH`) and the client-side index (`FS_INDEX`) |
| [fs-grep.md](fs-grep.md)     | Project-wide content search (`FS_GREP`)                              |
| [git.md](git.md)             | Git introspection — status, log, diffs                               |
| [lsp.md](lsp.md)             | Language intelligence over the blit wire                             |
| [kv.md](kv.md)               | Host-local key/value store with CAS writes and prefix watches        |
| [net.md](net.md)             | TCP and UDP relay, TLS termination (`NET_*`)                         |

Each document carries its own **Status** line; that line, not this table,
is the source of truth for how much of an RFC has shipped.
