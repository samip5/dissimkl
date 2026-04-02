# dissimkl

Discord Rich Presence for [Simkl](https://simkl.com) — shows what you are watching on Discord, sourced from Simkl playback history.

Inspired by [discrakt](https://github.com/afonsojramos/discrakt) (the Trakt equivalent).

---

## Features

- Runs silently in the system tray (macOS, Windows, Linux)
- Polls Simkl every 15 seconds for playback activity changes
- Displays the most recently paused/watched item as Discord Rich Presence
- Supports TV shows, anime, and movies with type-specific Discord app IDs
- Shows poster art, episode/movie details, and buttons linking to Simkl and IMDb/MAL
- Clears Discord status when nothing has been watched in the last 4 hours
- Tray menu: Pause/Resume polling, Quit

---

## Prerequisites

- Rust 1.75+ (`rustup` recommended)
- A [Simkl](https://simkl.com) account
- [Discord](https://discord.com) desktop app

---

## Setup

### 1. Create a Simkl API application

1. Go to <https://simkl.com/settings/developer/>
2. Click **Create new app**
3. Fill in a name and any redirect URI (e.g. `urn:ietf:wg:oauth:2.0:oob`)
4. Copy the **Client ID** — you will need it during first run

### 2. Create Discord applications (one per media type)

You need up to three Discord application IDs — one each for TV shows, anime, and movies. You can reuse the same one for all three, or create separate apps for distinct Rich Presence branding.

1. Go to <https://discord.com/developers/applications>
2. Click **New Application**, name it (e.g. "dissimkl Shows")
3. Copy the **Application ID** from the General Information page
4. Repeat for movies and anime if desired
5. (Optional) Upload art assets named `simkl` in the **Rich Presence → Art Assets** section to use as a fallback icon

### 3. Build and run

```sh
git clone https://github.com/samip5/dissimkl
cd dissimkl
cargo build --release
./target/release/dissimkl
```

On first launch, the app will prompt you for:

- Your Simkl **Client ID**
- (Optional) Discord application IDs for shows, movies, and anime
- It will then open your browser to <https://simkl.com/pin/> for authorization

After authorization, a config file is saved at:

| Platform | Path |
|----------|------|
| macOS    | `~/Library/Application Support/dissimkl/credentials.ini` |
| Linux    | `~/.config/dissimkl/credentials.ini` |
| Windows  | `%APPDATA%\dissimkl\credentials.ini` |

---

## Configuration

The config file (`credentials.ini`) is an INI file with three sections:

```ini
[simkl]
client_id = YOUR_SIMKL_CLIENT_ID
access_token = YOUR_SIMKL_ACCESS_TOKEN

[discord]
app_id_shows  = 123456789012345678
app_id_movies = 123456789012345679
app_id_anime  = 123456789012345680

[settings]
; How often (in seconds) to check Simkl for activity changes
poll_interval_secs = 15
; Sessions older than this many hours are ignored
session_window_hours = 4
```

You can edit this file directly at any time. Changes take effect on the next poll cycle.

If you leave any `app_id_*` values blank, Discord Rich Presence will be skipped for that media type.

---

## How it works

1. Every `poll_interval_secs` seconds, dissimkl calls `GET /sync/activities` to check if any `playback` timestamps changed for TV shows, anime, or movies.
2. If any timestamp changed (or on startup), it fetches `GET /sync/playback` to get the most recent paused/stopped sessions.
3. The newest session within the `session_window_hours` window is shown as Discord Rich Presence.
4. If no session is in-window, the Discord status is cleared.
5. The tray tooltip always reflects the current status.

---

## System tray

| Action | Effect |
|--------|--------|
| Left-click (varies by OS) | Open tray menu |
| Pause | Stops polling and clears Discord status until resumed |
| Resume | Restarts polling |
| Quit | Exits the app and clears Discord status |

---

## Logging

Set the `RUST_LOG` environment variable to control log verbosity:

```sh
RUST_LOG=dissimkl=debug ./dissimkl   # verbose
RUST_LOG=warn ./dissimkl             # errors/warnings only
```

---

## License

MIT
