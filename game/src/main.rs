//! PSXcel -- a gamepad-driven spreadsheet for the PlayStation 1,
//! built on the PSoXide Rust SDK. Grid + formula bar + an on-screen keyboard
//! for text entry (the PS1 has no keyboard, so cell text is typed with the
//! D-pad, the same trick arcade name-entry uses).
//!
//! Inspired by the open-source terminal spreadsheet `sc`/`sc-im`: A-Z columns,
//! numbered rows, cell references (A1), and =SUM/AVG/MIN/MAX ranges.
//!
//! Controls
//!   Nav:  D-pad move (L2+D-pad pages)   X edit   TRIANGLE new entry
//!         SQUARE clear   L1 copy   R1 paste   R2 mark a selection
//!         O drop the selection   START undo (L2+START home)   SELECT menu
//!   Edit: PS4-style keyboard (wraps) -- D-pad pick, X type/shift/symbols,
//!         L2+LEFT/RIGHT walk the caret (L2+UP/DOWN home/end), L1 or SQUARE
//!         backspace, R2 jump to OK, START commit, R1 point-at-a-cell, O back.
//!   Pick: D-pad roam the grid, X insert that cell's ref into the formula, O back.
//!   Chart: SELECT -> Chart of selection; view it as a bar/line/area/pie chart
//!          (TRI cycles, O back). Formulas are case-insensitive.
//!   Help:  SELECT -> Help, for the same table on-screen.

// Host `cargo test` (the sheet.rs suite) builds this crate with std and the
// libtest harness; the PSX build keeps no_std/no_main. Only the tests are
// live under cfg(test), so silence the resulting dead-code lint there.
#![cfg_attr(not(test), no_std)]
#![cfg_attr(not(test), no_main)]
#![cfg_attr(test, allow(dead_code, unused_imports))]
#![allow(static_mut_refs)]

extern crate psx_rt;

mod sheet;
mod theme;

use psx_engine::{button, App, Config, Ctx, Scene};
use psx_font::{fonts::SPLEEN_5X8, FontAtlas};
use psx_gpu::{draw_line_mono, draw_quad_flat, draw_rect_flat, draw_tri_flat};
use psx_math::fmt::u32_dec;
use psx_math::sincos::{cos_q12, sin_q12};
use psx_mc::{Card, Entry, Error as McError, HardwareCard, Slot};
use psx_osk::{Action, Dir, Keyboard};
use psx_pad::PadTracker;
use psx_vram::{Clut, TexDepth, Tpage};
use theme::{theme, Rgb};

use sheet::{fmt_value, fmt_value_fit, idx, Kind, Sheet, COLS, ROWS};

const FONT_TPAGE: Tpage = Tpage::new(320, 0, TexDepth::Bit4);
const FONT_CLUT: Clut = Clut::new(320, 256);

// Layout (320x240, Spleen 5x8 font: 5px advance, 8px tall).
const GW: i16 = 5; // glyph advance (SPLEEN_5X8.advance_x)
const DISP: usize = 7; // characters shown per cell
const COL_W: i16 = (DISP as i16) * GW; // 35px column
const ROWHDR_W: i16 = 16; // room for a 2-digit row number
const FBAR_Y: i16 = 2; // formula bar text
const COLHDR_Y: i16 = 12; // column-letter header row
const GRID_Y0: i16 = 22; // first data row
const ROW_H: i16 = 8;
const VIS_COLS: usize = ((320 - ROWHDR_W) / COL_W) as usize; // 8
const VIS_ROWS: usize = 24;
// Bottom edge of the scrolling grid band (the GPU scissor that clips a
// half-scrolled row so it can't bleed into the header or the command bar).
// A cell's pixels run from `y - 1`, so a band has to end on `TOP + n*ROW_H - 1`
// to finish on a whole row instead of slicing one lengthwise.
const GRID_TOP: i16 = GRID_Y0 - 1;
const GRID_Y1: i16 = GRID_TOP + (VIS_ROWS as i16) * ROW_H - 1;
/// The same, for when the on-screen keyboard is covering the lower screen.
const KBD_GRID_Y1: i16 = GRID_TOP + ((psx_osk::Y0 - 14 - GRID_TOP) / ROW_H) * ROW_H - 1;

/// Control hint on the on-screen keyboard's panel (Edit mode).
const KBD_HINT: &str = "X:Key L2+<>:Caret SQ:Del R2:OK R1:Cell O:Back";

/// Frames a toast stays up, and how many of those it spends fading out.
const TOAST_TTL: u16 = 150;
const TOAST_FADE: u16 = 40;

// The grid lives in a static: ~40KB is too large for the PS1 boot stack.
static mut SHEET: Sheet = Sheet::new();

// -- Undo ------------------------------------------------------------------
//
// Each entry is a whole-sheet snapshot in the same compact record stream
// `Sheet::serialize` writes for the memory card, so undo reuses the save format
// and adds no second serializer. A snapshot only stores non-empty cells, so a
// realistic sheet is a few hundred bytes.
//
// ponytail: full snapshots, not per-cell deltas. A delta would need its own
// encoding for "insert row shifted 1300 cells"; snapshots handle every mutation
// with one code path. The ceiling is UNDO_SLOT_LEN: a sheet too dense to
// serialize drops the history (see `push_undo`) rather than restoring a stale
// state. Move to deltas only if that limit is ever actually hit.
const UNDO_SLOTS: usize = 6;
const UNDO_SLOT_LEN: usize = 4096;
static mut UNDO_RING: [[u8; UNDO_SLOT_LEN]; UNDO_SLOTS] = [[0; UNDO_SLOT_LEN]; UNDO_SLOTS];
static mut UNDO_LEN: [usize; UNDO_SLOTS] = [0; UNDO_SLOTS];

/// Cells the rectangular clipboard holds. Beyond this a copy is truncated (and
/// says so), which beats silently pasting a partial rectangle.
const CLIP_MAX: usize = 64;

// PSXcel card files are named "PSXCEL-<NAME>" so the browser can pick ours out
// of any other game's saves. Serialize/compress scratch buffers live in statics
// (off the small boot stack).
const SAVE_PREFIX: &str = "PSXCEL-";
/// A card holds at most 15 files (one per 8 KiB block).
const MAX_FILES: usize = 15;
static mut SAVE_BUF: [u8; 8192] = [0; 8192];
static mut COMP_SCRATCH: [u8; 8192] = [0; 8192];

/// A zeroed directory entry, for the browse-list array.
const EMPTY_ENTRY: Entry = Entry {
    name: [0u8; psx_mc::MAX_NAME + 1],
    name_len: 0,
    blocks: 0,
};

/// Build a card filename from a user-typed save name: `PSXCEL-<NAME>`, the name
/// uppercased and reduced to `A-Z0-9`, capped at 20 chars total. Returns the
/// length written, or 0 if nothing usable remained.
fn make_filename(name: &[u8], out: &mut [u8; 20]) -> usize {
    let mut n = 0;
    for &b in SAVE_PREFIX.as_bytes() {
        out[n] = b;
        n += 1;
    }
    let base = n;
    for &b in name {
        if n >= 20 {
            break;
        }
        let c = b.to_ascii_uppercase();
        if c.is_ascii_alphanumeric() {
            out[n] = c;
            n += 1;
        }
    }
    if n == base {
        0
    } else {
        n
    }
}

/// A card file's display name (the `PSXCEL-` prefix stripped).
fn display_name(entry: &Entry) -> &str {
    let s = entry.name();
    s.strip_prefix(SAVE_PREFIX).unwrap_or(s)
}

/// Short on-screen code for a memory-card error.
fn mc_err(e: McError) -> &'static str {
    match e {
        McError::NoCard => "NO CARD",
        McError::NotFound => "NO SAVE FOUND",
        McError::NoSpace => "CARD FULL",
        McError::BadChecksum => "CARD CHECKSUM",
        McError::Protocol => "CARD ERROR",
        McError::BufferTooSmall => "SAVE TOO BIG",
        McError::BadName => "BAD NAME",
        _ => "CARD FAULT",
    }
}

/// Best-effort check at boot: is there any PSXcel save on the card? Any error
/// (no card, unformatted) reports `false`.
fn card_has_save() -> bool {
    let mut card = Card::new(HardwareCard::new(Slot::One));
    let mut entries = [EMPTY_ENTRY; MAX_FILES];
    match card.list(&mut entries) {
        Ok(k) => entries[..k]
            .iter()
            .any(|e| e.name().starts_with(SAVE_PREFIX)),
        Err(_) => false,
    }
}

#[derive(PartialEq)]
enum Mode {
    /// Boot title screen: pick a sample sheet or a memory-card save.
    Menu,
    Nav,
    Edit,
    /// Mid-edit "point at a cell" mode: roam the grid and insert a reference
    /// into the formula being typed (the Excel/Sheets flow). Also reused, with
    /// `chart_pick`, to select the data range for a chart.
    Pick,
    /// Full-screen bar/line chart of a selected range.
    Chart,
    /// Full-screen memory-card file browser (load / delete a saved sheet).
    Browse,
    /// The command menu (edit ops, sheet ops, theme, card, help).
    Cmd,
    /// Full-screen controls reference.
    Help,
    /// Modal yes/no over whatever drew last. `confirm` says what it's asking.
    Confirm,
}

/// What a [`Mode::Confirm`] prompt will do if the answer is yes. Only
/// destructive, hard-to-notice-until-too-late actions get one.
#[derive(Copy, Clone, PartialEq)]
enum Ask {
    DeleteSave,
}

#[derive(Copy, Clone, PartialEq)]
enum ChartKind {
    Bar,
    Line,
    Area,
    Pie,
}

impl ChartKind {
    /// Cycle to the next chart type (Triangle in the chart view).
    fn next(self) -> ChartKind {
        match self {
            ChartKind::Bar => ChartKind::Line,
            ChartKind::Line => ChartKind::Area,
            ChartKind::Area => ChartKind::Pie,
            ChartKind::Pie => ChartKind::Bar,
        }
    }
    fn label(self) -> &'static str {
        match self {
            ChartKind::Bar => "BAR",
            ChartKind::Line => "LINE",
            ChartKind::Area => "AREA",
            ChartKind::Pie => "PIE",
        }
    }
}

/// A command-menu action. Separate from the row table so the dispatch is a
/// match the compiler checks, not an index into a list of labels.
#[derive(Copy, Clone, PartialEq)]
enum Cmd {
    Undo,
    Cut,
    Copy,
    Paste,
    InsRow,
    DelRow,
    InsCol,
    DelCol,
    Chart,
    Theme,
    Save,
    Load,
    Help,
    MainMenu,
}

impl Cmd {
    /// Right-hand column in the menu: the pad shortcut where one exists, or the
    /// current value for a setting.
    fn hint(self) -> &'static str {
        match self {
            Cmd::Undo => "START",
            Cmd::Copy => "L1",
            Cmd::Paste => "R1",
            Cmd::Theme => {
                if theme::is_dark() {
                    "DARK"
                } else {
                    "LIGHT"
                }
            }
            _ => "",
        }
    }
}

/// Distinct slice / series colours (readable on both themes).
const SLICE_COLORS: [(u8, u8, u8); 8] = [
    (46, 170, 96),
    (240, 200, 70),
    (80, 150, 225),
    (232, 112, 92),
    (156, 100, 208),
    (90, 205, 185),
    (206, 146, 74),
    (126, 196, 66),
];

