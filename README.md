# arterminal

Terminal ASCII art with colour. A library first — every feature the bundled `arterminal` binary
has is a function in the crate, so a program embedding this is never the second-class caller.

Open a text file of art, pick colours into a palette, paint them onto the characters, save. The
colours are written back into the same file as terminal escapes, so `cat` shows the result.

```
███     # colour 1  (brush)
███     # colour 2
[+]

⣿⡇⣿⣿⣿⠛⠁⣴⣿⡿⠿⠧⠹⠿⠘⣿⣿⣿⡇⢸⡻⣿⣿⣿⣿⣿⣿⣿
⢹⡇⣿⣿⣿⠄⣞⣯⣷⣾⣿⣿⣧⡹⡆⡀⠉⢹⡌⠐⢿⣿⣿⣿⡞⣿⣿⣿
⣾⡇⣿⣿⡇⣾⣿⣿⣿⣿⣿⣿⣿⣿⣄⢻⣦⡀⠁⢸⡌⠻⣿⣿⣿⡽⣿⣿
brush: colour 1 · modified
↑↓←→ move · space/enter pick or paint · backspace erase · ^S save · ^Z undo · ^Y redo · esc/^X close
```

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

One argument: the file to edit. The picker draws on **stderr**; stdout carries nothing, because
everything the session produces goes back into the file on `Ctrl+S`. Exit codes: `0` the picker
ran, `2` the file or the terminal was unusable.

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
* **Colours are inline terminal escapes**, so printing the file shows the drawing coloured. Only
  the 24-bit foreground sequence sets a colour; everything else in a hand-made file is passed over
  with its text kept.
* **A file with colours but no palette still loads**: every colour found gets a swatch with a
  counted name, because every colour in a drawing must have one.
* **Rows are trimmed of trailing spaces on save**, so a ragged drawing stays ragged.
* Every glyph must be exactly one terminal column wide. Braille, box drawing and block elements
  are fine; emoji and CJK are rejected with a line and column. Tabs are refused.

The marker line is visible when the file is printed — a deliberate compromise, isolated in one
module so it can be changed.

## Keys

| Key | On a swatch | On `[+]` | On a cell |
|---|---|---|---|
| `↑` `↓` `←` `→` (Tab, Shift-Tab) | move | move | move |
| `Space` / `Enter` | pick it as the brush | add a random colour | paint with the brush |
| `Backspace` / `Delete` | — | — | erase |
| `Ctrl+S` | save to the file it was opened from |||
| `Ctrl+Z` / `Ctrl+Y` | undo / redo |||
| `Esc` / `Ctrl+X` | close — warns first if there is unsaved work, closes on the second press |||
| `Ctrl+C` | close at once, unsaved or not |||

Redo is `Ctrl+Y` rather than `Ctrl+Shift+Z` because a classic terminal sends the same byte for
both, so the two cannot be told apart. The cursor does not wrap: a canvas is a plane, and holding
`↑` should stop at the top edge rather than teleport to the bottom one.

## Layout

| Path | What lives there |
|---|---|
| `src/lib.rs` | The public API: crate docs and the re-exports every caller uses. |
| `src/color.rs` | `Rgb`, `Hsb`, hex parsing, and the small seeded generator behind random swatches. |
| `src/palette.rs` | `Palette`, `Swatch`, and derived swatches — the colours a drawing may use. |
| `src/canvas.rs` | `Canvas`, `Cell`, and the text loader with its validation rules. |
| `src/cursor.rs` | `Focus` and `Dir`: where the cursor is, and where a key sends it. |
| `src/document.rs` | The file format: art with inline colours, then the palette. Parse and render, pure. |
| `src/ui.rs` | `Picker` — brush, edits, undo history, save — and `render`, `apply`, `run`. |
| `src/paint.rs` | Getting a frame onto the terminal without flicker. **Ported** — see below. |
| `src/input.rs` | Raw mode, the blocking wait, and keystroke coalescing. **Ported.** |
| `src/main.rs` | The standalone binary. |
| `examples/skull.txt` | Sample art. |
| `tests/stderr_gate.rs` | The one test that must write `console`'s process-wide colour switches, kept in its own process. |
| `*_img_test.txt` | Sample art at two sizes. |

## Design notes

**Two dependencies**, `console` and `libc`, each with its reason written into `Cargo.toml`.
`console` is pinned at `0.16.6` rather than `0.16` for two independent reasons recorded there —
24-bit colour only arrived in `0.16.2`, and `0.16.6` fixed the truncation machinery the width
clipping is built from. Its default features are load-bearing: dropping them silently degrades
width measurement to `chars().count()`, with no compile error and a sheared grid at runtime.

**Rendering and key handling are pure functions.** `ui::render` turns state into lines and
`ui::apply` turns a keystroke into a change; `ui::run` is the small impure loop that connects
them to a terminal. So the whole picker is tested without a terminal anywhere — and a program
that already owns its screen can call the first two itself and composite the result. The one
thing a key can ask for that needs a file, saving, comes out of `apply` as `Action::Save` for the
caller to perform.

**Ctrl+C is read as a key, not a signal.** `console`'s plain `read_key` answers Ctrl+C by raising
`SIGINT` on the process — the terminal is raw, so nothing else would — and the default handler
ends the process before the raw mode can be undone, leaving the shell raw with its cursor hidden.
`run` uses `read_key_raw`, which hands Ctrl+C back as a keystroke to be closed on like any other.

**Undo carries a recolour with it.** The history records colours, not swatch names. When a swatch
moves, every cell holding its old colour is rewritten and so is every history entry mentioning
it — otherwise undoing an old paint would put a colour back on the canvas that no swatch owns.

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
ends with a newline, the last one included. Neither limit is silent: a canvas too tall ends in a
count of the dropped rows, and a line too wide ends in `…`.

**Compose style attributes; never concatenate escape strings.** A rendered string contains its
own `\x1b[0m`, and a reset inside a hand-rolled reverse-video wrapper ends the inversion partway
along the run — so wrapping obliges every site to re-arm after each embedded reset, and one
forgotten site is a bug. Nothing here wraps: focus on a swatch is a plain-text gutter mark beside
the colour rather than around it, and a canvas cell folds ink and inversion into a single style
applied to a single `char`, which cannot contain a reset. The full rule, and the future feature
most likely to break it, are in the `ui` module docs.

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

* **A scrolling viewport.** Today a canvas taller than the terminal is clipped with a marker.
* **A colour editor.** Swatches are random from `[+]`; `Picker::set_swatch_color` exists and is
  tested, but no key reaches it. When one does, it must commit only if the user actually moved
  something — an editor that wrote back on every close would round-trip the colour through HSB
  for nothing and, because recolouring rewrites the matrix, dirty the whole drawing on a no-op.
* **Derived swatches in the UI.** The palette and the file support them; nothing creates one yet.
* **Recolouring in the undo history.** A swatch's move rewrites the history but is not itself an
  entry in it.
* **Removing a swatch by hand.** Only undo removes one today.
* **A warning when a file with very many colours is loaded**, since each becomes a swatch.
