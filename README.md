# arterminal

Terminal ASCII art with colour. A library first — every feature the bundled `arterminal` binary
has is a function in the crate, so a program embedding this is never the second-class caller.

Open a text file of art, pick colours into a palette, paint them onto the characters, save. The
colours are written back into the same file as terminal escapes, so `cat` shows the result.

```
  ███     # colour 1  (brush)
  ███     # colour 2
  [+]

  ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒
  ▒⣿⡇⣿⣿⣿⠛⠁⣴⣿⡿⠿⠧⠹⠿⠘⣿⣿⣿⡇⢸⡻⣿⣿⣿⣿⣿⣿⣿▒
> ▒⢹⡇⣿⣿⣿⠄⣞⣯⣷⣾⣿⣿⣧⡹⡆⡀⠉⢹⡌⠐⢿⣿⣿⣿⡞⣿⣿⣿▒
  ▒⣾⡇⣿⣿⡇⣾⣿⣿⣿⣿⣿⣿⣿⣿⣄⢻⣦⡀⠁⢸⡌⠻⣿⣿⣿⡽⣿⣿▒
  ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒
painting while held · modified · brush: colour 1
Program: ^X/esc close · ^C quit · ^S save · ^Z undo · shift+^Z redo
Draw:    space hold to paint · backspace hold to erase · b toggle painting · del toggle erasing · i…
Display: F6 split · shift+F6 side by side · pgup/pgdn page · F5 redraw
```

The swatches are blocks of their colour and `▒` is the grey border around the art. Each hint line
starts with a grey title and colours its actions, a colour per line, so a key and what it does are
never run together.

## The picker, two ways

**In code** (the library route):

```rust
use arterminal::Picker;

let mut picker = Picker::open("examples/skull.txt")?;   // art, colours and palette, one file
arterminal::run(&mut picker)?;                           // Ctrl+S inside writes it back
```

A program that already owns its screen can skip `run` and drive `ui::render` and `ui::apply`
itself — both are pure functions over the `Picker`, which is also why everything is testable
without a terminal.

**From the shell:**

```sh
arterminal examples/skull.txt
```

## The binary's contract