struct Editor {
    font: Option<FontAtlas>,
    cur_col: usize,
    cur_row: usize,
    top_col: usize,
    top_row: usize,
    mode: Mode,
    edit_buf: [u8; sheet::INPUT_CAP],
    edit_len: usize,
    /// Insertion point in `edit_buf`, `0..=edit_len`. Typing and backspace both
    /// act here, so a typo mid-formula is fixable without retyping the rest.
    caret: usize,
    kbd: Keyboard,
    /// While in `Pick`, the roaming cursor (the cell being edited stays `cur_*`).
    pick_col: usize,
    pick_row: usize,
    /// Range anchor in `Pick`: `Some` while selecting a rectangle (for `A1:B5`).
    pick_anchor: Option<(usize, usize)>,
    /// Rectangular clipboard: each cell's raw text row-major, plus the source
    /// rectangle's top-left so a paste can shift relative references.
    clip: [[u8; sheet::INPUT_CAP]; CLIP_MAX],
    clip_lens: [u8; CLIP_MAX],
    clip_w: usize,
    clip_h: usize,
    clip_col: usize,
    clip_row: usize,
    /// Nav selection anchor. `Some` once R2 marks a corner; the cursor is the
    /// other corner, so moving the D-pad grows the rectangle.
    sel_anchor: Option<(usize, usize)>,
    /// Command-menu cursor, and the scroll offset when the list is taller than
    /// the panel.
    set_sel: usize,
    menu_top: usize,
    /// Toast: message, remaining frames, and whether it reads as an error.
    status: [u8; 32],
    status_len: usize,
    status_ttl: u16,
    status_bad: bool,
    /// Undo ring cursor: `undo_head` is where the next snapshot goes,
    /// `undo_count` how many are still restorable.
    undo_head: usize,
    undo_count: usize,
    /// Pending confirmation, live only in [`Mode::Confirm`].
    ask: Ask,
    /// Visible-frame counter, mirrored off `Ctx` so `render` can animate.
    frame: u32,
    /// Current vertical scroll in pixels, easing toward `top_row * ROW_H`.
    scroll_px: i16,
    /// Start-menu cursor + whether a memory-card save was detected at boot.
    menu_sel: usize,
    has_save: bool,
    /// True while the Edit keyboard is entering a save name (not a cell).
    saving: bool,
    /// File browser: the card's PSXcel files, the cursor, and where "back" goes.
    files: [Entry; MAX_FILES],
    file_count: usize,
    browse_sel: usize,
    browse_from_menu: bool,
    /// Normalised chart data rectangle (c0, r0, c1, r1) and chart kind.
    chart_range: (usize, usize, usize, usize),
    chart_kind: ChartKind,
    /// SDK button tracker driving the D-pad auto-repeat (fed `ctx.held_mask()`
    /// once per update).
    pad: PadTracker,
}

fn sheet() -> &'static mut Sheet {
    unsafe { &mut *core::ptr::addr_of_mut!(SHEET) }
}

impl Scene for Editor {
    fn init(&mut self, _ctx: &mut Ctx) {
        self.font = Some(FontAtlas::upload(&SPLEEN_5X8, FONT_TPAGE, FONT_CLUT));
        // Boot to the title menu; note whether a card save exists so it can be
        // offered there.
        self.has_save = card_has_save();
    }

    fn update(&mut self, ctx: &mut Ctx) {
        self.frame = ctx.visual_frame.as_u32();
        self.pad.update(ctx.held_mask());
        if self.status_ttl > 0 {
            self.status_ttl -= 1;
        }
        match self.mode {
            Mode::Menu => self.update_menu(ctx),
            Mode::Nav => self.update_nav(ctx),
            Mode::Edit => self.update_edit(ctx),
            Mode::Pick => self.update_pick(ctx),
            Mode::Chart => self.update_chart(ctx),
            Mode::Browse => self.update_browse(ctx),
            Mode::Cmd => self.update_cmd_menu(ctx),
            Mode::Help => self.update_help(ctx),
            Mode::Confirm => self.update_confirm(ctx),
        }
        self.keep_cursor_visible();
        self.ease_scroll();
    }

    fn render(&mut self, ctx: &mut Ctx) {
        let font = self.font.as_ref().unwrap();
        let t = theme();
        // Publish the back buffer's VRAM row for the scissor helpers.
        BUFFER_Y.store(ctx.fb.buffer_y(ctx.fb.drawing) as i16);
        // Own the background so the theme can switch at runtime.
        fill(0, 0, 320, 240, t.bg);
        // The menu and chart are full-screen views; everything else draws over
        // the grid.
        if self.mode == Mode::Menu {
            self.draw_menu(font);
            return;
        }
        if self.mode == Mode::Browse {
            self.draw_browse(font);
            return;
        }
        // A confirm is modal over the screen that raised it, not over the grid.
        if self.mode == Mode::Confirm {
            match self.ask {
                Ask::DeleteSave => self.draw_browse(font),
            }
            self.draw_confirm(font);
            return;
        }
        if self.mode == Mode::Chart {
            self.draw_chart(font);
            // "TRI:<next> O:Back" -- cycle Bar -> Line -> Area -> Pie.
            let mut bar = [0u8; 32];
            let hint = build_chart_hint(self.chart_kind.next().label(), &mut bar);
            draw_cmdbar(font, hint, "");
            return;
        }
        if self.mode == Mode::Help {
            self.draw_help(font);
            return;
        }
        self.draw_chrome(font);
        self.draw_grid(font);
        match self.mode {
            Mode::Edit => self.kbd.draw(font, &t.osk(), KBD_HINT),
            Mode::Cmd => self.draw_cmd_menu(font),
            Mode::Pick => draw_cmdbar(font, "D-Pad:Move  X:Insert Ref  SQ:Range  O:Cancel", ""),
            // Handled by early returns above.
            Mode::Chart | Mode::Menu | Mode::Browse | Mode::Help | Mode::Confirm => {}
            Mode::Nav => self.draw_nav_bar(font),
        }
        // Toasts float over every over-the-grid mode.
        self.draw_toast(font);
    }
}

impl Editor {
    /// The Nav bottom bar. With a selection marked it becomes a status bar in the
    /// Excel sense: the range plus live SUM / AVG / COUNT, with the hints below.
    fn draw_nav_bar(&self, font: &FontAtlas) {
        let Some((c0, r0, c1, r1)) = self.sel_anchor.map(|_| self.target_rect()) else {
            return draw_cmdbar(
                font,
                "X:Edit TRI:New SQ:Clr L1:Copy R1:Paste R2:Sel",
                "ST:Undo  SEL:Menu  L2+Dir:Page",
            );
        };
        let t = theme();
        let (sum, count) = sheet().stats(c0, r0, c1, r1);
        let mut line = [0u8; 64];
        let mut o = 0;
        let mut put = |s: &str| {
            for &b in s.as_bytes() {
                if o < line.len() {
                    line[o] = b;
                    o += 1;
                }
            }
        };
        let mut rb = [0u8; 12];
        let rn = range_ref(c0, r0, c1, r1, &mut rb);
        put(str_of(&rb[..rn]));
        let mut vb = [0u8; 24];
        put("  SUM ");
        put(fmt_value_fit(sum, 10, &mut vb));
        if count > 0 {
            put("  AVG ");
            put(fmt_value_fit(sum / count as i64, 8, &mut vb));
        }
        put("  N ");
        let mut cb = [0u8; 8];
        put(u32_dec(&mut cb, count));

        const BAR_Y: i16 = 214;
        fill(0, BAR_Y, 320, (240 - BAR_Y) as u16, t.panel);
        fill(0, BAR_Y, 320, 1, t.accent);
        text(font, 4, BAR_Y + 4, str_of(&line[..o]), t.good);
        text(
            font,
            4,
            BAR_Y + 15,
            "SQ:Clr L1:Copy R1:Paste SEL:Menu O:Drop",
            t.text,
        );
    }
}

/// Bottom command bar: a panel-backed legend of the current mode's controls,
/// so every command is visible on screen. `l2` may be empty for a one-line bar.
fn draw_cmdbar(font: &FontAtlas, l1: &str, l2: &str) {
    let t = theme();
    const BAR_Y: i16 = 214;
    fill(0, BAR_Y, 320, (240 - BAR_Y) as u16, t.panel);
    fill(0, BAR_Y, 320, 1, t.accent); // accent rule along the top edge
    text(font, 4, BAR_Y + 4, l1, t.text);
    if !l2.is_empty() {
        text(font, 4, BAR_Y + 15, l2, t.text);
    }
}

impl Editor {
    /// Should a D-pad direction fire this tick? First press, then delay-then-
    /// rate auto-repeat via the SDK tracker. Tuned at 60 Hz sim: ~0.23s to the
    /// first repeat, then ~20 steps/s.
    fn stepped(&self, btn: u16) -> bool {
        self.pad.repeats(btn, 14, 3)
    }

    fn update_nav(&mut self, ctx: &mut Ctx) {
        // Hold L2 to jump a whole screen at a time (page navigation).
        let l2 = ctx.is_held(button::L2);
        let (dc, dr) = if l2 { (VIS_COLS, VIS_ROWS) } else { (1, 1) };
        if self.stepped(button::LEFT) {
            self.cur_col = self.cur_col.saturating_sub(dc);
        }
        if self.stepped(button::RIGHT) {
            self.cur_col = (self.cur_col + dc).min(COLS - 1);
        }
        if self.stepped(button::UP) {
            self.cur_row = self.cur_row.saturating_sub(dr);
        }
        if self.stepped(button::DOWN) {
            self.cur_row = (self.cur_row + dr).min(ROWS - 1);
        }
        if ctx.just_pressed(button::CROSS) {
            self.begin_edit(false);
        }
        // TRIANGLE starts a fresh entry (overwrite), X amends the existing text.
        if ctx.just_pressed(button::TRIANGLE) {
            self.begin_edit(true);
        }
        // SQUARE clears the selection if there is one, else just this cell.
        if ctx.just_pressed(button::SQUARE) {
            self.push_undo();
            let (c0, r0, c1, r1) = self.target_rect();
            sheet().clear_rect(c0, r0, c1, r1);
            self.sel_anchor = None;
            self.toast("CLEARED", false);
        }
        if ctx.just_pressed(button::L1) {
            self.copy();
        }
        if ctx.just_pressed(button::R1) {
            self.paste();
        }
        // R2 marks a selection corner; pressing it again re-anchors here. No
        // toast: the status bar it opens already says what's selected, and the
        // hint line under it already says O drops it.
        if ctx.just_pressed(button::R2) {
            self.sel_anchor = Some((self.cur_col, self.cur_row));
        }
        if ctx.just_pressed(button::CIRCLE) && self.sel_anchor.is_some() {
            self.sel_anchor = None;
        }
        if ctx.just_pressed(button::START) {
            if l2 {
                // L2+START homes to A1; START alone is the undo everyone wants
                // on a face button.
                self.cur_col = 0;
                self.cur_row = 0;
            } else {
                self.undo();
            }
        }
        if ctx.just_pressed(button::SELECT) {
            self.set_sel = 0;
            self.menu_top = 0;
            self.mode = Mode::Cmd;
        }
    }

    /// Home the view for a freshly loaded sheet, and drop the undo history with
    /// it: those snapshots belong to the sheet that was just replaced, so
    /// keeping them would let one START press swap the whole document out.
    fn reset_view(&mut self) {
        self.cur_col = 0;
        self.cur_row = 0;
        self.top_col = 0;
        self.top_row = 0;
        self.scroll_px = 0;
        self.sel_anchor = None;
        self.undo_head = 0;
        self.undo_count = 0;
    }

    /// The rectangle an operation acts on: the selection if one is marked, else
    /// the single cell under the cursor.
    fn target_rect(&self) -> (usize, usize, usize, usize) {
        match self.sel_anchor {
            Some((ac, ar)) => (
                ac.min(self.cur_col),
                ar.min(self.cur_row),
                ac.max(self.cur_col),
                ar.max(self.cur_row),
            ),
            None => (self.cur_col, self.cur_row, self.cur_col, self.cur_row),
        }
    }

    /// Copy the target rectangle's raw text into the clipboard. Oversized
    /// rectangles are clipped to [`CLIP_MAX`] cells and say so, rather than
    /// pasting back a silently partial block.
    fn copy(&mut self) {
        let (c0, r0, c1, r1) = self.target_rect();
        let sh = sheet();
        let mut n = 0;
        let mut truncated = false;
        for row in r0..=r1 {
            for col in c0..=c1 {
                if n >= CLIP_MAX {
                    truncated = true;
                    break;
                }
                let cell = &sh.cells[idx(col, row)];
                let len = cell.len as usize;
                self.clip[n][..len].copy_from_slice(cell.raw());
                self.clip_lens[n] = cell.len;
                n += 1;
            }
        }
        self.clip_w = c1 - c0 + 1;
        self.clip_h = if self.clip_w > 0 { n / self.clip_w } else { 0 };
        self.clip_col = c0;
        self.clip_row = r0;
        if truncated {
            self.toast("COPIED (CLIPPED)", true);
        } else if n == 1 {
            self.toast("COPIED CELL", false);
        } else {
            self.toast("COPIED RANGE", false);
        }
    }

