**medley** is a terminal music player: one catalog and one queue over multiple 
source plugins (a local/HTTP directory, Spotify and SoundCloud), with a link 
tool for treating the same track from different sources as one entry.

<img width="1812" height="958" alt="image" src="https://github.com/user-attachments/assets/48fc37b5-b127-4e15-af64-991afa619d9d" />


## Screens & keys

Three screens, switched with <kbd>1</kbd> / <kbd>2</kbd> / <kbd>3</kbd>: Search,
Queue, Playlists.

| Key | What it does |
|-----|--------------|
| <kbd>/</kbd> | Focus the search box |
| <kbd>:</kbd> | Open the `:` command line (`:help` lists every command) |
| <kbd>Enter</kbd> | Play the selected track, or open the selected playlist |
| <kbd>Space</kbd> | Play / pause |
| <kbd>n</kbd> / <kbd>p</kbd> | Next / previous track |
| <kbd>q</kbd> | Enqueue the selected track |
| <kbd>.</kbd> / <kbd>,</kbd> | Seek forward / back |
| <kbd>+</kbd> / <kbd>-</kbd> | Volume up / down |
| <kbd>l</kbd> | Like the selected track, or unlike it (after a confirmation) when it is liked |
| <kbd>Shift</kbd>+<kbd>Q</kbd> | Quit |

`:` commands include `search`/`s`, `newplaylist`/`np`, `add-to-playlist`/`add`,
`open`, `export`, `log`, `settings`, `panes`, `link` (pick a row, then run it
again on a second row to merge them as one track), `unlink`, and `help`/`h`.
Every command has one long and one short spelling — run `:help` in-app for the
full list with descriptions.

## Configuration

`config.toml` under `$XDG_CONFIG_HOME/medley` (fallback `~/.config/medley`).

### Web interface

Playback controls and search in a browser: set `[web] enabled = true` (optional `port`, default `7878`) and open
`http://127.0.0.1:7878`. It listens on loopback only and rejects foreign `Host`/`Origin` headers. The frontend
(`web/`, React + zustand) is embedded from the committed `web/dist`; after changing it run
`npm install && npm run build` (or `bun install && bun run build`) in `web/` and commit `dist`.

## Build & run

Check the Github Releases page of this project before building it yourself.

### 1. Install Rust

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Restart your shell afterwards (or `source "$HOME/.cargo/env"`) so `cargo` is on
your `PATH`.

### 2. Install system dependencies

The UI is a pure-Rust `crossterm` backend and network calls use `rustls`, so
the only native dependency is the audio backend (`cpal`, via `rodio`):

**macOS**: none — `cpal` uses CoreAudio directly.

**Linux** — ALSA dev headers and `pkg-config`:

```sh
# Debian / Ubuntu
sudo apt install libasound2-dev pkg-config

# Fedora
sudo dnf install alsa-lib-devel pkgconf-pkg-config

# Arch
sudo pacman -S alsa-lib pkgconf
```

Building with the `spotify` feature additionally needs OpenSSL dev headers
(`libssl-dev` / `openssl-devel` / `openssl`) for `librespot`'s TLS stack.

### 3. Build and install

```sh
git clone https://github.com/aurabirb/greyball
cd greyball
cargo install --path app --locked --features spotify,soundcloud
```

Each source plugin is an opt-in cargo feature on the `app` crate; drop
`soundcloud` (or `spotify`) from `--features` to leave that source out. With no
features at all you still get the HTTP directory-listing source.

This puts the `medley` binary in `~/.cargo/bin`. To just run it from the source
tree without installing:

```sh
cargo run --all-features
```

Or skip Rust and download a prebuilt binary of the latest release (`:update` keeps it current):

| Platform | Download |
| --- | --- |
| Linux x86_64 | https://github.com/aurabirb/greyball/releases/latest/download/medley-linux-x86_64 |
| Linux arm64 | https://github.com/aurabirb/greyball/releases/latest/download/medley-linux-arm64 |
| macOS x86_64 | https://github.com/aurabirb/greyball/releases/latest/download/medley-macos-x86_64 |
| macOS arm64 | https://github.com/aurabirb/greyball/releases/latest/download/medley-macos-aarch64 |
| Linux x86_64 Flatpak | https://github.com/aurabirb/greyball/releases/latest/download/medley-linux-x86_64.flatpak |
| Linux arm64 Flatpak | https://github.com/aurabirb/greyball/releases/latest/download/medley-linux-arm64.flatpak |

```sh
install -Dm755 medley-<platform> ~/.local/bin/medley
```

`~/.local/bin` must be on your `PATH`; it is where `:update` installs. On macOS,
run `xattr -d com.apple.quarantine ~/.local/bin/medley` if the binary was
downloaded in a browser (or fetch it with `curl -fLo`).

The Flatpak shares `~/.config/medley`, `~/.local/share/medley` and
`~/.local/state/medley` with the native binary and adds a menu entry; paths
configured elsewhere (a moved media cache, a Spotify cache dir, the slskd data
dir) need `flatpak override --user --filesystem=<path> io.github.aurabirb.greyball`.
`:update` doesn't apply to it, so install a newer `.flatpak` instead:

```sh
flatpak install --user medley-linux-x86_64.flatpak
flatpak run io.github.aurabirb.greyball
```

### 4. Run

```sh
medley
```

Logs always go to `$XDG_STATE_HOME/medley/medley.log` (fallback
`~/.local/state/medley/medley.log`), truncated on each start; pass
`--log-level debug` (or set `RUST_LOG`) for more detail. If it crashes, the
panic and backtrace are appended to `medley.panic` next to the log.

## Credits

Online BPM lookups use the [GetSongBPM.com](https://getsongbpm.com) API.

Genre/style embeddings use the [discogs-effnet](https://essentia.upf.edu/models.html) model
(Music Technology Group, Universitat Pompeu Fabra), licensed CC BY-NC-SA 4.0.
