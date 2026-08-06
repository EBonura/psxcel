# Spreadsheet on the PlayStation 1: feasibility

> Historical note: this is the original feasibility writeup, kept for context.
> The numeric-type and save/load sections below have been corrected to match
> what actually shipped; `README.md` describes the current feature set.

**Verdict: easily feasible, and implemented.** A spreadsheet is one of the
cheapest "serious software" targets for the PS1 because it needs none of the
hardware the console is built to stress. No 3D, no textures beyond a font, no
CD streaming, no audio. It is a 2D text grid, a controller, and integer math.

## What the platform already gives you

Building on the PSoXide Rust SDK, the entire runtime is free:

- **`psx-engine` `App`/`Scene`** drives the boot, the 60 Hz loop, double
  buffering, VBlank pacing, pad polling, and present. The app is three methods:
  `init`, `update`, `render`.
- **`psx-font`** uploads an 8x8 bitmap font into VRAM once and draws tinted text
  with a 4-word GP0 rectangle per glyph. A 320x240 screen is a 40x30 character
  terminal, plenty for a grid.
- **`psx-pad`** exposes the digital buttons and D-pad through `ctx.pad` /
  `ctx.just_pressed`.
- **`psx-gpu::draw_quad_flat`** draws the cursor highlight (draw-offset aware, so
  it is correct under double buffering, unlike the raw VRAM `fill_rect`).

## The one real design problem: text entry

The PS1 has no keyboard in the general case. Cell contents (numbers, text, and
formulas like `=SUM(A1:A9)`) all have to be typed with a gamepad. The honest,
general solution is an **on-screen keyboard**: a character grid the D-pad moves
over, X inserts, L1 deletes, START commits. This is the same trick arcade name
entry and this repo's sibling `zelda3-psx` "REGISTER YOUR NAME" screen use. It is
slower than a keyboard but it types anything, so formulas are fully expressible.

## The one constraint that bites: integer-only math

The PS1 CPU has no FPU, so a spreadsheet's decimals have to be fixed-point
integers. Values are **`i64` fixed-point with four decimals** (the `DECIMALS`
knob in `src/sheet.rs`): `12.5` is stored as `125000`. Multiply is `a*b/ONE`,
divide is `a*ONE/b`, both done in `u64` magnitude for range. Overflow returns
`#ERR` rather than wrapping.

Getting there took a real compiler bug. The first headless render showed
correct layout but garbage cell values (`-21454`) because the compiler's
*signed* 64-bit divide (`__divdi3`) is broken on `mipsel-sony-psx`; this note
originally reacted by declaring `i64` forbidden and storing values as `i32`
hundredths. The bug was later root-caused and fixed in the SDK (`psx-rt`
overrides the broken builtins, proven by the `hello-i64probe` example), which
unlocked the `i64` representation the sheet uses today. What survives of the
old rule is about performance, not correctness: 64-bit math is slow on a
32-bit CPU, so it stays out of per-frame hot paths. A recalc that runs only on
edit is fine.

## What was built

See `README.md`. A-Z x 1-50 grid, cell references, `+ - * / ()`, and
`SUM/AVG/MIN/MAX(range)`, fixed-point decimals, scrolling viewport, on-screen
keyboard, all verified rendering correctly under the PSoXide emulator.

## Inspiration

Modelled on the open-source terminal spreadsheet **`sc` / `sc-im`** (the classic
Unix spreadsheet calculator): lettered columns, numbered rows, `A1` references,
and range functions. The formula grammar here is a small subset of theirs.

## Where it stops (deliberate)

- No cell dependency graph: recalc is a naive fixpoint over all cells on each
  edit. Fine at 1300 cells; add a topo sort only if edits feel slow.
- Append/backspace text entry only, no mid-string caret.
- Save/load: this note originally called persistence a next step, and it has
  since shipped. Sheets save to the memory card as named, compressed files via
  the `psx-mc` SDK crate (multi-file, with an in-game browser to load or
  delete).