    /// Cut: copy, then clear the source.
    fn cut(&mut self) {
        self.copy();
        self.push_undo();
        let (c0, r0, c1, r1) = self.target_rect();
        sheet().clear_rect(c0, r0, c1, r1);
        self.sel_anchor = None;
        self.toast("CUT", false);
    }

    /// Paste the clipboard, references shifted by the move. A single copied cell
    /// pasted onto a selection fills the whole selection (Excel's fill-down); a
    /// copied rectangle lands with its top-left at the cursor.
    fn paste(&mut self) {
        if self.clip_w == 0 || self.clip_h == 0 {
            self.toast("CLIPBOARD EMPTY", true);
            return;
        }
        self.push_undo();
        let sh = sheet();
        if self.clip_w == 1 && self.clip_h == 1 {
            let (c0, r0, c1, r1) = self.target_rect();
            let len = self.clip_lens[0] as usize;
            let src = self.clip[0];
            sh.fill(self.clip_col, self.clip_row, &src[..len], c0, r0, c1, r1);
        } else {
            let dcol = self.cur_col as i32 - self.clip_col as i32;
            let drow = self.cur_row as i32 - self.clip_row as i32;
            for i in 0..self.clip_w * self.clip_h {
                let col = self.cur_col + i % self.clip_w;
                let row = self.cur_row + i / self.clip_w;
                let len = self.clip_lens[i] as usize;
                let src = self.clip[i];
                sh.set_shifted(col, row, &src[..len], dcol, drow);
            }
            sh.recalc();
        }
        self.sel_anchor = None;
        self.toast("PASTED", false);
    }

    // -- Undo -------------------------------------------------------------

    /// Snapshot the sheet before a mutation. A sheet too dense to fit a slot
    /// drops the whole history, so `undo` reports "nothing to undo" instead of
    /// restoring a state that skips an edit.
    fn push_undo(&mut self) {
        let ring = unsafe { &mut *core::ptr::addr_of_mut!(UNDO_RING) };
        let lens = unsafe { &mut *core::ptr::addr_of_mut!(UNDO_LEN) };
        match sheet().serialize(&mut ring[self.undo_head]) {
            Some(len) => {
                lens[self.undo_head] = len;
                self.undo_head = (self.undo_head + 1) % UNDO_SLOTS;
                self.undo_count = (self.undo_count + 1).min(UNDO_SLOTS);
            }
            None => self.undo_count = 0,
        }
    }

    fn undo(&mut self) {
        if self.undo_count == 0 {
            self.toast("NOTHING TO UNDO", true);
            return;
        }
        self.undo_head = (self.undo_head + UNDO_SLOTS - 1) % UNDO_SLOTS;
        self.undo_count -= 1;
        let ring = unsafe { &*core::ptr::addr_of!(UNDO_RING) };
        let lens = unsafe { &*core::ptr::addr_of!(UNDO_LEN) };
        let n = lens[self.undo_head];
        if sheet().deserialize(&ring[self.undo_head][..n]) {
            self.sel_anchor = None;
            self.toast("UNDONE", false);
        } else {
            self.toast("UNDO FAILED", true);
        }
    }

    /// Point-at-a-cell mode: roam the grid, X inserts the highlighted cell's
    /// reference into the formula being edited.
    fn update_pick(&mut self, ctx: &mut Ctx) {
        if self.stepped(button::LEFT) && self.pick_col > 0 {
            self.pick_col -= 1;
        }
        if self.stepped(button::RIGHT) && self.pick_col + 1 < COLS {
            self.pick_col += 1;
        }
        if self.stepped(button::UP) && self.pick_row > 0 {
            self.pick_row -= 1;
        }
        if self.stepped(button::DOWN) && self.pick_row + 1 < ROWS {
            self.pick_row += 1;
        }
        // SQUARE drops (or lifts) a range anchor to build "A1:B5".
        if ctx.just_pressed(button::SQUARE) {
            self.pick_anchor = match self.pick_anchor {
                None => Some((self.pick_col, self.pick_row)),
                Some(_) => None,
            };
        }
        if ctx.just_pressed(button::CROSS) {
            match self.pick_anchor {
                Some((a, r)) => self.push_range(a, r, self.pick_col, self.pick_row),
                None => self.push_ref(self.pick_col, self.pick_row),
            }
            self.pick_anchor = None;
            self.mode = Mode::Edit;
        }
        if ctx.just_pressed(button::CIRCLE) || ctx.just_pressed(button::R1) {
            self.pick_anchor = None;
            self.mode = Mode::Edit;
        }
    }

    fn update_chart(&mut self, ctx: &mut Ctx) {
        if ctx.just_pressed(button::TRIANGLE) {
            self.chart_kind = self.chart_kind.next();
        }
        if ctx.just_pressed(button::CIRCLE) || ctx.just_pressed(button::CROSS) {
            self.mode = Mode::Nav;
        }
    }

    /// Append a cell reference ("B12") to the edit buffer.
    fn push_ref(&mut self, col: usize, row: usize) {
        self.push(b'A' + col as u8);
        let mut rb = [0u8; 4];
        for &d in u32_dec(&mut rb, (row + 1) as u32).as_bytes() {
            self.push(d);
        }
    }

    /// Append a range "A1:B5" (normalised to min..max corners) to the buffer.
    fn push_range(&mut self, c0: usize, r0: usize, c1: usize, r1: usize) {
        self.push_ref(c0.min(c1), r0.min(r1));
        self.push(b':');
        self.push_ref(c0.max(c1), r0.max(r1));
    }

    /// The cell the grid highlights: the roaming cursor while picking, else the
    /// selected/edited cell.
    fn active_cell(&self) -> (usize, usize) {
        if self.mode == Mode::Pick {
            (self.pick_col, self.pick_row)
        } else {
            (self.cur_col, self.cur_row)
        }
    }

    // -- Start menu -------------------------------------------------------

    fn menu_len(&self) -> usize {
        SAMPLES.len() + self.has_save as usize
    }

    fn update_menu(&mut self, ctx: &mut Ctx) {
        if ctx.just_pressed(button::UP) {
            self.menu_sel = self.menu_sel.saturating_sub(1);
        }
        if ctx.just_pressed(button::DOWN) && self.menu_sel + 1 < self.menu_len() {
            self.menu_sel += 1;
        }
        if ctx.just_pressed(button::CROSS) || ctx.just_pressed(button::START) {
            self.open_selection();
        }
    }

    fn open_selection(&mut self) {
        self.reset_view();
        let sel = self.menu_sel;
        if sel < SAMPLES.len() {
            (SAMPLES[sel].1)(sheet());
            // Some samples showcase a chart by opening straight into it.
            if let Some((kind, range)) = SAMPLES[sel].2 {
                self.chart_kind = kind;
                self.chart_range = range;
                self.mode = Mode::Chart;
            } else {
                self.mode = Mode::Nav;
            }
        } else {
            // The last item is "Browse saves".
            self.enter_browse(true);
        }
    }

    fn draw_menu(&self, font: &FontAtlas) {
        let t = theme();
        // Title (3x scale) + subtitle + a rule.
        text_scaled(font, 112, 28, "PSXcel", 3, 3, t.good);
        fill(40, 58, 240, 1, t.accent);
        text(font, 60, 64, "a spreadsheet for the PS1", t.dim);

        let x: i16 = 104;
        let mut y: i16 = 86;
        for i in 0..self.menu_len() {
            let sel = i == self.menu_sel;
            if sel {
                fill(x - 10, y - 1, 132, 11, t.accent);
            }
            let label = if i < SAMPLES.len() {
                SAMPLES[i].0
            } else {
                "Browse saves"
            };
            text(font, x, y, label, if sel { t.accent_text } else { t.text });
            y += 15;
        }
        draw_cmdbar(font, "D-Pad:Move   X:Open", "");
    }

    fn draw_browse(&self, font: &FontAtlas) {
        let t = theme();
        fill(0, 0, 320, 12, t.panel);
        text(font, 4, 2, "LOAD FROM CARD", t.good);

        if self.file_count == 0 {
            text(font, 76, 104, "NO SAVES ON CARD", t.dim);
            draw_cmdbar(font, "O:Back", "");
            self.draw_toast(font);
            return;
        }

        let x: i16 = 40;
        let mut y: i16 = 30;
        for i in 0..self.file_count {
            let sel = i == self.browse_sel;
            if sel {
                fill(x - 8, y - 1, 244, 11, t.accent);
            }
            let tint = if sel { t.accent_text } else { t.text };
            text(font, x, y, clip(display_name(&self.files[i]), 16), tint);
            // Block usage on the right.
            let mut bb = [0u8; 8];
            let bs = u32_dec(&mut bb, self.files[i].blocks as u32);
            text(
                font,
                x + 196,
                y,
                bs,
                if sel { t.accent_text } else { t.dim },
            );
            text(
                font,
                x + 204,
                y,
                "blk",
                if sel { t.accent_text } else { t.dim },
            );
            y += 14;
        }
        draw_cmdbar(font, "D-Pad:Move  X:Load  SQ:Delete  O:Back", "");
        self.draw_toast(font);
    }

    // -- Command menu -----------------------------------------------------

