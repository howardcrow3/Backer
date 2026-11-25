# Backer in the Mac App Store

The Mac App Store edition is the same code built with the `mas` feature. It is sandboxed, paid up front ($4.99, no trial),
and updated by the App Store. Bundle ID: `studio.turtlebox.backer`.

## How it differs from the direct download

| | Direct download | Mac App Store |
|---|---|---|
| Background agent | separate `backer-agent` program, LaunchAgent, Unix socket | runs on a thread inside the app (`backer_agent::service`) |
| Licensing | Paddle key, 14-day trial, online activation | none: the purchase is the license (`licensing::STORE_EDITION`) |
| Updates | Tauri updater | the App Store (updater compiled out) |
| `backer://` links | license activation | not registered |
| MCP server | opt-in | left out of the first store release |
| Sleep prevention | `caffeinate` | IOKit power assertion |
| Notifications | `osascript` | UserNotifications |
| Launch at login | LaunchAgent | `SMAppService` main-app login item, switched on in Settings (starts hidden in the menu bar) |
| Chosen folders | plain paths | security-scoped bookmarks, saved when the folder is picked and reopened at start (`bookmarks.json`) |

The system calls live in `crates/backer-agent/native/mas.m` (plain C functions, built only with `mas` on macOS);
`bookmarks.rs` holds the bookkeeping and is tested everywhere.

Still to do before shipping: signing, the installer package and the TestFlight upload, and trying it on a real Mac
(sandbox bookmarks, the login item and notification permission cannot be exercised in CI).

## Building

```
cd app/src-tauri
cargo tauri build --config tauri.mas.conf.json --target aarch64-apple-darwin --bundles app \
  -- --no-default-features --features mas
```

Needs `app/src-tauri/embedded.provisionprofile` (the Mac App Store provisioning profile). CI's `mas` job builds with an
empty stand-in and checks the bundle: no helper program, privacy manifest present, no `backer://`, the right bundle ID.

Files: `tauri.mas.conf.json` (overlay), `entitlements.mas.plist`, `privacy/PrivacyInfo.xcprivacy`, `capabilities/mas.json`,
`Info.plist` (local network prompt text, used by both editions).

## Still needed from Howard (Apple side)

Developer Program enrolment for High Turtle LLC; Apple Distribution and Mac Installer Distribution certificates; the
provisioning profile for `studio.turtlebox.backer`; an App Store Connect API key; the app record, price, screenshots,
privacy policy URL, export-compliance and DSA answers. `ITSAppUsesNonExemptEncryption` is deliberately not set in
Info.plist so App Store Connect asks.
