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
git clone https://github.com/aurabirb/spotclean
cd spotclean
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

### 4. Run

```sh
medley
```

Logs always go to `$XDG_STATE_HOME/medley/medley.log` (fallback
`~/.local/state/medley/medley.log`), truncated on each start; pass
`--log-level debug` (or set `RUST_LOG`) for more detail. If it crashes, the
panic and backtrace are appended to `medley.panic` next to the log.
