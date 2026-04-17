# niri-wallpaper

![Preview](preview.png)

A lightweight, single-process wallpaper daemon for [niri](https://github.com/YaLTeR/niri).

It manages two layer-shell surfaces per output:

- **`wallpaper-bg`** -- the regular desktop background
- **`wallpaper`** -- a pre-blurred copy shown as the overview backdrop (via niri's `place-within-backdrop` layer rule)

Both surfaces live in one process, so there is no extra compositor round-trip or GPU context. The blur is computed once on the CPU when the wallpaper changes, and the daemon is fully idle the rest of the time.

> **Note:** This project was largely built with AI assistance.
> It exists to scratch a personal itch -- a niri-specific wallpaper setup that is simple and lightweight.
> I don't plan to actively maintain it beyond my own needs.

## Features

- Dual-layer output: normal background + blurred overview backdrop
- **TOML config file** with hot-reload (`~/.config/niri-wallpaper/config.toml`)
- **Wallpaper rotation** from a library directory on a cron schedule
- **Weighted random selection** — less-used wallpapers are picked more often
- **Cross-fade animation** with configurable duration and easing
- **IPC** via Unix socket (`niri-wallpaper msg next/set/current`)
- **`rotate` subcommand** — pick a wallpaper and update the symlink without the daemon running (useful for greetd/lock screen integration)
- Hot-reload: watches the wallpaper path and config file via inotify
- SIMD-accelerated cover-scale resize (via `fast_image_resize`)
- Multi-output aware
- Minimal RSS footprint (`malloc_trim` after decode/resize cycles)

## Getting started

### Build

```sh
cargo build --release
```

The binary is at `target/release/niri-wallpaper`.

### Run

```sh
# Use the default path (~/.config/niri-wallpaper/current)
niri-wallpaper

# Or point to a specific image
niri-wallpaper --path /path/to/wallpaper.jpg
```

The daemon stays in the foreground. To run it at startup, add it to your niri config:

```kdl
spawn-at-startup "sh" "-c" "niri-wallpaper rotate && niri-wallpaper"
spawn-at-startup "hyprlock"
```

`rotate` picks a fresh wallpaper and updates the `current` symlink before the daemon starts, so hyprlock (or any other app reading the symlink) sees the same wallpaper.

You also need a layer rule in your niri config so the blurred surface is placed within the overview backdrop:

```kdl
layer-rule {
    match namespace="wallpaper"
    place-within-backdrop true
}
```

## Configuration

Config file: `~/.config/niri-wallpaper/config.toml`

All fields are optional. If the file doesn't exist, the daemon behaves identically to the pre-config version. CLI arguments override config values.

```toml
# Directory containing wallpaper images for rotation.
# Default: ~/.config/niri-wallpaper/library/
library = "/path/to/wallpapers/"

# Cron expression for wallpaper rotation schedule.
# Examples: "0 0 * * *" (daily at midnight), "*/30 * * * *" (every 30 min)
schedule = "0 0 * * *"

[animation]
# Cross-fade duration in milliseconds. 0 = instant switch.
duration_ms = 800
# Easing function: "linear", "ease-in", "ease-out", "ease-in-out"
easing = "ease-in-out"

[blur]
downsample = 8       # Downsample factor before blurring (higher = softer + faster)
radius = 6           # Box-blur radius in downsampled pixels
iterations = 3       # Box-blur passes (3 ≈ Gaussian)

[namespaces]
normal = "wallpaper-bg"
backdrop = "wallpaper"
```

## Commands

```
niri-wallpaper                      Start the daemon
niri-wallpaper rotate               Pick a random wallpaper, update the current
                                    symlink, and exit (no Wayland needed)
niri-wallpaper msg next             Force rotation on a running daemon
niri-wallpaper msg set <PATH>       Set a specific wallpaper on a running daemon
niri-wallpaper msg current          Print the current wallpaper path
```

### CLI options (daemon mode)

```
-p, --path <PATH>              Path to wallpaper image (symlink OK). Watched for changes.
                               Overrides config file. [default: ~/.config/niri-wallpaper/current]
--blur-downsample <N>          Downsample factor before blurring. [default: 8]
--blur-radius <N>              Box-blur radius in downsampled pixels. [default: 6]
--blur-iterations <N>          Box-blur passes (3 ≈ Gaussian). [default: 3]
--normal-namespace <NAME>      Layer-shell namespace for the normal surface. [default: wallpaper-bg]
--backdrop-namespace <NAME>    Layer-shell namespace for the backdrop surface. [default: wallpaper]
-h, --help                     Print help.
```

## How it works

### Wallpaper rotation

When `schedule` is configured, the daemon sets a timer based on the cron expression. On each tick it scans the library directory, picks a wallpaper using weighted random selection (less-used wallpapers have higher probability), and applies it with a cross-fade animation.

Usage counts are persisted at `~/.local/state/niri-wallpaper/usage.toml` so they survive restarts.

### Current symlink

The `~/.config/niri-wallpaper/current` symlink always points to the active wallpaper. This is updated by `rotate`, `msg next`, and `msg set`. Other apps (lock screens, theme managers) can read this symlink to discover the current wallpaper.

### IPC

The daemon listens on `$XDG_RUNTIME_DIR/niri-wallpaper.sock`. The `msg` subcommands connect to this socket to send commands. If the daemon isn't running, the client exits with an error.

Supported image formats: JPEG, PNG, WebP.

## License

MIT