    /// The command menu: everything the pad has run out of buttons for. A
    /// separator row (`None`) groups it without needing a nested menu.
    const CMD_ROWS: &'static [Option<(&'static str, Cmd)>] = &[
        Some(("Undo", Cmd::Undo)),
        Some(("Cut", Cmd::Cut)),
        Some(("Copy", Cmd::Copy)),
        Some(("Paste", Cmd::Paste)),
        None,
        Some(("Insert row", Cmd::InsRow)),
        Some(("Delete row", Cmd::DelRow)),
        Some(("Insert column", Cmd::InsCol)),
        Some(("Delete column", Cmd::DelCol)),
        None,
        Some(("Chart selection", Cmd::Chart)),
        Some(("Theme", Cmd::Theme)),
        None,
        Some(("Save to card", Cmd::Save)),
        Some(("Load from card", Cmd::Load)),
        Some(("Help", Cmd::Help)),
        Some(("Main menu", Cmd::MainMenu)),
    ];
    /// Rows the menu panel shows at once.
    const CMD_VIS: usize = 12;

    /// Step `set_sel` by `d`, skipping separators and stopping at the ends.
    fn cmd_step(&mut self, d: isize) {
        let n = Self::CMD_ROWS.len() as isize;
        let mut i = self.set_sel as isize + d;
        while i >= 0 && i < n && Self::CMD_ROWS[i as usize].is_none() {
            i += d;
        }
        if i >= 0 && i < n {
            self.set_sel = i as usize;
        }
        // Keep the cursor inside the scrolling window.
        self.menu_top = self
            .menu_top
            .min(self.set_sel)
            .max(self.set_sel.saturating_sub(Self::CMD_VIS - 1));
    }

    fn update_cmd_menu(&mut self, ctx: &mut Ctx) {
        if self.stepped(button::UP) {
            self.cmd_step(-1);
        }
        if self.stepped(button::DOWN) {
            self.cmd_step(1);
        }
        let cmd = Self::CMD_ROWS[self.set_sel].map(|(_, c)| c);
        // LEFT/RIGHT also toggle the theme, so it reads as a setting not an action.
        let toggled = cmd == Some(Cmd::Theme)
            && (ctx.just_pressed(button::LEFT) || ctx.just_pressed(button::RIGHT));
        if ctx.just_pressed(button::CROSS) || toggled {
            if let Some(cmd) = cmd {
                self.run_cmd(cmd);
            }
        }
        if ctx.just_pressed(button::CIRCLE) || ctx.just_pressed(button::SELECT) {
            self.mode = Mode::Nav;
        }
    }

    /// Run a menu command. Anything that mutates the sheet snapshots for undo
    /// first and closes the menu, so the result is visible immediately.
    fn run_cmd(&mut self, cmd: Cmd) {
        let (c0, r0, c1, r1) = self.target_rect();
        match cmd {
            Cmd::Undo => {
                self.undo();
                self.mode = Mode::Nav;
            }
            Cmd::Cut => {
                self.cut();
                self.mode = Mode::Nav;
            }
            Cmd::Copy => {
                self.copy();
                self.mode = Mode::Nav;
            }
            Cmd::Paste => {
                self.paste();
                self.mode = Mode::Nav;
            }
            // Row / column ops act on the selection's span, so a 3-row selection
            // inserts 3 rows.
            Cmd::InsRow => {
                self.push_undo();
                for _ in r0..=r1 {
                    sheet().insert_row(r0);
                }
                self.after_structural("ROW INSERTED");
            }
            Cmd::DelRow => {
                self.push_undo();
                for _ in r0..=r1 {
                    sheet().delete_row(r0);
                }
                self.after_structural("ROW DELETED");
            }
            Cmd::InsCol => {
                self.push_undo();
                for _ in c0..=c1 {
                    sheet().insert_col(c0);
                }
                self.after_structural("COLUMN INSERTED");
            }
            Cmd::DelCol => {
                self.push_undo();
                for _ in c0..=c1 {
                    sheet().delete_col(c0);
                }
                self.after_structural("COLUMN DELETED");
            }
            Cmd::Chart => {
                // A one-cell chart is a dead end, so say what's missing rather
                // than drawing a single bar.
                if c0 == c1 && r0 == r1 {
                    self.toast("MARK A RANGE FIRST (R2)", true);
                    self.mode = Mode::Nav;
                } else {
                    self.chart_range = (c0, r0, c1, r1);
                    self.mode = Mode::Chart;
                }
            }
            Cmd::Theme => theme::set_dark(!theme::is_dark()),
            Cmd::Save => self.begin_saveas(),
            Cmd::Load => self.enter_browse(false),
            Cmd::Help => self.mode = Mode::Help,
            Cmd::MainMenu => {
                self.menu_sel = 0;
                self.mode = Mode::Menu;
            }
        }
    }

    /// Shared tail for insert/delete row+column: the selection's coordinates no
    /// longer mean anything once the grid has shifted under them.
    fn after_structural(&mut self, msg: &str) {
        self.sel_anchor = None;
        self.toast(msg, false);
        self.mode = Mode::Nav;
    }

    fn draw_cmd_menu(&self, font: &FontAtlas) {
        let t = theme();
        let (x, y, w) = (86, 26, 176);
        let h = 22 + Self::CMD_VIS as i16 * 13 + 14;
        fill(x, y, w as u16, h as u16, t.panel);
        fill(x, y, w as u16, 2, t.accent);
        text(font, x + 10, y + 8, "MENU", t.good);
        // Scroll indicator, counting commands rather than rows: separators are
        // not places the cursor can be, so including them would overstate the list.
        let commands = Self::CMD_ROWS.iter().filter(|r| r.is_some()).count();
        let nth = Self::CMD_ROWS[..=self.set_sel]
            .iter()
            .filter(|r| r.is_some())
            .count();
        let mut cnt = [0u8; 12];
        let cn = build_frac(nth, commands, &mut cnt);
        text(
            font,
            x + w - 4 - (cn as i16) * GW,
            y + 8,
            str_of(&cnt[..cn]),
            t.dim,
        );

        for slot in 0..Self::CMD_VIS {
            let i = self.menu_top + slot;
            let Some(row) = Self::CMD_ROWS.get(i) else {
                break;
            };
            let ry = y + 22 + slot as i16 * 13;
            let Some((label, cmd)) = row else {
                // Separator: a hairline instead of a blank gap.
                fill(x + 10, ry + 4, w as u16 - 20, 1, t.sep);
                continue;
            };
            let sel = i == self.set_sel;
            if sel {
                fill(x + 5, ry - 1, w as u16 - 10, 11, t.accent);
            }
            let tint = if sel { t.accent_text } else { t.text };
            text(font, x + 10, ry, label, tint);
            // Right-hand column: the current value or the shortcut, so the menu
            // teaches the button map instead of hiding it.
            let hint = cmd.hint();
            if !hint.is_empty() {
                let hx = x + w - 6 - (hint.len() as i16) * GW;
                text(font, hx, ry, hint, if sel { t.accent_text } else { t.dim });
            }
        }
        text(font, x + 10, y + h - 11, "X:Select  O:Back", t.dim);
    }

    // -- Help --------------------------------------------------------------

    fn update_help(&mut self, ctx: &mut Ctx) {
        if ctx.just_pressed(button::CIRCLE) || ctx.just_pressed(button::CROSS) {
            self.mode = Mode::Nav;
        }
    }

    fn draw_help(&self, font: &FontAtlas) {
        let t = theme();
        fill(0, 0, 320, 12, t.panel);
        text(font, 4, 2, "CONTROLS", t.good);

        // (section, button, meaning) -- an empty button starts a section.
        const ROWS: &[(&str, &str)] = &[
            ("GRID", ""),
            ("D-Pad", "Move   (L2+D-Pad pages)"),
            ("X / TRI", "Edit cell / fresh entry"),
            ("R2", "Mark a selection, D-Pad extends"),
            ("O", "Drop the selection"),
            ("SQ", "Clear cell or selection"),
            ("L1 / R1", "Copy / Paste"),
            ("START", "Undo   (L2+START homes to A1)"),
            ("SELECT", "Menu: rows, columns, chart, save"),
            ("TYPING", ""),
            ("D-Pad, X", "Pick a key, type it"),
            ("L2+<  >", "Move the caret (L2+UP/DN ends)"),
            ("SQ / L1", "Backspace at the caret"),
            ("R2", "Jump to OK, then X commits"),
            ("R1", "Point at a cell to insert its ref"),
        ];
        let mut y = 18i16;
        for (btn, meaning) in ROWS {
            if meaning.is_empty() {
                y += 4;
                text(font, 6, y, btn, t.hot);
                fill(6, y + 9, 308, 1, t.sep);
                y += 13;
                continue;
            }
            text(font, 10, y, btn, t.good);
            text(font, 68, y, meaning, t.text);
            y += 11;
        }
        draw_cmdbar(font, "O:Back", "");
    }

    // -- Confirm ----------------------------------------------------------

    fn ask(&mut self, ask: Ask) {
        self.ask = ask;
        self.mode = Mode::Confirm;
    }

    fn update_confirm(&mut self, ctx: &mut Ctx) {
        if ctx.just_pressed(button::CROSS) {
            match self.ask {
                Ask::DeleteSave => self.delete_selected(),
            }
        } else if ctx.just_pressed(button::CIRCLE) {
            // Cancelling a card delete goes back to the browser it came from.
            self.mode = match self.ask {
                Ask::DeleteSave => Mode::Browse,
            };
        }
    }

    fn draw_confirm(&self, font: &FontAtlas) {
        let t = theme();
        let (x, y, w, h) = (52, 92, 216, 56);
        fill(x - 2, y - 2, w as u16 + 4, h as u16 + 4, t.bad);
        fill(x, y, w as u16, h as u16, t.panel);
        let (q, subject) = match self.ask {
            Ask::DeleteSave => ("DELETE THIS SAVE?", self.selected_save_name()),
        };
        text(font, x + 10, y + 10, q, t.bad);
        text(font, x + 10, y + 24, clip(subject, 38), t.text);
        text(
            font,
            x + 10,
            y + 40,
            "X:Yes, delete    O:No, keep it",
            t.dim,
        );
    }

    /// Display name of the browser's highlighted save, for the confirm prompt.
    fn selected_save_name(&self) -> &str {
        if self.browse_sel < self.file_count {
            display_name(&self.files[self.browse_sel])
        } else {
            ""
        }
    }

    // -- Toasts -----------------------------------------------------------

    /// Post a transient message. Replaces the old sticky status line: a result
    /// you have to dismiss reads as an error state long after it stopped being
    /// one.
    fn toast(&mut self, s: &str, bad: bool) {
        let b = s.as_bytes();
        let n = b.len().min(self.status.len());
        self.status[..n].copy_from_slice(&b[..n]);
        self.status_len = n;
        self.status_ttl = TOAST_TTL;
        self.status_bad = bad;
    }

    fn draw_toast(&self, font: &FontAtlas) {
        if self.status_ttl == 0 || self.status_len == 0 {
            return;
        }
        let t = theme();
        let msg = str_of(&self.status[..self.status_len]);
        let w = (msg.len() as i16) * GW + 16;
        let x = (320 - w) / 2;
        let y = 190i16;
        // No alpha blending here: lerp the colours toward the background over
        // the last TOAST_FADE frames, which looks the same over opaque chrome.
        let (num, den) = if self.status_ttl < TOAST_FADE {
            (self.status_ttl as u16, TOAST_FADE)
        } else {
            (1, 1)
        };
        let edge = if self.status_bad { t.bad } else { t.accent };
        fill(x, y, w as u16, 14, lerp(t.bg, t.panel, num, den));
        fill(x, y, w as u16, 1, lerp(t.bg, edge, num, den));
        text(font, x + 8, y + 4, msg, lerp(t.bg, t.text, num, den));
    }

    // -- Save (name entry reuses the Edit keyboard) -----------------------

    /// Begin entering a save name on the on-screen keyboard.
    fn begin_saveas(&mut self) {
        self.saving = true;
        self.edit_len = 0;
        // Must move with edit_len: a caret left past the end of an empty buffer
        // makes the next insert slice backwards, which aborts on target.
        self.caret = 0;
        self.kbd = Keyboard::new();
        self.mode = Mode::Edit;
    }

    /// Serialize the sheet and write it to `PSXCEL-<typed name>` on the card.
    fn commit_saveas(&mut self) {
        self.saving = false;
        self.mode = Mode::Nav;
        let name_buf = self.edit_buf;
        let mut fname = [0u8; 20];
        let flen = make_filename(&name_buf[..self.edit_len], &mut fname);
        if flen == 0 {
            self.toast("NAME NEEDS A-Z 0-9", true);
            return;
        }
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(SAVE_BUF) };
        let scratch = unsafe { &mut *core::ptr::addr_of_mut!(COMP_SCRATCH) };
        let Some(len) = sheet().serialize(buf) else {
            self.toast("SHEET TOO BIG", true);
            return;
        };
        let name = str_of(&fname[..flen]);
        let title = str_of(&name_buf[..self.edit_len]);
        let mut card = Card::new(HardwareCard::new(Slot::One));
        let res = (|| -> core::result::Result<(), McError> {
            if !card.is_formatted()? {
                card.format()?;
            }
            card.write_compressed(name, title, &buf[..len], scratch)
        })();
        match res {
            Ok(()) => {
                // The title screen only offers "Browse saves" when a save
                // exists; it checked at boot, so tell it about this one.
                self.has_save = true;
                self.toast("SAVED OK", false);
            }
            Err(e) => self.toast(mc_err(e), true),
        }
    }

    // -- Load / browse ----------------------------------------------------

    /// Read the card's PSXcel files into `self.files` and open the browser.
    fn enter_browse(&mut self, from_menu: bool) {
        self.browse_from_menu = from_menu;
        self.browse_sel = 0;
        self.file_count = 0;
        let mut all = [EMPTY_ENTRY; MAX_FILES];
        if let Ok(k) = Card::new(HardwareCard::new(Slot::One)).list(&mut all) {
            for e in all[..k].iter() {
                if e.name().starts_with(SAVE_PREFIX) && self.file_count < MAX_FILES {
                    self.files[self.file_count] = *e;
                    self.file_count += 1;
                }
            }
        }
        self.mode = Mode::Browse;
    }

    fn update_browse(&mut self, ctx: &mut Ctx) {
        if ctx.just_pressed(button::UP) {
            self.browse_sel = self.browse_sel.saturating_sub(1);
        }
        if ctx.just_pressed(button::DOWN) && self.browse_sel + 1 < self.file_count {
            self.browse_sel += 1;
        }
        if ctx.just_pressed(button::CROSS) && self.file_count > 0 {
            self.load_selected();
        }
        // A card delete is unrecoverable, so it asks first.
        if ctx.just_pressed(button::SQUARE) && self.file_count > 0 {
            self.ask(Ask::DeleteSave);
        }
        if ctx.just_pressed(button::CIRCLE) {
            self.cur_col = 0;
            self.cur_row = 0;
            self.mode = if self.browse_from_menu {
                Mode::Menu
            } else {
                Mode::Nav
            };
        }
    }

    fn load_selected(&mut self) {
        let name = self.files[self.browse_sel].name();
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(SAVE_BUF) };
        let mut card = Card::new(HardwareCard::new(Slot::One));
        match card.read(name, buf) {
            Ok(n) if sheet().deserialize(&buf[..n]) => {
                self.reset_view();
                self.toast("LOADED", false);
                self.mode = Mode::Nav;
            }
            Ok(_) => self.toast("BAD SAVE DATA", true),
            Err(e) => self.toast(mc_err(e), true),
        }
    }

    fn delete_selected(&mut self) {
        let name = self.files[self.browse_sel].name();
        let res = Card::new(HardwareCard::new(Slot::One)).delete(name);
        // Refresh the list (stay in the browser).
        let from_menu = self.browse_from_menu;
        self.enter_browse(from_menu);
        match res {
            Ok(()) => self.toast("SAVE DELETED", false),
            Err(e) => self.toast(mc_err(e), true),
        }
    }

    fn begin_edit(&mut self, blank: bool) {
        let cell = &sheet().cells[idx(self.cur_col, self.cur_row)];
        self.edit_len = if blank { 0 } else { cell.len as usize };
        self.edit_buf[..self.edit_len].copy_from_slice(&cell.raw()[..self.edit_len]);
        // Open with the caret at the end: amending is the common case.
        self.caret = self.edit_len;
        self.kbd = Keyboard::new();
        self.pick_col = self.cur_col;
        self.pick_row = self.cur_row;
        self.pick_anchor = None;
        self.sel_anchor = None;
        self.mode = Mode::Edit;
    }

    fn update_edit(&mut self, ctx: &mut Ctx) {
        // L2 turns the D-pad into caret control, mirroring Nav's L2+D-pad page
        // jump: one modifier, no new buttons, and the keyboard keeps all four
        // directions when L2 is up.
        if ctx.is_held(button::L2) {
            if self.stepped(button::LEFT) {
                self.caret = self.caret.saturating_sub(1);
            }
            if self.stepped(button::RIGHT) {
                self.caret = (self.caret + 1).min(self.edit_len);
            }
            if ctx.just_pressed(button::UP) {
                self.caret = 0;
            }
            if ctx.just_pressed(button::DOWN) {
                self.caret = self.edit_len;
            }
        } else {
            let dirs = [
                (button::LEFT, Dir::Left),
                (button::RIGHT, Dir::Right),
                (button::UP, Dir::Up),
                (button::DOWN, Dir::Down),
            ];
            for &(btn, dir) in dirs.iter() {
                if self.stepped(btn) {
                    self.kbd.step(dir);
                }
            }
        }
        // X activates the highlighted key (type / shift / symbols / space / del / ok).
        if ctx.just_pressed(button::CROSS) {
            match self.kbd.activate() {
                Action::Insert(c) => self.push(c),
                Action::Backspace => self.backspace(),
                Action::Commit => self.finish_edit(),
                Action::None => {}
            }
        }
        // Hardware shortcuts alongside the on-screen keys.
        if ctx.just_pressed(button::L1) || ctx.just_pressed(button::SQUARE) {
            self.backspace();
        }
        if ctx.just_pressed(button::R2) {
            self.kbd = focus_ok();
        }
        if ctx.just_pressed(button::START) {
            self.finish_edit();
        }
        // R1 jumps to "point at a cell" mode to insert a reference / range (not
        // while typing a save name).
        if ctx.just_pressed(button::R1) && !self.saving {
            self.pick_col = self.cur_col;
            self.pick_row = self.cur_row;
            self.pick_anchor = None;
            self.mode = Mode::Pick;
        }
        if ctx.just_pressed(button::CIRCLE) {
            self.saving = false;
            self.mode = Mode::Nav;
        }
    }

    /// Commit the Edit keyboard: save the sheet if naming, else set the cell.
    fn finish_edit(&mut self) {
        if self.saving {
            self.commit_saveas();
        } else {
            self.commit();
        }
    }

    /// Insert one character at the caret, shifting the tail right.
    fn push(&mut self, c: u8) {
        if self.edit_len >= sheet::INPUT_CAP {
            self.toast("LINE FULL", true);
            return;
        }
        self.edit_buf
            .copy_within(self.caret..self.edit_len, self.caret + 1);
        self.edit_buf[self.caret] = c;
        self.edit_len += 1;
        self.caret += 1;
    }

    /// Delete the character before the caret, closing the gap.
    fn backspace(&mut self) {
        if self.caret == 0 {
            return;
        }
        self.edit_buf
            .copy_within(self.caret..self.edit_len, self.caret - 1);
        self.edit_len -= 1;
        self.caret -= 1;
    }

    fn commit(&mut self) {
        let buf = self.edit_buf;
        self.push_undo();
        sheet().set(self.cur_col, self.cur_row, &buf[..self.edit_len]);
        self.mode = Mode::Nav;
    }

    fn keep_cursor_visible(&mut self) {
        let (cc, cr) = self.active_cell();
        if cc < self.top_col {
            self.top_col = cc;
        } else if cc >= self.top_col + VIS_COLS {
            self.top_col = cc - VIS_COLS + 1;
        }
        if cr < self.top_row {
            self.top_row = cr;
        } else if cr >= self.top_row + VIS_ROWS {
            self.top_row = cr - VIS_ROWS + 1;
        }
    }

    /// Formula bar + column-letter header.
    fn draw_chrome(&self, font: &FontAtlas) {
        let t = theme();
        let (acol, _) = self.active_cell();
        // Formula bar background.
        fill(0, 0, 320, 10, t.panel);
        // While typing a save name, the bar shows "SAVE AS: <name>|".
        if self.saving {
            text(font, 4, FBAR_Y, "SAVE AS:", t.good);
            let cx = 4 + 9 * GW;
            text(
                font,
                cx,
                FBAR_Y,
                str_of(&self.edit_buf[..self.edit_len]),
                t.text,
            );
            self.draw_caret(cx);
            self.draw_col_headers(font, acol);
            return;
        }
        // "B3:" reference tag -- always the edited/selected cell (not the pick
        // cursor), so you can see where the formula lands while pointing.
        let mut tag = [0u8; 6];
        let n = cell_ref(self.cur_col, self.cur_row, &mut tag);
        text(font, 4, FBAR_Y, str_of(&tag[..n]), t.good);
        // Contents: the edit buffer while editing/picking, else the raw text.
        let cx = 4 + (n as i16) * GW + 4;
        if matches!(self.mode, Mode::Edit | Mode::Pick) {
            let m = self.edit_len;
            text(font, cx, FBAR_Y, str_of(&self.edit_buf[..m]), t.text);
            self.draw_caret(cx);
            self.draw_preview(font, cx + (m as i16) * GW + 10);
        } else {
            let c = &sheet().cells[idx(self.cur_col, self.cur_row)];
            text(font, cx, FBAR_Y, str_of(c.raw()), t.text);
            // For a formula cell, echo the evaluated result: "=A1+B2  = 4.5".
            if matches!(c.kind, Kind::Num) && c.raw().first() == Some(&b'=') {
                let mut vb = [0u8; 16];
                let vs = fmt_value(c.value, &mut vb);
                let rx = cx + (c.len as i16) * GW + 8;
                text(font, rx, FBAR_Y, "=", t.dim);
                text(font, rx + 12, FBAR_Y, vs, t.good);
            }
        }

        self.draw_col_headers(font, acol);
    }

    /// The text caret: a blinking 1px bar between glyphs, at the insertion
    /// point. `x0` is the left edge of the text it sits in.
    fn draw_caret(&self, x0: i16) {
        // ~1.9 Hz: fast enough to read as a caret, slow enough not to strobe.
        if (self.frame / 16) % 2 == 0 {
            return;
        }
        let t = theme();
        fill(x0 + (self.caret as i16) * GW - 1, FBAR_Y - 1, 1, 9, t.hot);
    }

    /// Live result of the half-typed formula, before it is committed. A formula
    /// that doesn't parse yet shows a dim "?" rather than a red error, since
    /// every formula is briefly invalid while you type it.
    fn draw_preview(&self, font: &FontAtlas, x: i16) {
        let t = theme();
        if self.edit_len == 0 || self.edit_buf[0] != b'=' {
            return;
        }
        text(font, x, FBAR_Y, "=", t.dim);
        let buf = self.edit_buf;
        let (v, kind) = sheet().preview(&buf[..self.edit_len]);
        match kind {
            Kind::Num => {
                let mut vb = [0u8; 24];
                text(font, x + 10, FBAR_Y, fmt_value(v, &mut vb), t.good);
            }
            _ => text(font, x + 10, FBAR_Y, "?", t.dim),
        }
    }

    /// The A..F column-letter header row (`acol` is highlighted).
    fn draw_col_headers(&self, font: &FontAtlas, acol: usize) {
        let t = theme();
        fill(0, COLHDR_Y - 1, 320, 9, t.panel);
        for vc in 0..VIS_COLS {
            let col = self.top_col + vc;
            if col >= COLS {
                break;
            }
            let x = ROWHDR_W + (vc as i16) * COL_W;
            let letter = [b'A' + col as u8];
            let tint = if col == acol { t.hot } else { t.dim };
            text(
                font,
                x + COL_W / 2 - GW / 2,
                COLHDR_Y,
                str_of(&letter),
                tint,
            );
        }
    }

    /// Gather the numeric values in the chart range (row-major), capped at
    /// [`MAX_POINTS`]. Non-numeric / empty cells are skipped.
    fn collect_values(&self, out: &mut [i64; MAX_POINTS]) -> usize {
        let sh = sheet();
        let (c0, r0, c1, r1) = self.chart_range;
        let mut n = 0;
        for row in r0..=r1 {
            for col in c0..=c1 {
                if n >= MAX_POINTS {
                    return n;
                }
                let cell = &sh.cells[idx(col, row)];
                if matches!(cell.kind, Kind::Num) {
                    out[n] = cell.value;
                    n += 1;
                }
            }
        }
        n
    }

    /// Full-screen bar / line chart of the selected range.
    fn draw_chart(&self, font: &FontAtlas) {
        let t = theme();
        // Title bar.
        fill(0, 0, 320, 12, t.panel);
        let (c0, r0, c1, r1) = self.chart_range;
        let mut rb = [0u8; 12];
        let rn = range_ref(c0, r0, c1, r1, &mut rb);
        text(font, 4, 2, "CHART", t.good);
        text(font, 52, 2, str_of(&rb[..rn]), t.text);
        let lbl = self.chart_kind.label();
        text(font, 320 - font.text_width(lbl) as i16 - 6, 2, lbl, t.hot);

        let mut vals = [0i64; MAX_POINTS];
        let n = self.collect_values(&mut vals);
        // Point count in the title bar, clear of the category labels.
        let mut cbuf = [0u8; 12];
        cbuf[..2].copy_from_slice(b"n=");
        let cn = 2 + u32_dec(&mut cbuf[2..], n as u32).len();
        text(font, 120, 2, str_of(&cbuf[..cn]), t.dim);
        if n == 0 {
            text(font, 80, 110, "NO NUMERIC DATA", t.dim);
            return;
        }

        // Pie is a circular view; the others share an axis-based plot.
        if self.chart_kind == ChartKind::Pie {
            self.draw_pie(font);
            return;
        }

        // Plot rectangle.
        const PX0: i16 = 44;
        const PY0: i16 = 28;
        const PX1: i16 = 306;
        const PY1: i16 = 188;
        let plot_h = PY1 - PY0;
        let plot_w = PX1 - PX0;

        // Value range, always including the zero baseline.
        let mut lo = 0i64;
        let mut hi = 0i64;
        for &v in vals[..n].iter() {
            lo = lo.min(v);
            hi = hi.max(v);
        }
        if hi == lo {
            hi = lo + 1; // avoid a zero span for an all-equal series
        }
        let span = hi.saturating_sub(lo);
        let y_of = |v: i64| -> i16 { PY1 - scale_px(v - lo, span, plot_h) };

        // Gridlines with tick labels at quarters of the range, so a bar can be
        // read off the chart instead of only compared to its neighbours.
        const TICKS: i16 = 4;
        for i in 0..=TICKS {
            let v = lo + (hi - lo) * i as i64 / TICKS as i64;
            let y = y_of(v);
            // The axis itself is drawn below; gridlines stay faint.
            if i > 0 {
                fill(PX0 + 1, y, plot_w as u16 - 1, 1, t.sep);
            }
            let mut lbuf = [0u8; 16];
            let s = fmt_value_fit(v, 7, &mut lbuf);
            text(font, PX0 - 4 - (s.len() as i16) * GW, y - 4, s, t.dim);
        }

        // Axes: axis-aligned 1px filled rects.
        fill(PX0, PY0, 1, plot_h as u16, t.dim);
        fill(PX0, PY1, plot_w as u16, 1, t.dim);
        // Zero baseline (only distinct from the x-axis when there are negatives).
        if lo < 0 {
            let yz = y_of(0);
            fill(PX0, yz, plot_w as u16, 1, t.hot);
        }

        let step = (plot_w / n as i16).max(1);
        // Category labels under the axis, from the text column beside the data
        // (the same convention the pie legend uses). Only when they fit.
        if step >= 4 * GW {
            let sh = sheet();
            let (dc0, dr0, _, _) = self.chart_range;
            for i in 0..n {
                if dc0 == 0 {
                    break;
                }
                let lc = &sh.cells[idx(dc0 - 1, dr0 + i)];
                if !matches!(lc.kind, Kind::Text) {
                    continue;
                }
                let s = clip(str_of(lc.raw()), (step / GW) as usize - 1);
                let cx = PX0 + i as i16 * step + step / 2;
                text(font, cx - (s.len() as i16) * GW / 2, PY1 + 4, s, t.dim);
            }
        }
        match self.chart_kind {
            ChartKind::Bar => {
                let y0 = y_of(0);
                let bw = (step - 2).max(1) as u16;
                for (i, &v) in vals[..n].iter().enumerate() {
                    let x = PX0 + 1 + i as i16 * step;
                    let yv = y_of(v);
                    let (top, h) = if yv <= y0 {
                        (yv, (y0 - yv).max(1) as u16)
                    } else {
                        (y0, (yv - y0).max(1) as u16)
                    };
                    fill(x, top, bw, h, t.accent);
                    // Value label on the bar, when the column is wide enough.
                    // Above the bar normally; inside the top when a full-height
                    // bar leaves no headroom, so the tallest bar keeps its label.
                    let mut vb = [0u8; 16];
                    let s = fmt_value_fit(v, (step / GW).max(1) as usize, &mut vb);
                    let sw = (s.len() as i16) * GW;
                    if step >= sw + 2 {
                        let lx = x + (step - 1 - sw) / 2;
                        if top - PY0 >= 9 {
                            text(font, lx, top - 9, s, t.hot);
                        } else if h >= 10 {
                            text(font, lx, top + 1, s, t.accent_text);
                        }
                    }
                }
            }
            ChartKind::Line | ChartKind::Area => {
                let cx = |i: usize| PX0 + i as i16 * step + step / 2;
                // Area: fill the region under the line down to the baseline.
                if self.chart_kind == ChartKind::Area {
                    let y0 = y_of(0);
                    let fillc = mix(t.accent, t.bg);
                    for i in 0..n.saturating_sub(1) {
                        let (xa, ya) = (cx(i), y_of(vals[i]));
                        let (xb, yb) = (cx(i + 1), y_of(vals[i + 1]));
                        // Trapezoid as a flat quad (fan order: TL, TR, BL, BR).
                        draw_quad_flat(
                            [(xa, ya), (xb, yb), (xa, y0), (xb, y0)],
                            fillc.0,
                            fillc.1,
                            fillc.2,
                        );
                    }
                }
                // Line + markers on top.
                for i in 0..n.saturating_sub(1) {
                    draw_seg(cx(i), y_of(vals[i]), cx(i + 1), y_of(vals[i + 1]), t.accent);
                }
                for i in 0..n {
                    fill(cx(i) - 1, y_of(vals[i]) - 1, 3, 3, t.hot);
                }
            }
            ChartKind::Pie => {} // handled above
        }
    }

    /// Pie chart of the selected range: slice angles proportional to each value's
    /// magnitude, with a colour legend (label from the adjacent text column, else
    /// the value).
    fn draw_pie(&self, font: &FontAtlas) {
        let t = theme();
        let sh = sheet();
        let (c0, r0, c1, r1) = self.chart_range;

        let mut vals = [0i64; MAX_POINTS];
        let mut src = [(0usize, 0usize); MAX_POINTS];
        let mut n = 0;
        'outer: for row in r0..=r1 {
            for col in c0..=c1 {
                if n >= MAX_POINTS {
                    break 'outer;
                }
                let cell = &sh.cells[idx(col, row)];
                if matches!(cell.kind, Kind::Num) && cell.value != 0 {
                    vals[n] = cell.value;
                    src[n] = (col, row);
                    n += 1;
                }
            }
        }
        let mut total: u64 = 0;
        for &v in vals[..n].iter() {
            total += v.unsigned_abs();
        }
        if total == 0 {
            text(font, 80, 110, "NO DATA", t.dim);
            return;
        }

        // Slices, from 12 o'clock clockwise.
        let (cx, cy, r) = (104i16, 118i16, 74i16);
        let mut a0: u32 = 0;
        for i in 0..n {
            let frac = (vals[i].unsigned_abs() * 4096 / total) as u32;
            let a1 = if i == n - 1 {
                4096
            } else {
                (a0 + frac).min(4096)
            };
            let c = SLICE_COLORS[i % SLICE_COLORS.len()];
            let mut a = a0;
            while a < a1 {
                let b = (a + 64).min(a1); // ~5.6-degree steps
                let (x1, y1) = pie_point(cx, cy, r, a);
                let (x2, y2) = pie_point(cx, cy, r, b);
                draw_tri_flat([(cx, cy), (x1, y1), (x2, y2)], c.0, c.1, c.2);
                a = b;
            }
            a0 = a1;
        }

        // Legend on the right.
        let lx = cx + r + 18;
        let mut ly = 26i16;
        let mut buf = [0u8; 16];
        for i in 0..n {
            if ly > 196 {
                break;
            }
            let c = SLICE_COLORS[i % SLICE_COLORS.len()];
            fill(lx, ly, 8, 8, c);
            let (col, row) = src[i];
            let label = if col > 0 {
                let lc = &sh.cells[idx(col - 1, row)];
                if matches!(lc.kind, Kind::Text) {
                    Some(str_of(lc.raw()))
                } else {
                    None
                }
            } else {
                None
            };
            let label = label.unwrap_or_else(|| fmt_value_fit(vals[i], 10, &mut buf));
            text(font, lx + 12, ly, clip(label, 12), t.text);
            ly += 14;
        }
    }

    /// Normalised range rectangle to shade: the range being picked mid-formula,
    /// or the Nav selection.
    fn shaded_range(&self) -> Option<(usize, usize, usize, usize)> {
        if self.mode == Mode::Pick {
            return self.pick_anchor.map(|(ac, ar)| {
                (
                    ac.min(self.pick_col),
                    ar.min(self.pick_row),
                    ac.max(self.pick_col),
                    ar.max(self.pick_row),
                )
            });
        }
        self.sel_anchor.map(|_| self.target_rect())
    }

    /// Bottom edge of the visible grid band. The on-screen keyboard covers the
    /// lower screen while typing, and a data row half-hidden behind its top edge
    /// reads as a glitch, so the band stops at the last whole row above it.
    fn grid_bottom(&self) -> i16 {
        if self.mode == Mode::Edit {
            KBD_GRID_Y1
        } else {
            GRID_Y1
        }
    }

    /// Ease `scroll_px` toward the row the cursor put in view. Exponential (a
    /// quarter of the remaining distance per frame) with a 1px floor so it
    /// always lands: a linear crawl over a 24-row page jump takes too long, and
    /// snapping is what looked cheap.
    fn ease_scroll(&mut self) {
        let target = self.top_row as i16 * ROW_H;
        let d = target - self.scroll_px;
        if d == 0 {
            return;
        }
        let step = (d / 4).abs().max(1).min(d.abs());
        self.scroll_px += if d > 0 { step } else { -step };
    }

    fn draw_grid(&self, font: &FontAtlas) {
        let sh = sheet();
        let t = theme();
        let (acol, arow) = self.active_cell();
        let range = self.shaded_range();
        let range_bg = mix(t.accent, t.bg);
        // Ranges named in the formula being typed, each in its own tint -- the
        // Excel trick that makes =SUM(A1:A9) legible at a glance.
        let refs = self.typed_refs();

        // Faint column separators for readability.
        let grid_h = (VIS_ROWS as i16) * ROW_H;
        for vc in 0..=VIS_COLS {
            let x = ROWHDR_W + (vc as i16) * COL_W;
            if x <= 320 {
                fill(x - 1, GRID_Y0 - 1, 1, grid_h as u16, t.sep);
            }
        }

        // Smooth vertical scroll: rows are drawn at a sub-row pixel offset and
        // the GPU scissor clips the partial row at each edge of the band.
        // ponytail: vertical only. Columns still snap -- 8 of 26 columns are on
        // screen against 24 of 50 rows, so paging vertically is where the jump
        // was actually visible. Same treatment would do for x if it ever grates.
        let off = self.scroll_px.rem_euclid(ROW_H);
        let first = (self.scroll_px / ROW_H) as usize;
        scissor_band(GRID_TOP, self.grid_bottom());
        for vr in 0..=VIS_ROWS {
            let row = first + vr;
            if row >= ROWS {
                break;
            }
            let y = GRID_Y0 + (vr as i16) * ROW_H - off;
            // Row header (1-based).
            let mut rb = [0u8; 4];
            let rs = u32_dec(&mut rb, (row + 1) as u32);
            let rtint = if row == arow { t.hot } else { t.dim };
            text(font, 2, y, rs, rtint);

            for vc in 0..VIS_COLS {
                let col = self.top_col + vc;
                if col >= COLS {
                    break;
                }
                let x = ROWHDR_W + (vc as i16) * COL_W;
                let selected = col == acol && row == arow;
                let in_range = range.is_some_and(|(c0, r0, c1, r1)| {
                    col >= c0 && col <= c1 && row >= r0 && row <= r1
                });
                let in_ref = refs.tint_at(col, row);
                if selected {
                    fill(x, y - 1, COL_W as u16, ROW_H as u16, self.cursor_color());
                    // Ants before the glyphs: an 8px row leaves the bottom border
                    // sharing pixels with the text, and a legible cell beats a
                    // complete rectangle.
                    self.draw_ants(x, y - 1, COL_W, ROW_H);
                } else if let Some(c) = in_ref {
                    fill(x, y - 1, COL_W as u16, ROW_H as u16, mix(c, t.bg));
                } else if in_range {
                    fill(x, y - 1, COL_W as u16, ROW_H as u16, range_bg);
                }
                self.draw_cell(
                    font,
                    sh,
                    col,
                    row,
                    x,
                    y,
                    selected || in_range || in_ref.is_some(),
                );
            }
        }
        scissor_reset();
    }

    /// The cursor cell's fill, breathing between the accent and a lifted version
    /// of it on a ~1s cycle. Alive without being distracting, and it survives
    /// the 15-bit colour depth because the two ends are several steps apart.
    fn cursor_color(&self) -> Rgb {
        let t = theme();
        let phase = self.frame % 64;
        let tri = if phase < 32 { phase } else { 64 - phase }; // 0..32..0
        lerp(t.accent, lerp(t.accent, t.hot, 1, 3), tri as u16, 32)
    }

    /// Marching-ants border: a 2-on/2-off dash pattern whose phase advances with
    /// the frame, so the cursor reads as live even when nothing is moving.
    fn draw_ants(&self, x: i16, y: i16, w: i16, h: i16) {
        let t = theme();
        let march = (self.frame / 4) as i16;
        // Walk the perimeter, painting a 2px dash every 4px.
        let perim = 2 * (w + h);
        let mut i = 0;
        while i < perim {
            if ((i + march) / 2) % 2 == 0 {
                i += 2;
                continue;
            }
            // Map the perimeter position onto an edge.
            let (px, py) = if i < w {
                (x + i, y)
            } else if i < w + h {
                (x + w - 1, y + (i - w))
            } else if i < 2 * w + h {
                (x + (2 * w + h - 1 - i), y + h - 1)
            } else {
                (x, y + (perim - 1 - i))
            };
            // Gold, not accent_text: the cell's own text is accent_text, and two
            // touching runs of the same colour read as noise rather than a border.
            fill(px, py, 2, 1, t.hot);
            i += 2;
        }
    }

    /// Cell rectangles named by the formula being typed, so `=SUM(A1:A9)+B2`
    /// lights up both A1:A9 and B2 while you build it.
    fn typed_refs(&self) -> RefHighlights {
        let mut out = RefHighlights::NONE;
        if !matches!(self.mode, Mode::Edit | Mode::Pick) || self.saving {
            return out;
        }
        if self.edit_len == 0 || self.edit_buf[0] != b'=' {
            return out;
        }
        out.scan(&self.edit_buf[..self.edit_len]);
        out
    }

    fn draw_cell(
        &self,
        font: &FontAtlas,
        sh: &Sheet,
        col: usize,
        row: usize,
        x: i16,
        y: i16,
        sel: bool,
    ) {
        let cell = &sh.cells[idx(col, row)];
        if cell.is_empty() {
            return;
        }
        let t = theme();
        let tint = if sel { t.accent_text } else { t.text };
        let mut buf = [0u8; 16];
        match cell.kind {
            Kind::Num => {
                // Fit the value in the column: full if it fits, else compact
                // scientific (never a left-truncated wrong number).
                let s = fmt_value_fit(cell.value, DISP, &mut buf);
                // right-align numbers in the column
                let xoff = ((DISP - s.len().min(DISP)) as i16) * GW;
                text(font, x + xoff, y, s, tint);
            }
            Kind::Err => {
                text(font, x, y, "#ERR", if sel { t.accent_text } else { t.bad });
            }
            _ => {
                // text: left-aligned, clipped to the column
                let s = clip(str_of(cell.raw()), DISP);
                text(font, x, y, s, tint);
            }
        }
    }
}

