# Extensions

An extension is a program that draws one bar cell and its popup. cosmicbar
spawns it once, sends it JSON lines on stdin, and reads JSON lines from its
stdout. It can be written in any language that can print a line.

Nothing in the protocol has an interval: the bar draws the last frame it was
sent, and an extension sends a frame when its own source told it something
changed. `contrib/extensions/cosmicbar-mlq` is a complete example in
dependency-free Python that streams the local ML job queue.

## Declaring one

```toml
# ~/.config/cosmicbar/config.toml
right = ["extension:mlq", "volume", "power"]

[[extensions]]
name = "mlq"
command = ["/home/you/bin/cosmicbar-mlq"]
```

A region places the module as `extension:<name>`; the same string addresses it
from a keybind, `cosmicbar toggle extension:mlq`. A declared extension runs only
while it is placed in `left`/`center`/`right`: the bar spawns it on startup,
restarts it with backoff (1s → 60s) if it exits, and kills it when the bar exits.
Its stderr goes to the bar's log, so use it for diagnostics.

On a config reload, editing `command` restarts that program, and removing or
renaming the entry - or dropping it from every region - stops it. Editing
anything else leaves it running.

## The bar → extension

One object per line on your stdin:

| Line | Meaning |
|---|---|
| `{"popup": true}` / `{"popup": false}` | Your popup opened / closed. |
| `{"action": "<id>"}` | A popup button was pressed. |

Both are advisory: answer an action by sending the frame that reflects it, and
use `popup` to gather expensive detail (a process list, a device scan) only
while it is on screen. Reaching EOF means the bar is gone — exit.

`popup` is state, not an event stream: it is sent when the state changes, so a
program that misses one is told the truth by the next one. Both kinds are
dropped rather than queued without bound if you stop reading stdin, so read it
from a thread that never blocks on your own work.

## The extension → bar

One *frame* per line on stdout. A frame is everything to draw until the next
one:

```json
{"cell": {"glyph": "󰁹", "text": "3 running", "color": "green"},
 "popup": [{"text": {"text": "queue", "color": "muted", "small": true}},
           "divider",
           {"row": {"lines": [{"text": "#12 sweep"}, {"text": "running · 4m", "small": true}],
                    "action": {"id": "cancel:12", "label": "cancel", "danger": true}}}]}
```

| Field | Type | Notes |
|---|---|---|
| `cell` | object or `null` | `null` (or absent) hides the module: no island, no space taken. |
| `cell.glyph` | string | Nerd Font glyph, drawn at icon size before the text. |
| `cell.text` | string | May be empty for an icon-only cell. |
| `cell.color` | role | Colours glyph and text alike. |
| `popup` | array | Empty (or absent) means the cell is not clickable. |

Unknown fields are rejected, not ignored: a frame with a typo in a key is a
malformed frame.

Popup items:

| Item | Shape |
|---|---|
| Text | `{"text": <text>}` |
| Row | `{"row": {"lines": [<text>, …], "action": <action>&#124;null}}` |
| Divider | `"divider"` |

A `<text>` is `{"text": "…", "color": <role>, "small": false}`; `small` picks the
secondary text size.

An action is `{"id": …, "label": …, "danger": false, "enabled": true}`. Pressing
it sends `{"action": "<id>"}`; `enabled: false` keeps a spoken-for button
visible instead of vanishing (a cancel already requested), and `danger` paints
it as destructive.

Colours are palette roles, never hex, so an extension follows the bar's theme:
`fg`, `muted`, `faint`, `accent`, `green`, `yellow`, `peach`, `red`.

## Rules that keep the bar cheap

- Send a frame only when the pixels would change. A frame identical to the one
  already on screen is dropped, so re-emitting whole state costs nothing but
  your own work; anything else is a repaint.
- Do not poll on a timer to see whether something changed; subscribe to the
  thing itself (a socket, a D-Bus signal, `inotify`). A timer is only for
  something that genuinely moves on its own, like an elapsed time — and then at
  the resolution you actually display.
- Keep a frame under 256 KiB, newline included. A longer line is a runaway
  writer, not a bar cell: the bar stops reading and restarts the program.
- A malformed frame is logged and ignored; the last good frame stays on screen.

## Minimal example

```python
#!/usr/bin/env python3
import json, sys

state = {"popup": False, "pokes": 0}
last = None

def frame() -> None:
    global last
    body = json.dumps({
        "cell": {"glyph": "\uf120", "text": f"hello {state['pokes']}", "color": "accent"},
        "popup": [{"row": {"lines": [{"text": f"popup is {'open' if state['popup'] else 'shut'}"}],
                           "action": {"id": "poke", "label": "poke"}}}],
    })
    # The rule the bar cares about: never write a frame it already has.
    if body != last:
        last = body
        print(body, flush=True)

frame()
for line in sys.stdin:
    message = json.loads(line)
    if "popup" in message:
        state["popup"] = bool(message["popup"])
    if message.get("action") == "poke":
        state["pokes"] += 1
    frame()
```
