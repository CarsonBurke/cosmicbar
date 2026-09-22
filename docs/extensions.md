# Extensions

An extension is a program that draws one bar cell and its popup. cosmicbar
spawns it once, sends it JSON lines on stdin, and reads JSON lines from its
stdout. It can be written in any language that can print a line.

Nothing in the protocol has an interval: the bar draws the last frame it was
sent, and an extension sends a frame when its own source told it something
changed. [`src/bin/cosmicbar-mlq.rs`](../src/bin/cosmicbar-mlq.rs) is a complete
native Rust example that streams the local ML job queue.

## Declaring one

```toml
# ~/.config/cosmicbar/config.toml
right = ["extension:mlq", "volume", "power"]

[[extensions]]
name = "mlq"
command = ["cosmicbar-mlq"]
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
{"cell": {"glyph": "󰁹", "text": "sweep +2 · 4m", "color": "green"},
 "header": {"lines": [{"text": "3 running"}, {"text": "3 of 4 slots busy", "color": "muted", "small": true}],
            "action": {"id": "pause", "label": "pause"}},
 "popup": [{"section": "running"},
           {"row": {"lines": [{"text": "sweep"}, {"text": "4m of 1h · #12", "color": "muted", "small": true}],
                    "progress": {"value": 0.07, "color": "green"},
                    "action": {"id": "cancel:12", "glyph": "󰅖"}}}]}
```

| Field | Type | Notes |
|---|---|---|
| `cell` | object or `null` | `null` (or absent) hides the module: no island, no space taken. |
| `cell.glyph` | string | Nerd Font glyph, drawn at icon size before the text. |
| `cell.text` | string | May be empty for an icon-only cell. |
| `cell.color` | role | Colours glyph and text alike. |
| `header` | row or `null` | The popup's header, pinned above the list. Its first line is the card's title. |
| `popup` | array | The list under the header. Empty (or absent) means the cell is not clickable unless a `header` is sent. |

The popup is a card: the `header` stays on screen and the `popup` list scrolls
under it. Put what the popup *is* and the verb that acts on all of it in the
header — a queue and its pause, a device and its switch — and one row per thing
in the list. A header line is drawn at the popup's title size unless it sets
`small`, so `lines` reads as a title with its state under it.

Unknown fields are rejected, not ignored: a frame with a typo in a key is a
malformed frame.

Popup items:

| Item | Shape |
|---|---|
| Text | `{"text": <text>}` |
| Row | `{"row": {"lines": [<text>, …], "progress": <progress>&#124;null, "action": <action>&#124;null}}` |
| Section | `{"section": "up next"}`: a small label over the rows after it |
| Divider | `"divider"` |

A `<text>` is `{"text": "…", "color": <role>, "small": false}`; `small` picks the
secondary text size.

A `<progress>` is `{"value": 0.5, "color": <role>}`, a thin meter under the
row's lines: a run against its time limit, a transfer. `value` runs from 0 to 1
and is clamped; `color` defaults to `accent`.

An action is `{"id": …, "label": …, "glyph": …, "danger": false, "enabled":
true}`. Pressing it sends `{"action": "<id>"}`; `enabled: false` keeps a
spoken-for button visible instead of vanishing (a cancel already requested), and
`danger` paints it as destructive. A `glyph` (a Nerd Font icon) is drawn instead
of the `label`: use it for a verb repeated down a list, where a column of words
would be the loudest thing in the popup, and a label for a one-off or a
question. Send at least one; a button with neither shows its `id`.

Group rows with sections rather than prefixes: `running`, `up next`, `recent`
say once what each row would otherwise repeat.

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

## Native worked example

Install the bundled extension from the repository:

```sh
cargo install --path . --bin cosmicbar-mlq
```

Ensure Cargo's installation directory (normally `~/.cargo/bin`) is on the
bar's `PATH`, or use the installed binary's absolute path in `command`.
Install it alongside the bar it came with: frames use the protocol items of
their version, and a bar rejects a frame with items it does not know.

The complete implementation is
[`src/bin/cosmicbar-mlq.rs`](../src/bin/cosmicbar-mlq.rs). It subscribes to mlqd's
version-8 length-prefixed JSON socket protocol, accepting frames up to 1 MiB.
It looks for `$XDG_RUNTIME_DIR/mlqueue/mlqd.sock`, falling back to
`$XDG_STATE_HOME/mlqueue/runtime/mlqd.sock` (or
`~/.local/state/mlqueue/runtime/mlqd.sock` when `XDG_STATE_HOME` is unset).
No queue polling or external interpreter is involved.

The cell shows the longest-running job and its elapsed time; an idle queue
hides it. The popup's header counts running and waiting jobs over the slot use
or the reason nothing starts (paused, admission blocked), beside a pinned
pause/resume button. Under it, sections list:

- **needs attention**: jobs mlqd wants `mlq recover` for, counted as stuck in
  the header.
- **running**: longest first, with elapsed time and, for a job with a time
  limit, a meter that turns peach at 80%.
- **up next**: in the order the scheduler will take them, each with what it
  waits for in words (`next · when a slot frees`, `after <job>`, `held`) rather
  than mlqd's eligibility code. A held job's button releases it rather than
  cancelling it; `mlq cancel` still does that.
- **recent**: the last three jobs finished in the past day, with their outcome
  (`failed · exit 1 · 13:08`); a failed or lost one can be retried.

Cancel is two presses: the first turns the row's button into `cancel?`, and a
second within four seconds sends it. Any other press, or closing the popup,
disarms it. A separate task serializes mutations so a slow response cannot
hold up popup notifications. Failed mutations appear in the popup until a
successful mutation or a new snapshot clears the error.

Elapsed times refresh at the next displayed digit: seconds for the first
minute, then minutes. With the popup closed only the cell's headline controls
that cadence; opening it includes every visible run. Job start estimates stay
fixed through unrelated status updates. A disconnected daemon leaves the last
snapshot visible, mutes a running cell and marks the header as reconnecting.
Reconnections back off through 1, 2, 5, 10 and 30 seconds, resetting after a
60-second healthy session. Stdin EOF or a broken stdout pipe ends the extension.