/// Cell rectangles named by the formula being typed, each with its own colour.
/// Four is enough for any formula that fits [`sheet::INPUT_CAP`] and keeps the
/// per-cell lookup a fixed four compares.
struct RefHighlights {
    rects: [(u8, u8, u8, u8); 4],
    n: usize,
}

impl RefHighlights {
    const NONE: RefHighlights = RefHighlights {
        rects: [(0, 0, 0, 0); 4],
        n: 0,
    };

    /// Scan `src` for `A1` references and `A1:B5` ranges, recording each as a
    /// rectangle. Mirrors the parser's notion of a reference (a single letter
    /// followed by digits) so `SUM` and `IF` are not mistaken for one.
    fn scan(&mut self, src: &[u8]) {
        let mut i = 0;
        while i < src.len() && self.n < self.rects.len() {
            if !src[i].is_ascii_alphabetic() {
                i += 1;
                continue;
            }
            let a0 = i;
            while i < src.len() && src[i].is_ascii_alphabetic() {
                i += 1;
            }
            if i - a0 != 1 {
                continue; // a function name
            }
            let Some((c0, r0)) = read_ref(src, a0, &mut i) else {
                continue;
            };
            // A ':' immediately after makes it a range.
            let mut rect = (c0, r0, c0, r0);
            if src.get(i) == Some(&b':') {
                let j = i + 1;
                if src.get(j).is_some_and(|c| c.is_ascii_alphabetic()) {
                    let mut k = j + 1;
                    if let Some((c1, r1)) = read_ref(src, j, &mut k) {
                        rect = (c0.min(c1), r0.min(r1), c0.max(c1), r0.max(r1));
                        i = k;
                    }
                }
            }
            self.rects[self.n] = rect;
            self.n += 1;
        }
    }

