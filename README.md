# az-servicebus

Terminal UI for browsing and managing Azure Service Bus topics, subscriptions, and messages. Built with Rust, Ratatui, and Tokio.

## Features

- **Multi-profile connection management**: store multiple Azure Service Bus connection strings locally, switch profiles at runtime, and add or remove profiles from the UI.
- **Topic and subscription browsing**: lists topics and subscriptions via the Azure Service Bus AtomPub REST Management API, displaying active and dead-letter message counts.
- **Non-destructive message peek**: inspects messages in either the Active queue or the Dead-Letter Queue (DLQ) without locking or removing them, with configurable batch size (`25` to `500` messages per peek).
- **Message inspector**: displays `BrokerProperties`, custom `UserProperties`, and pretty-printed JSON payloads (or raw text when the body is not JSON).
- **Single-message deletion**: deletes an individual message from the active queue or DLQ after an explicit confirmation prompt.
- **High-throughput subscription purge**: drains a subscription or its DLQ using AMQP over WebSocket (`azservicebus`, TLS on port 443) with 4 concurrent `ReceiveAndDelete` receivers in batches of up to 100 messages, reporting live progress and requiring explicit `[y/N]` confirmation.
- **Keyboard and mouse support**: full keyboard navigation plus clickable panels, list rows, and footer shortcuts. Toggle mouse capture with `m` when you need native terminal text selection.
- **Non-blocking architecture**: network I/O, XML/JSON parsing, and AMQP operations run on a dedicated Tokio worker thread decoupled from the rendering loop.

## Installation

### Prerequisites

- Rust 1.85 or newer (edition 2024)

### Build from source

```sh
cargo build --release
```

The compiled binary is written to `target/release/az-servicebus`.

Run directly with Cargo:

```sh
cargo run --bin az-servicebus
```

## Configuration

Connection profiles are stored as JSON with owner-only permissions (`0600` on Unix):

- macOS: `~/Library/Application Support/asb-tui/config.json`
- Linux: `~/.config/asb-tui/config.json`
- Windows: `%APPDATA%\asb-tui\config.json`

The `asb-tui` directory name is preserved for backwards compatibility with existing profile configurations.

Example `config.json`:

```json
{
  "active_profile": "production",
  "profiles": [
    {
      "id": "prod-1",
      "name": "production",
      "connection_string": "Endpoint=sb://<namespace>.servicebus.windows.net/;SharedAccessKeyName=<policy>;SharedAccessKey=<key>"
    }
  ]
}
```

Profiles can also be created and removed directly from the in-app Profiles modal (`p`).

## Controls

### Main workspace

| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` / `h` / `l` / `Left` / `Right` | Switch focused panel (Topics, Subscriptions, Messages, Message Detail) |
| `j` / `k` / `Up` / `Down` | Move selection or scroll message detail |
| `PgUp` / `PgDn` / `g` / `G` / `Home` / `End` | Jump in lists or message detail |
| `Enter` | Advance to Subscriptions, load Messages, or focus Message Detail |
| `Esc` | Return from Message Detail to Messages list |
| `t` | Toggle between Active queue and Dead-Letter Queue (DLQ) |
| `+` / `-` | Increase or decrease peek batch size by 25 (range 25-500) |
| `r` | Refresh the focused panel |
| `p` | Open Profiles modal |
| `d` / `Delete` | Delete selected message (requires confirmation) |
| `P` | Purge all available messages in the selected subscription or DLQ (requires confirmation) |
| `m` | Toggle mouse capture (disable for native terminal text selection) |
| `q` / `Ctrl+C` | Quit |

### Profiles modal

| Key | Action |
|---|---|
| `j` / `k` / `Up` / `Down` | Select profile |
| `Enter` | Activate selected profile |
| `a` | Add a new profile (`Tab` switches fields, `Ctrl+U` clears field, `Enter` saves) |
| `x` | Remove selected profile |
| `Esc` | Close modal |

## Purge behavior

Purge opens 4 concurrent AMQP-over-WebSocket receivers in `ReceiveAndDelete` mode targeting only the selected subscription (or its `$DeadLetterQueue`). Prefetch is disabled so messages are not deleted ahead of the reported count. Each receiver continues until an empty receive times out (1 second); purge completes when all receivers finish.

- Only available messages are affected (locked, deferred, or future messages are untouched).
- `ReceiveAndDelete` is irreversible: if an error occurs mid-purge, in-flight receives are awaited and confirmed counts are reported, without falling back to REST deletion.

## Architecture

`az-servicebus` uses a decoupled two-thread async worker architecture: the main thread handles terminal events and Ratatui rendering without blocking on I/O, while a Tokio worker executes REST Management API calls (AtomPub XML with HMAC-SHA256 SAS tokens), non-destructive message peeks, and AMQP WebSocket purges over `tokio::sync::mpsc` channels. A panic hook restores terminal state (`disable_raw_mode` and `LeaveAlternateScreen`) on unexpected exit.

```
az-servicebus/
├── src/
│   ├── main.rs      # Terminal setup, panic/restore hook, event loop
│   ├── app.rs       # UI state machine, keyboard and mouse handling
│   ├── ui.rs        # Ratatui layout, panels, modals, footer hit-testing
│   ├── worker.rs    # Async Tokio worker and command/event channels
│   ├── azure.rs     # Connection string parsing, SAS HMAC-SHA256 auth, REST management & peek
│   ├── purge.rs     # Concurrent AMQP WebSocket ReceiveAndDelete purge
│   └── config.rs    # Local profile storage
└── Cargo.toml
```
