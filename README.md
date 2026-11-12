# activity-watch-manager

A central store and UI for [ActivityWatch](https://activitywatch.net). Collectors on each machine read the local ActivityWatch API and report to one server. The server keeps the history in a series of SQLite files, about 30 days each, and the UI reads every file a range touches in one query.

Two binaries:

- `awm-server` receives events, stores them, and serves the UI
- `awm-collector` polls the local ActivityWatch server and posts what it finds

```
Mac A                          Mac B
ActivityWatch :5600            ActivityWatch :5600
        |                              |
        v                              v
awm-collector                  awm-collector
        |                              |
        +------------+    +------------+
                     v    v
                awm-server :8787
                meta.sqlite
                periods/2026-10-13.sqlite
                periods/2026-11-12.sqlite
                UI
```

Window titles and URLs are sensitive. The token is a password. Treat it like one.

## Install

### Homebrew (macOS and Linux x86_64)

```sh
brew install vangheem/tap/awm-server
awm-server install
awm-server status
```

`install` writes a config and token if you don't have one, registers a login item, and starts the process. It starts again at login, and restarts if it exits. On Linux this is a systemd user service, with linger enabled so it also starts at boot.

On each Mac that should report activity, with ActivityWatch running:

```sh
brew install vangheem/tap/awm-collector
awm-collector install --server http://192.168.1.20:8787 --token <token>
awm-collector status
```

On the server Mac, omit `--server` and `--token`. The collector reads the local server config.

`status` exits 0 only when that side is actually working: the login item is running, and (for the collector) ActivityWatch, the server, and a recent sync all check out. `awm-server uninstall` and `awm-collector uninstall` remove the login item and keep data. `--purge` deletes it.

After `brew upgrade`, run `install` again so the already-running process restarts on the new binary. The login item points at Homebrew's version-independent path, so the next start picks up the upgrade either way.

The binaries aren't signed. Homebrew installs work as is.

### Prebuilt binaries

Tagged releases (`v*`) are on the [releases page](https://github.com/vangheem/activity-watch-manager/releases), and every push to `main` refreshes the [`latest` prerelease](https://github.com/vangheem/activity-watch-manager/releases/tag/latest).

| Platform | Asset |
| --- | --- |
| macOS (Apple silicon and Intel) | `activity-watch-manager-macos-universal.tar.gz` |
| Linux x86_64 | `activity-watch-manager-linux-x86_64.tar.gz` |

```sh
curl -L https://github.com/vangheem/activity-watch-manager/releases/latest/download/activity-watch-manager-macos-universal.tar.gz | tar xz
mkdir -p ~/.local/bin && mv awm-server awm-collector ~/.local/bin/
awm-server install
```

If a browser download is quarantined, clear it with `xattr -d com.apple.quarantine ~/.local/bin/awm-server`.

### From source

```sh
make install
awm-server install
```

Or `./scripts/install-server-macos.sh` and `./scripts/install-collector-macos.sh`, which build, install into `~/.local/bin`, and run the same `install` command.

## How the databases work

Events are written to the period file that contains their start time. Files are named by the UTC date that period starts, for example `periods/2026-10-13.sqlite`, and each one covers `period_days` (default 30) from the Unix epoch. Changing `period_days` after data exists is refused, so old and new files are not mixed.

A summary does not open one file and hope. It attaches every period file the range can overlap and runs a single `UNION ALL`. The previous file is included too, because a long event can start there and run into the range. The storage panel marks which files the current view actually read.

ActivityWatch grows the duration of a recent event when heartbeats merge, without changing its id. The collector re-sends the last two hours on every poll, and the server upserts on `(device_id, bucket_id, aw_event_id)`.

Active time is current-window time with AFK removed. If two window events overlap on one machine, the one that started later keeps that stretch. Two machines are never collapsed into each other, so totals can exceed clock time when more than one computer was in use.

## Config

Server config is created on first run:

`~/Library/Application Support/activity-watch-manager/server.toml`

```toml
listen = "0.0.0.0:8787"
token = "..."
data_dir = ""          # empty uses Application Support/activity-watch-manager/data
period_days = 30
trust_localhost = true
timezone = ""          # empty uses this machine's timezone

[[categories]]
name = "Code"
color = "#c45c26"
apps = ["Code", "Terminal", "Ghostty"]
```

Leave `categories` empty, or delete every block, to use the built-in groups. `listen = "127.0.0.1:8787"` keeps the server on this Mac only.

Collector config:

`~/Library/Application Support/activity-watch-manager/collector.toml`

```toml
server_url = "http://127.0.0.1:8787"
token = "..."
aw_url = "http://127.0.0.1:5600"
aw_token = ""                 # only if ActivityWatch itself requires a key
poll_interval_secs = 60
lookback_secs = 7200
initial_sync_days = 30
```

The first successful sync pulls `initial_sync_days`. After that it only re-reads `lookback_secs`, unless the server was down — the cursor does not advance on a failed post, so the gap is sent on the next success.

Logs:

- `~/Library/Logs/activity-watch-manager/server.log`
- `~/Library/Logs/activity-watch-manager/collector.log`

## Commands

| Command | What it does |
| --- | --- |
| `awm-server install` | Login item, start now, print the token |
| `awm-server status` | Service, health, machines, storage. Exit 1 if not healthy |
| `awm-server token` | Print the token |
| `awm-server uninstall` | Remove the login item. `--purge` deletes data |
| `awm-collector install` | Login item. `--server` and `--token` the first time |
| `awm-collector status` | Service, ActivityWatch, server, last sync |
| `awm-collector --once` | Sync once and exit, without installing |

`awm-server paths` and `awm-collector paths` print the files they use.

## Releasing

Run **Release new version** from the Actions tab (or `gh workflow run bump-version.yml -f bump=patch`) and pick `patch`, `minor`, or `major`. It bumps the workspace version, commits to `main`, and pushes a `vX.Y.Z` tag. The tag builds universal macOS and Linux x86_64 archives, publishes the GitHub release, and updates `awm-server` and `awm-collector` in [vangheem/homebrew-tap](https://github.com/vangheem/homebrew-tap).

Tagging by hand works too, as long as the tag matches the version in `Cargo.toml`.

## API

| Method | Path | Auth |
| --- | --- | --- |
| GET | `/api/v1/health` | none |
| POST | `/api/v1/ingest` | bearer token |
| GET | `/api/v1/summary?start=&end=&tz=&device=` | token, or localhost |
| GET | `/api/v1/status` | token, or localhost |

`start` and `end` are RFC3339, `end` exclusive. `tz` is an IANA name such as `America/Chicago`. `device` is the ActivityWatch device id; omit it for every machine.

From another browser, open `http://host:8787/?token=...` once. The page stores the token and strips it from the URL.