    /// The highlight colour for a cell, if any reference covers it.
    fn tint_at(&self, col: usize, row: usize) -> Option<Rgb> {
        let (col, row) = (col as u8, row as u8);
        for i in 0..self.n {
            let (c0, r0, c1, r1) = self.rects[i];
            if col >= c0 && col <= c1 && row >= r0 && row <= r1 {
                return Some(SLICE_COLORS[i % SLICE_COLORS.len()]);
            }
        }
        None
    }
}

/// Read the reference whose column letter is at `letter`, leaving `*end` past
/// the row digits. `None` if the digits are missing or out of the grid.
fn read_ref(src: &[u8], letter: usize, end: &mut usize) -> Option<(u8, u8)> {
    let col = src[letter].to_ascii_uppercase().checked_sub(b'A')?;
    let mut row1: usize = 0;
    let mut any = false;
    while *end < src.len() && src[*end].is_ascii_digit() {
        row1 = row1 * 10 + (src[*end] - b'0') as usize;
        *end += 1;
        any = true;
    }
    if !any || row1 == 0 || row1 > ROWS || col as usize >= COLS {
        return None;
    }
    Some((col, (row1 - 1) as u8))
}

/// Clip drawing to a horizontal band of the back buffer, for the smooth-scroll
/// partial rows. The engine re-points the draw area at the target buffer on
/// every swap, so both the narrowing and [`scissor_reset`] work in that
/// buffer's own VRAM rows.
fn scissor_band(y0: i16, y1: i16) {
    let base = BUFFER_Y.load();
    psx_gpu::set_draw_area(0, (base + y0) as u16, 319, (base + y1) as u16);
}