One argument, the file to edit, and optionally `--su` — see
[Holding through the input devices](#holding-through-the-input-devices). The picker draws on
**stderr**; stdout carries nothing, because everything the session produces goes back into the
file on `Ctrl+S`. Exit codes: `0` the picker ran, or you declined to open the file; `2` the file or
the terminal was unusable. Under `--su`, a sudo that refuses passes its own status through.

## The file

Plain UTF-8 text, in which the art comes first and a palette may follow:

```text
⣿⣿ESC[38;2;226;57;57m⡇ESC[0m
=== arterminal palette ===
ember<TAB>#e23939
shade<TAB>#a02020<TAB>ember<TAB>0,0,-60
```

* **A plain text file is a valid document** with no colours and an empty palette — and a file
  that was only opened and saved comes back byte for byte.
* **Colours are inline terminal escapes**, so printing the file shows the drawing coloured. The
  24-bit foreground sets a colour, spelled with semicolons or the ITU way with colons. Escapes are
  cut out by the terminal's own grammar, so what loads as art is exactly what a terminal would
  draw; everything else in a hand-made file, bold and backgrounds included, is passed over.
* **16- and 256-colour codes are kept as slots.** They name places in the terminal's palette
  rather than colours, so they load as slot swatches, tagged `slot 196` in the palette — the first
  16 as `slot 3 (theme)`, since only those follow the theme — and are drawn and saved as the slot
  they are. A slot is written as `ESC[38;5;n m`, the spelling the picker draws it in, and a slot
  swatch as `label<TAB>slot 196`. The colour dial makes 24-bit colours only, so a new swatch is
  always one, and a colour kept from `F2` makes a slot swatch one too, and says so; kept unchanged,
  a slot stays a slot. A swatch cannot follow a slot, since what a slot looks like is the
  terminal's to say.
* **A file with colours but no palette still loads**: every colour found gets a swatch with a
  counted name, because every colour in a drawing must have one.
* **Rows are trimmed of trailing spaces on save**, so a ragged drawing stays ragged.
* Every glyph must be exactly one terminal column wide. Braille, box drawing and block elements
  are fine; emoji and CJK are rejected with a line and column. Tabs are refused.
* **UTF-8 only.** A UTF-8 byte-order mark, which Windows editors like to write, is taken off; a
  UTF-16 file is refused with a message saying so.
* **A drawing past 10,000 columns or rows, or past 10 million cells, is asked about before it is
  built**, with what it will cost in memory. Every row is padded to the widest, so one long line in
  a small file can need a great deal. Declining exits cleanly without opening anything.

The marker line is visible when the file is printed — a deliberate compromise, isolated in one
module so it can be changed.

## Keys

| Key | On a swatch | On `[+]` | On a cell |
|---|---|---|---|
| `↑` `↓` `←` `→` (Tab, Shift-Tab) | move | move | move — painting or erasing the cells passed over while a pen is down |
| `Shift` + arrows | scroll the window without moving the cursor |||
| `PageUp` / `PageDown` | — | — | jump a screen up or down, painting nothing on the way |
| `Space` / `Enter` | pick it as the brush | choose a colour on the dial, then name it | **hold** to paint while moving; a tap paints one cell |
| `Backspace` | (while naming: delete a character) | — | **hold** to erase while moving; a tap erases one cell |
| `b` | — | — | **toggle** painting: the pen stays down until `b` again |
| `Delete` | — | — | **toggle** erasing: the pen stays down until `Delete` again |
| `i` | — | — | pick up the colour under the cursor as the brush |
| `]` / `[` | grow / shrink the pen: a square from 1×1 up to the canvas's longer side |||
| `F2` | choose a new colour on the dial, then rename it | — | — |
| `F5` | clear the terminal and redraw everything |||
| `F6` | split the art into a solid-colour canvas over a coloured preview, or join it back |||
| `Shift+F6` | the same split, side by side, or join it back |||
| `c` | in a split, show or hide the cursor on the preview |||
| `Ctrl+S` | save to the file it was opened from |||
| `Ctrl+Z` / `Ctrl+Shift+Z` | undo / redo — hold to keep going; a whole drag is one undo. `Ctrl+Y` redoes too, on every terminal |||
| `Ctrl+X` / `Esc` | close — warns first if there is unsaved work, closes on the second press |||
| `Ctrl+C` | quit at once — unsaved work is kept in `<file>.arterminal.tmp` |||

**The hint lines** under the status row follow the cursor and say only what works right now:
`Program:` for the file, `Draw:` for colour and the pen, `Display:` for the view. Scrolling is
offered only when the picture is bigger than the window. On a short terminal the display line goes
first, then the draw line, so the drawing keeps at least three rows; on a narrow one the titles go
before any key does. The way out is never dropped.

**The memory line.** Once the picker holds more than 1 GB, a red line under the hints says how
much, and how much of it is the image and how much the undo history. Nothing is capped: a big pen
dragged over a large canvas records every cell it touches, about 48 bytes each, and the line is
there so that is never a surprise. Reopening the file starts a fresh history.

**Hold keys and toggle keys.** Space, Enter and Backspace work while held and never latch: the
pen is down exactly while the key is. Held together, the last one pressed is in charge, and letting
it go hands the pen back to the one still down. `b` and `Delete` latch on one press and unlatch on
the next. The toggles need nothing from the terminal, so they drag the same way everywhere.

**The pen's size.** `]` grows the pen to 2×2, 3×3 and so on, up to the canvas's longer side, and
`[` shrinks it back to 1×1. The square is centred on the cursor, leaning right and down when its
size is even, and clipped at the edges. The cursor draws its whole footprint, so what the next
press will cover is always visible, and a tap of a big pen is still a single undo.

**Holding needs something that reports key releases.** A classic terminal reports only key
*presses*: holding Space auto-repeats it, and letting go sends nothing. arterminal takes releases
from the first of these that works:

1. **The terminal**, through the kitty keyboard protocol, which arterminal asks for at startup.
   kitty, WezTerm, foot, Ghostty and others speak it, and there is nothing to set up.
2. **The input devices**, on Linux, when the terminal cannot report releases. The kernel knows
   which keys are physically down, below X11, Wayland and the console alike. This needs read access
   to `/dev/input`: run with `--su`, or join the `input` group — see
   [Holding through the input devices](#holding-through-the-input-devices).
3. **Neither**: a hold key paints or erases the one cell under the cursor, and a notice at startup
   says so. The toggles still drag.

The hint line says which behaviour this terminal gets, and the status row always shows whether the
pen is down and what holds it. tmux passes the protocol's encoding but not its release events, so
expect taps there. A pen whose release never arrives is caught at the next press of its key: it is
lifted, the keys tap from then on, and the status row says why.

**Scrolling.** A drawing bigger than the terminal scrolls, palette and all. The window follows the
cursor and moves only as far as it must when the cursor reaches an edge, so the picture holds still
while you draw inside it. `Shift` with an arrow moves the window on its own, which is how the split
view's preview is seen; the next cursor move brings the window back. The status row says what is
showing — `rows 5-40 of 200 · cols 1-60 of 78` — whenever some of it is not, and the status and
hint rows stay at the bottom at every size.

**The colour dial.** `[+]` and `F2` open a dial where the swatch's row is, or will be — on a
random colour nothing holds for a new swatch, on the swatch's own colour for `F2`:

```
                       ^
> ███     H:  16 ; S:  85 ; B:  89  #e25822
                       v
          Max S if/when typing: 100. Or: #rrggbb
```

`←` `→`, or `Tab` `Shift+Tab`, choose hue, saturation or brightness, and the `^` `v` point at the
one chosen. `↑` `↓` turn it a degree or a percent, `PageUp` `PageDown` ten, and holding keeps
turning. Or type it, with no mode to switch into: digits go straight into the value, taking effect
at each one — `1` `2` `0` is 120 — and `#` with six hex digits, or three for the shorthand, sets
exactly that colour, the one way to name a colour turning cannot land on. `Backspace` takes a digit
back. A hex colour half typed holds the other keys until it is finished, and `Esc` gives it up. The
blue line under the dial gives the most the chosen value takes when it is typed — 359 for hue, 100
for saturation and brightness — and the hex colour that will do instead.

Beside the dial, on black so that nothing behind them interferes, three strips show each range
from its top (highest) to its bottom, with a `<` at the value: the hue strip is always the pure
wheel, and saturation and brightness are slices through the colour as it is. A value that turning
would not change right now — the hue of a grey, the saturation of black — is dim. While `F2`'s dial
is turned, the drawing shows the new colour, but nothing changes until `Enter` or `Space` keeps it
and goes on to naming; `Esc` gives it up. A colour another swatch holds can be dialled past but not
kept, and the status row says whose it is. A dial turned and turned back is no change: the colour
comes back to the bit, so nothing is recoloured and the file stays clean.

**Naming.** Keeping a colour on the dial, for `[+]` or `F2`, drops straight into naming it: the
row shows the name being typed with a prompt in the swatch's own colour. Type, `Backspace` to
fix, `Enter` to keep, `Esc` to leave it as it was. A name the palette will not take (a duplicate,
or empty) is refused with a notice and left on screen to be corrected rather than thrown away. The
colour and the name are one thing done, so they are one undo: `Ctrl+Z` takes the new swatch away,
or gives back both the old colour and the old name.

**The split view** shows the drawing twice: a canvas where every cell is a solid block of its
colour — black where it has none — so coverage is easy to judge, and the art as it will really
look. `F6` stacks them, `Shift+F6` puts them side by side, each in half the width. The cursor
lives on the canvas; the preview is read-only, but marks the cursor's place with the canvas's own
`+` on a filled cell, which `c` hides when it covers the glyph being judged.

**The border** around the art is a solid band of grey. It is drawn as coloured spaces rather than
box-drawing lines, which some terminals draw two columns wide, so with colour switched off it takes
its cells but cannot be seen.

**Redo** is `Ctrl+Shift+Z` wherever the terminal can tell it from `Ctrl+Z`: one that speaks the
kitty keyboard protocol reports the Shift. A classic terminal sends the very same byte for both
chords, so there `Ctrl+Shift+Z` can only undo, and `Ctrl+Y` is the redo. `Ctrl+Y` works on every
terminal, and the hint line names whichever redo this one has.

The cursor does not wrap: a canvas is a plane, and holding `↑` stops at the top edge.

### Holding through the input devices

On Linux, `/dev/input/event*` is readable by root and the `input` group only. There are two ways
in, and they trade against each other.

**Per run: `--su`.** `arterminal --su art.txt` asks sudo for the keyboards for that run only. It
explains itself before the password prompt, re-runs itself under sudo, opens the keyboards, and
then becomes you again at once — groups, group and user, each checked, and verified afterwards so
that root cannot be regained. The art file is read only after that, so `--su` never shows or saves
a file you could not open yourself. sudo itself is taken from its system location, never from
`PATH`, so a planted `sudo` cannot be the thing that asks for your password. A sudo ticket the run
created is revoked when it ends; one you already had is left alone. Without a terminal to ask at, it goes ahead without held keys rather
than block. sudo resets the environment, so settings such as `NO_COLOR` do not reach the session.

**Standing: the `input` group.** Join it, then log out and back in, since membership is applied at
login (`newgrp input` tries it in the current shell first):

```sh
sudo usermod -aG input "$USER"
```

**Know what the group grants.** Membership of `input` lets *any* program you run read *every*
input device on the machine: full keylogging, passwords typed into any application included.
arterminal uses a sliver of it. Beyond finding which devices are keyboards, it asks the kernel one
question, "which keys are down right now?", at startup, on each key the terminal delivers, and
while a stroke is held. It never reads the stream of keystrokes, so none of them pass through it,
and since every stroke begins with the terminal's own key press, nothing pressed in another window
can paint. The group knows none of that, and neither does anything else you run. If the trade is
not worth holding Space, use `--su` instead, or skip both: the toggles drag without either.

Over ssh, the devices belong to the machine arterminal runs on, not the keyboard being typed on.
When no device ever shows the key behind a press, arterminal says so once and the keys tap.

## Layout

| Path | What lives there |
|---|---|
| `src/lib.rs` | The public API: crate docs and the re-exports every caller uses. |
| `src/color.rs` | `Rgb`, `Hsb`, hex parsing, and the small seeded generator behind random swatches. |
| `src/palette.rs` | `Palette`, `Swatch`, and derived swatches — the colours a drawing may use. |
| `src/canvas.rs` | `Canvas`, `Cell`, and the text loader with its validation rules. |
| `src/cursor.rs` | `Focus` and `Dir`: where the cursor is, and where a key sends it. |
| `src/dial.rs` | `Dial`: a colour chosen by hue, saturation and brightness, a step at a time. Pure. |
| `src/keys.rs` | Bytes from the terminal into key events — presses, repeats, releases. Pure. |
| `src/document.rs` | The file format: art with inline colours, then the palette. Parse and render, pure. |
| `src/ui.rs` | `Picker` — brush, pen, naming, undo history, save/salvage — and `render`, `apply`, `run`. |
| `src/paint.rs` | Getting a frame onto the terminal without flicker. **Ported** — see below. |
| `src/input.rs` | Raw mode, the waits and reads, keystroke coalescing (**ported**), and the keyboard-protocol guard. |
| `src/devices.rs` | Key releases from `/dev/input`, for terminals that report none. Polls which keys are down; never reads keystrokes. |
| `src/elevate.rs` | `--su`: sudo for one run, the keyboards opened, root given back and the drop verified. Linux only. |
| `src/main.rs` | The standalone binary. |
| `examples/skull.txt` | Sample art. |
| `tests/stderr_gate.rs` | The one test that must write `console`'s process-wide colour switches, kept in its own process. |
| `tests/no_colour.rs` | The picker with styling switched off — the dial draws no strips — in a process of its own for the same reason. |
| `tests/pty.rs` | The real binary on a pseudo-terminal, with the test playing the terminal: holding, saving, salvage, `--su` without a terminal, the startup questions, width measuring, the colour dial. |
| `*_img_test.txt` | Sample art at two sizes. |

## Design notes

**Three dependencies**, `console`, `libc` and `unicode-width`, each with its reason written into
`Cargo.toml`; the last two were already in the tree through `console`. `console` is pinned at
`0.16.6` rather than `0.16`: 24-bit colour only arrived in `0.16.2`, and `0.16.6` fixed the
truncation the clipping was first built on, before it became its own. Its default features are
load-bearing: dropping them silently degrades width measurement to `chars().count()`, with no
compile error and a sheared grid at runtime.

**Rendering and key handling are pure functions.** `ui::render` turns state into lines and
`ui::apply` turns a keystroke into a change; `ui::run` is the small impure loop that connects
them to a terminal. So the whole picker is tested without a terminal anywhere — and a program
that already owns its screen can call the first two itself and composite the result. The one
thing a key can ask for that needs a file, saving, comes out of `apply` as `Action::Save` for the
caller to perform.

**Keys are decoded here, not by `console`.** Two defects forced it, and either alone would have.
`console` has no notion of key releases, so "stop painting when Space is let go" could not be
heard at all. And its reader takes an `ESC` plus at most three bytes, with no loop to a CSI final
byte and no SS3 branch: every F-key splits into an unknown escape plus a stray typed character,
every time. `keys.rs` scans to the real final byte, understands the kitty protocol, and waits for
the rest of a sequence rather than tearing it — briefly for a lone `ESC`, which may be the Escape
key, and generously for anything else unfinished, since no valid input ends there.

**Ctrl+C is a key, not a signal.** Raw mode turns signal generation off, so `Ctrl+C` arrives as the
byte `0x03` and is decoded like any other chord. Nothing ever raises `SIGINT`, which is why the
picker never needs a handler for it.

**What the keyboard-protocol guard cannot cover.** The protocol's flags are pushed at startup and
popped by a `Drop` guard, which runs on normal return and on panics — but not on
`process::exit`, `abort`, or a signal such as `SIGTERM`. If the process is killed while running,
the shell is left receiving every key as an escape sequence; closing the terminal tab recovers it.
A signal handler would close that gap and is a known follow-up, not yet built.

**Undo carries a recolour and a rename with it.** The history records colours and labels, not
references. When a swatch moves, every cell holding its old colour is rewritten and so is every
history entry mentioning it — otherwise undoing an old paint would put a colour back on the canvas
that no swatch owns. A rename is carried into the history the same way, so undoing the `[+]` that
made a swatch still finds it under the name it has now.

**Undo and redo are all or nothing.** One undo can hold several changes — a drag of the pen, or a
colour and the name given it — and the palette can refuse some of them, as when an edit made in
code takes a colour an undo would give back. So every change is checked first, on a copy of the
palette, and an undo that cannot happen whole does not happen at all; the status row says why.

**Ctrl+C salvages rather than argues.** An interrupt that stopped to ask about unsaved work would
not be an interrupt; one that silently dropped the work would be worse. So `Ctrl+C` writes any
unsaved state to `<file>.arterminal.tmp` — a real document that reopens — and leaves the original
untouched. Nothing is written when there is nothing unsaved, or when the picker was built in code
with no file behind it.

**Function keys are reassembled, not trusted.** `console` has no notion of an F-key and splits the
escape that spells one across two reads, leaving the tail to surface as a typed letter on the next.
`Picker::decode` remembers the first half and completes it, recognising F1/F3/F4 only to swallow
them so a stray `P`/`R`/`S` never lands in a name.

**`paint` and `input` are ported** from the sibling project `terminal_choice` (commit `4f7a222`),
which solved the flicker problem first. Both projects are GPL-3.0-only and share an owner. The
module headers carry the provenance and the reasoning; the short version is that one repaint is
one write to a buffered terminal, and lines are overwritten in place rather than cleared first,
because "erase n lines then draw n lines" describes an empty screen even when it arrives in one
piece. A shared crate is the right long-term answer and is deliberately not built yet: the
interface is still moving, and freezing it now would freeze the wrong shape.

**Every style is built for stderr, and a test enforces it.** `console` emits an escape only when
it believes the destination is a terminal — and the stream a bare `Style` asks about is *stdout*.
The picker draws on stderr so that stdout stays free, which means a default-built style consults
the redirected file under this crate's own advertised `arterminal art.txt > palette.txt` and emits
nothing at all. Attributes sit inside the same gate, so `.dim()` and `.bold()` go with the
colours. Every style therefore comes from one `ui::stderr_style()` helper — public, so that the binary and
any embedder can follow the same rule rather than hand-copying it — and a test scans the source
tree so a new call site cannot quietly reintroduce the bug. It checks three things, because the
obvious one check is walkable: a bare `Style::new()` without `.for_stderr()`, the stdout-gated
free function, and any `use console::…` importing a style name, which is what would otherwise let
an alias slip past the first two. Note that none of this is visible to `cargo test` — under a test
harness neither stream is a terminal — so the bug was found by driving the real binary under a
pty, and it stays found by the scan.

**The frame never exceeds the terminal.** A frame taller than the screen scrolls, and `\x1b[nA`
clamps at the top margin rather than un-scrolling — so the cursor arithmetic breaks permanently,
not cosmetically. `paint::height_budget` is one row shorter than the terminal because every line
ends with a newline, the last one included. Neither limit is silent: art that does not fit scrolls,
with the status row saying which rows and columns are showing, and any other line too wide ends in
`…`.

**Widths are measured the terminal's way.** Some terminals draw East Asian Ambiguous characters —
`·`, `—`, `…`, block elements — two columns wide, and a line measured the narrow way would then
wrap and throw every later row off. At startup arterminal draws a `·`, asks the terminal where the
cursor went, and erases it again; the locale is used only when the terminal gives no answer. Every
line is then clipped to what the terminal really draws. This changes only the measuring, never
which glyphs a canvas accepts, so a file loads the same on every machine. On a terminal set wide,
art using such glyphs looks sheared, since each takes two columns there, but the frame keeps its
place.

**Compose style attributes; never concatenate escape strings.** A rendered string contains its
own `\x1b[0m`, and a reset inside a hand-rolled reverse-video wrapper ends the inversion partway
along the run — so wrapping obliges every site to re-arm after each embedded reset, and one
forgotten site is a bug. Nothing here wraps: focus on a swatch is a plain-text gutter mark beside
the colour rather than around it, and a run of canvas cells folds ink and inversion into a single
style applied once to their glyphs, which are plain characters and cannot contain a reset. The
full rule, and the future feature most likely to break it, are in the `ui` module docs.

**A frame costs what it shows, not what is painted.** Every keystroke redraws the whole frame, so
what matters is its size in bytes. Cells that look alike are drawn as one styled run rather than a
colour escape each, which took a 6×6 pen dragged over the large sample art from 34 KB a frame to
7 KB, about what the same picture costs unpainted. The pen's own arithmetic is a microsecond or
two per move at any size.

**Colour is identity; a label is a name.** No two swatches may hold the same colour, because a
canvas cell records the colour itself rather than a reference — so if two shared one, nothing
could say which swatch a given pixel belonged to. Editing a swatch still recolours the drawing,
but by rewriting every cell holding that colour (`Picker::set_swatch_color`) rather than by
indirection. That is what keeps a canvas printable as it stands, and it is sound only while
colours stay unique.

Labels are unique for a separate reason: a **derived** swatch follows another — "the same hue, a
third darker" — and it cannot name its base by colour, since the colour is precisely what moves.
So the label is the stable handle, and renaming rewrites whoever pointed at the old name. A move
that would put two swatches on one colour is refused whole, palette and canvas both, and the error
names the swatch that blocked it.

**Colours are picked in HSB and stored as RGB.** `Hsb` exists for the moment someone is choosing,
because "a bit more orange" is a thought about hue and an unreachable one about three independent
channels. Hue is counted in 1530 steps rather than 360 degrees, and that is measured rather than
tidy: in degrees a sextant holds sixty hues spread across a range of 255, so integer HSB can name
only 58.6% of the 16.7M colours and a round trip through an editor changed a colour two times in
three, by as much as 5 per channel. At 1530 steps nothing is spread — 80.7% return exactly and the
rest by at most 1, with every remaining failure below full brightness. Show degrees if they read
better; do not store them.

Plain-text art loads with no ink anywhere.

## Not yet built

* **Derived swatches in the UI.** The palette and the file support them; nothing creates one yet.
* **Removing a swatch by hand.** Only undo removes one today.
* **A signal handler** restoring raw mode and the keyboard flags on `SIGTERM` — see above.
* **The alternate screen.** The picker draws inline, below the shell prompt, so a resize reflows
  its old frames into the scrollback; `F5` repairs the damage by hand. Drawing on the terminal's
  alternate screen, as full-screen programs do, would remove the cause.
* **A warning when a file with very many colours is loaded**, since each becomes a swatch.
