# File Backup (macOS, Apple silicon)

Backs up chosen folders to a NAS on a schedule. Never deletes NAS files without asking.

## Layout
| Path | What |
|---|---|
| `crates/nasbak-core` | shared types, job validation |
| `crates/nasbak-store` | SQLite state (jobs, runs, files, deleted-files list) |
| `crates/nasbak-connector` | destination access; `Connector` has no delete; `Eraser` is separate |
| `crates/nasbak-engine` | scan, diff, copy (temp file, verify, atomic rename), flag missing files |
| `crates/nasbak-removal` | the only code that can delete from the NAS, only reviewed files |
| `crates/nasbak-sched` | schedule maths, DST-safe, catch-up after sleep |
| `crates/nasbak-mcp` | MCP server logic, nine tools; cannot depend on engine/removal/store |
| `crates/nasbak-agent` | background agent: scheduler, `ui.sock`, `mcp.sock`, approvals, LaunchAgent plist |
| `app/` | Tauri 2 menu-bar app and UI (excluded from the workspace; build on a Mac) |

## Build on your Mac
```sh
xcode-select --install
curl https://sh.rustup.rs -sSf | sh
cargo test --workspace && ./scripts/check-crate-graph.sh
cargo build --release -p nasbak-agent
cd app/src-tauri && cargo install tauri-cli --version "^2" --locked
cargo tauri icon /path/to/1024px-icon.png     # once
cargo tauri build                             # produces the .app and .dmg
```
Copy `target/release/nasbak-agent` next to the app binary (`Contents/MacOS/`) so the app can install it as a LaunchAgent.

## Status
See the design doc. Done and tested on Linux: everything in `crates/`. Written but **not yet compiled or run on macOS**: the `cfg(target_os = "macos")` branches in `nasbak-agent/src/platform.rs` and all of `app/`.
Not built yet: SFTP connector, browse-and-restore, signing/notarization, Windows.

## Windows

The same workspace builds on Windows (CI builds an unsigned NSIS installer). The agent listens on loopback TCP with a
random token kept in `%LOCALAPPDATA%\FileBackup`. Windows cannot tell which program is on a local port, so MCP approval
there is one shared "unverified" identity; Authenticode-based identity is future work.

## Connecting an AI assistant (MCP)

Assistants launch `nasbak-agent mcp-bridge` (Settings, "Connect an assistant" shows the exact JSON). The bridge only copies
JSON-RPC lines between stdio and the agent. The agent identifies the program that launched the bridge (macOS: its
code-signing requirement) and answers only after you allow it in the app. `crates/nasbak-agent/tests/bridge_e2e.rs` runs
a real agent and a real bridge end to end.
