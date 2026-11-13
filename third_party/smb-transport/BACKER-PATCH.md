# Backer patch to smb-transport 0.12.1

Unmodified copy of the published crate (MIT, https://github.com/afiffon/smb-rs) except one change in
`src/tcp/transport.rs`: the TCP socket gets `set_nodelay(true)` after connecting.

Without it every SMB request waits about 40 ms for a delayed ACK (Nagle's algorithm), so each file
costs roughly 0.4 s and a 100,000-file backup takes many hours. With it, a file costs a few
milliseconds on a LAN.

Drop this copy and the `[patch.crates-io]` entry in the workspace Cargo.toml once upstream sets
TCP_NODELAY (or exposes it in `TransportConfig`).