fn scissor_reset() {
    let base = BUFFER_Y.load();
    psx_gpu::set_draw_area(0, base as u16, 319, (base + 239) as u16);
}

/// VRAM row of the buffer currently being drawn, published by `render` so the
/// scissor helpers don't need `Ctx` threaded through every draw call.
struct BufferY(core::cell::Cell<i16>);
// SAFETY: single-threaded PS1; the value is written once per frame in `render`
// and read by the scissor helpers on the same call stack.
unsafe impl Sync for BufferY {}
impl BufferY {
    fn load(&self) -> i16 {
        self.0.get()
    }
    fn store(&self, v: i16) {
        self.0.set(v);
    }
}
static BUFFER_Y: BufferY = BufferY(core::cell::Cell::new(0));

/// Max data points a chart plots (more than this is unreadable at 320px).
const MAX_POINTS: usize = 64;

/// Map a fixed-point value offset `delta` (0..=`span`) to a pixel height in
/// `[0, plot_h]`. Done in `u64` (both inputs are non-negative here, and the
/// signed 64-bit divide is broken on this target). The product is kept in range
/// by halving a very large span, trading a pixel of precision for safety.
fn scale_px(delta: i64, span: i64, plot_h: i16) -> i16 {
    if span <= 0 || delta <= 0 {
        return 0;
    }
    let (mut d, mut s) = (delta as u64, span as u64);
    while s > 1 << 40 {
        d >>= 1;
        s >>= 1;
    }
    let px = d * plot_h as u64 / s;
    px.min(plot_h as u64) as i16
}

/// Average two colours (used for the dimmed range-selection fill).
fn mix(a: Rgb, b: Rgb) -> Rgb {
    lerp(a, b, 1, 2)
}

/// Blend `num/den` of the way from `a` to `b`. The PS1 has no cheap per-pixel
/// alpha for flat rects, so every fade/pulse in the UI is a colour lerp against
/// the (opaque) thing behind it.
fn lerp(a: Rgb, b: Rgb, num: u16, den: u16) -> Rgb {
    let (num, den) = (num.min(den) as i32, den.max(1) as i32);
    let ch = |x: u8, y: u8| -> u8 { (x as i32 + (y as i32 - x as i32) * num / den) as u8 };
    (ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

/// "3/17" into `out`, for the menu's scroll position. Returns the length.
fn build_frac(n: usize, total: usize, out: &mut [u8]) -> usize {
    let mut o = u32_dec(out, n as u32).len();
    out[o] = b'/';
    o += 1;
    let mut tmp = [0u8; 8];
    for &d in u32_dec(&mut tmp, total as u32).as_bytes() {
        out[o] = d;
        o += 1;
    }
    o
}

/// A keyboard whose highlight sits on the OK key, for R2's "jump to confirm".
///
/// psx-osk keeps its highlight private and exposes no `focus_ok`, so this walks
/// there from a known start: `Keyboard::new()` homes on the letters page at
/// (row 1, col 0), three Downs reach the function row, and four Rights land on
/// OK past Shift / Sym / Space / Del. `focus_ok_lands_on_ok` pins that walk.
///
/// ponytail: the cost is that shift/sym reset, which only matters if you then
/// D-pad away from OK instead of committing. A three-line `Keyboard::focus_ok()`
/// in psx-osk removes it -- worth doing next time that crate is touched, not
/// worth a cross-repo submodule bump on its own.
fn focus_ok() -> Keyboard {
    let mut kbd = Keyboard::new();
    for _ in 0..3 {
        kbd.step(Dir::Down);
    }
    for _ in 0..4 {
        kbd.step(Dir::Right);
    }
    kbd
}

/// A point on a circle of radius `r` about `(cx, cy)` at `angle_q12` (Q0.12).
fn pie_point(cx: i16, cy: i16, r: i16, angle_q12: u32) -> (i16, i16) {
    let a = (angle_q12 & 0x0FFF) as u16;
    let x = cx + ((cos_q12(a) * r as i32) / 4096) as i16;
    let y = cy + ((sin_q12(a) * r as i32) / 4096) as i16;
    (x, y)
}

/// "TRI:<next> View   O:Back" into `out`.
fn build_chart_hint<'a>(next: &str, out: &'a mut [u8]) -> &'a str {
    let mut o = 0;
    for &b in b"TRI:" {
        out[o] = b;
        o += 1;
    }
    for &b in next.as_bytes() {
        out[o] = b;
        o += 1;
    }
    for &b in b" View   O:Back" {
        out[o] = b;
        o += 1;
    }
    str_of(&out[..o])
}

/// Draw a chart line segment: [`draw_line_mono`] (GP0 0x40, native 1px line)
/// with the theme's tuple colour unpacked. Formerly a 2x2-quad DDA workaround
/// for the hw renderer skipping GP0 lines; that gap is closed at this pin.
fn draw_seg(x0: i16, y0: i16, x1: i16, y1: i16, c: (u8, u8, u8)) {
    draw_line_mono(x0, y0, x1, y1, c.0, c.1, c.2);
}

/// Fill a screen rect with a flat colour (draw-offset aware, double-buffer
/// safe): [`draw_rect_flat`] with the theme's tuple colour unpacked.
fn fill(x: i16, y: i16, w: u16, h: u16, c: (u8, u8, u8)) {
    draw_rect_flat(x, y, w, h, c.0, c.1, c.2);
}

/// Draw text in a literal palette colour. The counterpart to [`fill`]: both
/// take a real screen colour, and this one applies the [`theme::ink`]
/// texel-modulation correction that flat rects don't need.
fn text(font: &FontAtlas, x: i16, y: i16, s: &str, c: Rgb) {
    font.draw_text(x, y, s, theme::ink(c));
}

/// [`text`] at an integer scale, for the title.
fn text_scaled(font: &FontAtlas, x: i16, y: i16, s: &str, sx: u8, sy: u8, c: Rgb) {
    font.draw_text_scaled(x, y, s, sx, sy, theme::ink(c));
}

/// "B3" -> bytes, returns length. Column letter + 1-based row.
fn cell_ref(col: usize, row: usize, out: &mut [u8]) -> usize {
    out[0] = b'A' + col as u8;
    let n = u32_dec(&mut out[1..], (row + 1) as u32).len();
    out[1 + n] = b':';
    n + 2
}

/// "A1:B5" (no trailing colon) into `out`, returns the length.
fn range_ref(c0: usize, r0: usize, c1: usize, r1: usize, out: &mut [u8]) -> usize {
    let mut n = 0;
    out[n] = b'A' + c0 as u8;
    n += 1;
    n += u32_dec(&mut out[n..], (r0 + 1) as u32).len();
    out[n] = b':';
    n += 1;
    out[n] = b'A' + c1 as u8;
    n += 1;
    n += u32_dec(&mut out[n..], (r1 + 1) as u32).len();
    n
}

