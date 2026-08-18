# cosmicbar

A Wayland status bar written as an application, not as a shell script harness.
Rust and [libcosmic](https://github.com/pop-os/libcosmic) on `wlr-layer-shell`:
every module talks to its own source in-process, and every cell can open a real
popup you can click.

![bar](docs/bar.png)

![queue popup](docs/popup.png)

## Why

Bars like waybar are configured as text: a JSON module list, a CSS file, and a
row of `exec` scripts polled on timers. That buys portability at the cost of
`nvidia-smi` forks every five seconds, tooltips instead of interfaces, and
layout hacks for anything that is not a label.

cosmicbar keeps the same look but makes the bar a program:

- **Subscribe where the source has events.** D-Bus, netlink, niri's IPC, logind,
  BlueZ, UPower, NetworkManager, MPRIS and the tray all push; the bar has no
  global timer and no `exec` scripts on intervals. What the kernel and drivers
  expose no event for is sampled in-process on its own clock instead - `/proc`
  for CPU and memory and NVML for the GPU, every 2s, with no fork per reading -
  and detail nobody is looking at (per-process tables, per-monitor DDC state)
  is only gathered while its popup is open. Two things still run a program,
  because there is no library to call: `ddcutil` for external-monitor
  brightness, and `checkupdates`/`pacman -Qu` hourly.
- **Real widgets.** Clicking a cell opens a popup surface with buttons: pick a
  wifi network, connect a bluetooth device, switch audio sink, kill a process,
  jump to a window, cancel a queued job.
- **Cheap.** Against the waybar setup it replaces, both bars running on the same
  desktop for 150s (`contrib/measure-bar.py`): **0.28% CPU vs 2.43%**, 59 MB RSS
  vs 28 MB. It buys the CPU with memory: one process with a GPU-less renderer
  and its own font atlas, instead of GTK plus a script per module.
- **Typed configuration.** One `config.toml`; palette roles instead of a CSS
  cascade; a bad file logs and falls back instead of leaving you barless.

## Install

```bash
cargo install --path .            # ~/.cargo/bin/cosmicbar
```

Then start it from your compositor. niri:

```kdl
spawn-at-startup "/home/you/.cargo/bin/cosmicbar"
```

Hyprland: `exec-once = cosmicbar`. sway: `exec cosmicbar`. A systemd user unit
is in `contrib/cosmicbar.service`, and `contrib/niri.kdl` has the keybinds.

Needs a Nerd Font (`CommitMono Nerd Font Mono` by default) for module glyphs.

## Configure

`~/.config/cosmicbar/config.toml`; every field is optional.

```toml
height = 24
font_size = 16.0
palette = "catppuccin-mocha"   # or catppuccin-latte
terminal = "kitty"
outputs = []                   # empty = every monitor

left = ["launcher", "taskbar"]
center = ["cpu", "memory", "gpu", "workspaces", "idle_inhibitor", "date", "time",
          "network", "bluetooth", "updates", "notifications", "tray"]
right = ["mpris", "volume", "brightness", "battery", "power"]
```

Modules: `time`, `date`, `workspaces`, `taskbar`, `cpu`, `memory`, `gpu`,
`network`, `bluetooth`, `volume`, `mpris`, `tray`, `notifications`, `updates`,
`battery`, `brightness`, `idle_inhibitor`, `launcher`, `power`. Placing one is
what starts its subscription; leaving it out costs nothing at runtime.

The file is watched: saving it re-lays out the bar in place.

## Popups from a keybind

```kdl
Mod+Shift+D { spawn "cosmicbar" "toggle" "date"; }
Mod+Shift+Escape { spawn "cosmicbar" "close"; }
```

`cosmicbar toggle <module>`, `close` and `reload` talk to the running bar over a
per-display socket, so the same binds work in a nested session.

## Extensions

Anything the bar does not ship can be another program: it prints one JSON frame
per line and gets back popup and click events. Frames are drawn with the bar's
own widgets and palette, so an extension does not look bolted on.

```toml
right = ["extension:mlq", "volume", "power"]

[[extensions]]
name = "mlq"
command = ["/home/you/.local/bin/cosmicbar-mlq"]
```

`contrib/extensions/cosmicbar-mlq` is a working one in dependency-free Python:
it subscribes to a local ML job queue and gives the bar a cell plus a popup with
per-job cancel buttons. Protocol: [docs/extensions.md](docs/extensions.md).

## Compositor support

Developed and run on niri. The bar itself is plain `wlr-layer-shell`, so
Hyprland, sway and river should work too — untested. Three modules speak niri's
IPC today: `workspaces` and `taskbar`, which need compositor state and stay
hidden elsewhere, and `power`, whose *log out* button asks niri to quit (the
rest of that popup is logind and systemd). Everything else — D-Bus, `/proc`,
sysfs, NVML, PulseAudio/PipeWire, NetworkManager, BlueZ, UPower — is
compositor-agnostic.

## Layout

`src/modules/<name>.rs` is one module: its state, its subscriptions, its bar
cell and its popup, with no shared plumbing to edit. `src/bar.rs` lays regions
out into islands, `src/theme.rs` holds the palette and glyph metrics,
`src/control.rs` is the socket, `src/extension.rs` is the extension protocol.

MIT.