fn clip(s: &str, n: usize) -> &str {
    if s.len() <= n {
        s
    } else {
        &s[..n]
    }
}

fn str_of(b: &[u8]) -> &str {
    // SAFETY: every byte we render originates from ASCII cell text, formatted
    // numbers, or the ASCII keyboard set.
    unsafe { core::str::from_utf8_unchecked(b) }
}

// -------------------------------------------------------------------------
// Sample sheets, offered on the start menu.
// -------------------------------------------------------------------------

/// Optional chart to open when a sample is selected: (kind, data range).
type OpenChart = Option<(ChartKind, (usize, usize, usize, usize))>;

/// The start-menu sample sheets: (label, populate fn, chart to open on select).
const SAMPLES: &[(&str, fn(&mut Sheet), OpenChart)] = &[
    ("Blank", sample_blank, None),
    ("Budget", sample_budget, None),
    (
        "Sales chart",
        sample_sales,
        Some((ChartKind::Bar, (1, 1, 1, 12))),
    ), // B2:B13
    (
        "Expenses",
        sample_expenses,
        Some((ChartKind::Pie, (1, 0, 1, 4))),
    ), // B1:B5
    ("Grades", sample_grades, None),
    ("Fibonacci", sample_fibonacci, None),
    ("Functions", sample_functions, None),
];

fn sample_blank(sh: &mut Sheet) {
    sh.reset();
}

/// A small shopping list with per-row totals and a SUM.
fn sample_budget(sh: &mut Sheet) {
    sh.reset();
    sh.set(0, 0, b"ITEM");
    sh.set(1, 0, b"QTY");
    sh.set(2, 0, b"PRICE");
    sh.set(3, 0, b"TOTAL");
    sh.set(0, 1, b"APPLE");
    sh.set(1, 1, b"3");
    sh.set(2, 1, b"1.50");
    sh.set(3, 1, b"=B2*C2");
    sh.set(0, 2, b"BREAD");
    sh.set(1, 2, b"2");
    sh.set(2, 2, b"2.25");
    sh.set(3, 2, b"=B3*C3");
    sh.set(0, 3, b"MILK");
    sh.set(1, 3, b"1");
    sh.set(2, 3, b"1.20");
    sh.set(3, 3, b"=B4*C4");
    sh.set(0, 5, b"TOTAL");
    sh.set(3, 5, b"=SUM(D2:D4)");
}

/// Twelve months of numbers, ready to chart (this is the one that opens a chart).
fn sample_sales(sh: &mut Sheet) {
    sh.reset();
    sh.set(0, 0, b"MONTH");
    sh.set(1, 0, b"SALES");
    let months: [(&[u8; 3], &[u8; 2]); 12] = [
        (b"JAN", b"12"),
        (b"FEB", b"19"),
        (b"MAR", b"28"),
        (b"APR", b"22"),
        (b"MAY", b"35"),
        (b"JUN", b"41"),
        (b"JUL", b"38"),
        (b"AUG", b"45"),
        (b"SEP", b"33"),
        (b"OCT", b"29"),
        (b"NOV", b"24"),
        (b"DEC", b"31"),
    ];
    for (i, (m, v)) in months.iter().enumerate() {
        sh.set(0, 1 + i, *m);
        sh.set(1, 1 + i, *v);
    }
    sh.set(0, 13, b"TOTAL");
    sh.set(1, 13, b"=SUM(B2:B13)");
}

/// A cheat-sheet showing the built-in functions at work.
fn sample_functions(sh: &mut Sheet) {
    sh.reset();
    sh.set(3, 0, b"10"); // D1
    sh.set(3, 1, b"20"); // D2
    sh.set(3, 2, b"30"); // D3
    let rows: [(&[u8], &[u8]); 7] = [
        (b"SUM", b"=SUM(D1:D3)"),
        (b"AVG", b"=AVG(D1:D3)"),
        (b"COUNT", b"=COUNT(D1:D3)"),
        (b"IF>50", b"=IF(B1>50,1,0)"),
        (b"MOD", b"=MOD(17,5)"),
        (b"ROUND", b"=ROUND(3.14159)"),
        (b"BIG", b"=100000*100000"),
    ];
    for (i, (label, formula)) in rows.iter().enumerate() {
        sh.set(0, i, label);
        sh.set(1, i, formula);
    }
}

/// Monthly expenses by category -- opens straight into a pie chart.
fn sample_expenses(sh: &mut Sheet) {
    sh.reset();
    let items: [(&[u8], &[u8]); 5] = [
        (b"RENT", b"800"),
        (b"FOOD", b"400"),
        (b"CAR", b"250"),
        (b"FUN", b"150"),
        (b"MISC", b"100"),
    ];
    for (i, (name, amount)) in items.iter().enumerate() {
        sh.set(0, i, name);
        sh.set(1, i, amount);
    }
    sh.set(0, 6, b"TOTAL");
    sh.set(1, 6, b"=SUM(B1:B5)");
}

/// A gradebook: scores, a pass/fail column filled with a relative IF, and AVG.
fn sample_grades(sh: &mut Sheet) {
    sh.reset();
    sh.set(0, 0, b"NAME");
    sh.set(1, 0, b"SCORE");
    sh.set(2, 0, b"PASS");
    let students: [(&[u8], &[u8]); 5] = [
        (b"ANA", b"85"),
        (b"BEN", b"72"),
        (b"CAI", b"58"),
        (b"DOT", b"91"),
        (b"EVE", b"64"),
    ];
    for (i, (name, score)) in students.iter().enumerate() {
        sh.set(0, 1 + i, name);
        sh.set(1, 1 + i, score);
    }
    // Fill the pass/fail check down the column (relative refs shift per row).
    sh.set(2, 1, b"=IF(B2>=60,1,0)");
    sh.fill(2, 1, b"=IF(B2>=60,1,0)", 2, 1, 2, 5);
    sh.set(0, 7, b"AVG");
    sh.set(1, 7, b"=AVG(B2:B6)");
}

/// The Fibonacci sequence: each cell = the two above it (a fill of `=A2+A3`).
/// Shows relative-fill and the i64 range -- later terms use the compact display.
fn sample_fibonacci(sh: &mut Sheet) {
    sh.reset();
    sh.set(0, 0, b"FIB");
    sh.set(0, 1, b"1"); // A2
    sh.set(0, 2, b"1"); // A3
    sh.set(0, 3, b"=A2+A3"); // A4 = 2
    sh.fill(0, 3, b"=A2+A3", 0, 3, 0, 41); // fill A4:A42
}

// -------------------------------------------------------------------------
// Self-check for the bits of the UI that are pure logic. The rendering can only
// be checked by eye (see tools/shot.sh); these are the parts that silently
// break.
// -------------------------------------------------------------------------
#[cfg(test)]
mod ui_tests {
    use super::*;

    /// `focus_ok` walks a fixed path through psx-osk's private layout. If the
    /// SDK ever reorders the function row, this is what catches it -- otherwise
    /// R2 would quietly land on DEL and eat a character.
    #[test]
    fn focus_ok_lands_on_ok() {
        let mut kbd = focus_ok();
        assert!(matches!(kbd.activate(), Action::Commit));
    }

    /// The reference scanner behind the typed-range highlighting: it must find
    /// ranges and bare refs but never mistake a function name for one.
    #[test]
    fn ref_scan_finds_ranges_not_function_names() {
        let mut h = RefHighlights::NONE;
        h.scan(b"=SUM(A1:A3)+C2");
        assert_eq!(h.n, 2);
        assert_eq!(h.rects[0], (0, 0, 0, 2)); // A1:A3
        assert_eq!(h.rects[1], (2, 1, 2, 1)); // C2
        assert!(h.tint_at(0, 1).is_some()); // A2, inside the range
        assert!(h.tint_at(1, 1).is_none()); // B2, outside it
    }

    #[test]
    fn ref_scan_rejects_out_of_grid_and_partial_refs() {
        let mut h = RefHighlights::NONE;
        h.scan(b"=A99+B+Z1"); // row 99 > ROWS, "B" has no row
        assert_eq!(h.n, 1);
        assert_eq!(h.rects[0], (25, 0, 25, 0)); // just Z1
    }

    /// Caret editing: insert lands at the caret and backspace takes the
    /// character before it, so a typo mid-formula is fixable in place.
    #[test]
    fn caret_inserts_and_deletes_in_place() {
        let mut ed = test_editor();
        for &c in b"=A1+B2" {
            ed.push(c);
        }
        assert_eq!(&ed.edit_buf[..ed.edit_len], b"=A1+B2");
        // Walk back over "+B2" and fix the "1" to a "5".
        ed.caret = 3;
        ed.backspace(); // drop the '1'
        ed.push(b'5');
        assert_eq!(&ed.edit_buf[..ed.edit_len], b"=A5+B2");
        // Backspace at the start of the line is a no-op, not an underflow.
        ed.caret = 0;
        ed.backspace();
        assert_eq!(ed.edit_len, 6);
    }

    #[test]
    fn caret_insert_stops_at_capacity() {
        let mut ed = test_editor();
        for _ in 0..sheet::INPUT_CAP + 5 {
            ed.push(b'9');
        }
        assert_eq!(ed.edit_len, sheet::INPUT_CAP);
        assert_eq!(ed.caret, sheet::INPUT_CAP);
    }

    /// A minimal Editor for the pure-logic paths (no font, no GPU).
    fn test_editor() -> Editor {
        Editor {
            font: None,
            cur_col: 0,
            cur_row: 0,
            top_col: 0,
            top_row: 0,
            mode: Mode::Edit,
            edit_buf: [0; sheet::INPUT_CAP],
            edit_len: 0,
            caret: 0,
            kbd: Keyboard::new(),
            pick_col: 0,
            pick_row: 0,
            pick_anchor: None,
            clip: [[0; sheet::INPUT_CAP]; CLIP_MAX],
            clip_lens: [0; CLIP_MAX],
            clip_w: 0,
            clip_h: 0,
            clip_col: 0,
            clip_row: 0,
            sel_anchor: None,
            set_sel: 0,
            menu_top: 0,
            status: [0; 32],
            status_len: 0,
            status_ttl: 0,
            status_bad: false,
            undo_head: 0,
            undo_count: 0,
            ask: Ask::DeleteSave,
            frame: 0,
            scroll_px: 0,
            menu_sel: 0,
            has_save: false,
            saving: false,
            files: [EMPTY_ENTRY; MAX_FILES],
            file_count: 0,
            browse_sel: 0,
            browse_from_menu: false,
            chart_range: (0, 0, 0, 0),
            chart_kind: ChartKind::Bar,
            pad: PadTracker::new(),
        }
    }
}

// Not compiled under test: the no_mangle "main" symbol would collide with the
// libtest harness's entry point.
#[cfg(not(test))]
#[no_mangle]
fn main() -> ! {
    static mut SCENE: Editor = Editor {
        font: None,
        cur_col: 0,
        cur_row: 0,
        top_col: 0,
        top_row: 0,
        mode: Mode::Menu,
        edit_buf: [0; sheet::INPUT_CAP],
        edit_len: 0,
        caret: 0,
        kbd: Keyboard::new(),
        pick_col: 0,
        pick_row: 0,
        pick_anchor: None,
        clip: [[0; sheet::INPUT_CAP]; CLIP_MAX],
        clip_lens: [0; CLIP_MAX],
        clip_w: 0,
        clip_h: 0,
        clip_col: 0,
        clip_row: 0,
        sel_anchor: None,
        set_sel: 0,
        menu_top: 0,
        status: [0; 32],
        status_len: 0,
        status_ttl: 0,
        status_bad: false,
        undo_head: 0,
        undo_count: 0,
        ask: Ask::DeleteSave,
        frame: 0,
        scroll_px: 0,
        menu_sel: 0,
        has_save: false,
        saving: false,
        files: [EMPTY_ENTRY; MAX_FILES],
        file_count: 0,
        browse_sel: 0,
        browse_from_menu: false,
        chart_range: (0, 0, 0, 0),
        chart_kind: ChartKind::Bar,
        pad: PadTracker::new(),
    };
    let config = Config {
        clear_color: (8, 10, 18),
        ..Config::default()
    };
    App::run(config, unsafe { &mut *core::ptr::addr_of_mut!(SCENE) })
}
