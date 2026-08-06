//! Spreadsheet model: a fixed grid of cells, a recursive-descent formula
//! evaluator, and fixed-point number formatting.
//!
//! Values are `i64` fixed-point scaled by [`ONE`] (`DECIMALS` places). The PS1
//! has no hardware 64-bit and the compiler's *signed* 64-bit divide is broken
//! on this target, so the arithmetic is done in `u64` magnitude with the sign
//! handled by hand (see the note by [`fmul`]). No float, no heap.

use psx_math::fmt::u64_dec;

pub const COLS: usize = 26; // A..Z
pub const ROWS: usize = 50; // 1..50
pub const NCELLS: usize = COLS * ROWS;
pub const INPUT_CAP: usize = 24; // max raw characters per cell

/// Recalc value snapshot, kept off the stack (see [`Sheet::recalc`]).
static mut SNAP: [i64; NCELLS] = [0; NCELLS];
/// Companion "is this cell numeric" snapshot, for `COUNT`.
static mut NUMS: [bool; NCELLS] = [false; NCELLS];

/// Decimal places of fixed-point precision. This is the one knob for the whole
/// numeric type. Cell values are `i64` (storage range ±9.2e18 / ONE, so
/// effectively unbounded for a spreadsheet). The practical limit is that a
/// multiply's scaled product / a divide's scaled numerator must fit `u64`:
///
/// | DECIMALS | multiply & divide operands up to |
/// |----------|----------------------------------|
/// | 4        | ~180 billion                     |
/// | 6        | ~18 million                      |
/// | 8        | ~1.8 million                     |
///
/// 4 gives four decimals with a range far beyond any real sheet; raise it for
/// more precision on small-valued data. Overflow returns `None` -> `#ERR`.
pub const DECIMALS: usize = 4;
/// The fixed-point scale, `10^DECIMALS`. A cell value is stored as `real * ONE`.
const ONE: i64 = 10i64.pow(DECIMALS as u32);

// NOTE ON 64-BIT: the PSX R3000A has no hardware 64-bit. The compiler's *signed*
// 64-bit divide (`__divdi3`) was broken on this target; `psx-rt` now ships a
// correct override (see `sdk/examples/hello-i64probe`), so plain `i64 / i64`
// works SDK-wide. We still do the fixed-point math in `u64` magnitude here for a
// second reason: it gives one extra bit of range over signed i64 for the
// multiply/divide intermediates. This is slower than i32 (the house rule bans
// i64 in perf-critical game code) but a spreadsheet only recalculates on edit,
// so the cost is invisible -- and it buys huge range + more decimals.

/// Fixed-point multiply `a*b/ONE`, via unsigned magnitude. Overflow -> `None`.
fn fmul(a: i64, b: i64) -> Option<i64> {
    let neg = (a < 0) ^ (b < 0);
    let mag = a.unsigned_abs().checked_mul(b.unsigned_abs())? / ONE as u64;
    let mag = i64::try_from(mag).ok()?;
    Some(if neg { -mag } else { mag })
}

/// Fixed-point divide `a*ONE/b`, via unsigned magnitude. Overflow / div0 -> `None`.
fn fdiv(a: i64, b: i64) -> Option<i64> {
    if b == 0 {
        return None;
    }
    let mag = a.unsigned_abs().checked_mul(ONE as u64)? / b.unsigned_abs();
    let mag = i64::try_from(mag).ok()?;
    Some(if (a < 0) ^ (b < 0) { -mag } else { mag })
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Kind {
    Empty,
    Num,
    Text,
    Err,
}

#[derive(Copy, Clone)]
pub struct Cell {
    pub buf: [u8; INPUT_CAP],
    pub len: u8,
    pub value: i64, // fixed-point (scaled by ONE); meaningful for Num, else 0
    pub kind: Kind,
}

impl Cell {
    const EMPTY: Cell = Cell {
        buf: [0; INPUT_CAP],
        len: 0,
        value: 0,
        kind: Kind::Empty,
    };
    pub fn raw(&self) -> &[u8] {
        &self.buf[..self.len as usize]
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

pub struct Sheet {
    pub cells: [Cell; NCELLS],
}

pub const fn idx(col: usize, row: usize) -> usize {
    row * COLS + col
}

impl Sheet {
    pub const fn new() -> Self {
        Sheet {
            cells: [Cell::EMPTY; NCELLS],
        }
    }

    /// Empty every cell (no recalc; the caller repopulates and the following
    /// `set`s recalc). Used when loading a fresh sample sheet.
    pub fn reset(&mut self) {
        for c in self.cells.iter_mut() {
            *c = Cell::EMPTY;
        }
    }

    /// Overwrite a cell's raw text (truncated to `INPUT_CAP`) and recalc.
    pub fn set(&mut self, col: usize, row: usize, text: &[u8]) {
        let c = &mut self.cells[idx(col, row)];
        let n = text.len().min(INPUT_CAP);
        c.buf[..n].copy_from_slice(&text[..n]);
        c.len = n as u8;
        self.recalc();
    }

    /// Write `raw` into one cell with every relative reference shifted by
    /// `(dcol, drow)`, the mechanic behind both fill-down and paste. Plain
    /// numbers and text are copied verbatim (only a leading `=` marks a
    /// formula whose references should move).
    ///
    /// No recalc: callers write a whole rectangle then [`Sheet::recalc`] once.
    pub fn set_shifted(&mut self, col: usize, row: usize, raw: &[u8], dcol: i32, drow: i32) {
        if col >= COLS || row >= ROWS {
            return;
        }
        let mut buf = [0u8; INPUT_CAP];
        let n = if raw.first() == Some(&b'=') {
            rewrite_refs(raw, Shift::Fill { dcol, drow }, &mut buf).min(INPUT_CAP)
        } else {
            let n = raw.len().min(INPUT_CAP);
            buf[..n].copy_from_slice(&raw[..n]);
            n
        };
        let cell = &mut self.cells[idx(col, row)];
        cell.buf[..n].copy_from_slice(&buf[..n]);
        cell.len = n as u8;
    }

    /// Fill the rectangle `(c0,r0)..=(c1,r1)` from a source cell's raw text,
    /// each target's references shifted by its offset from `(src_col, src_row)`
    /// (the spreadsheet "fill down" mechanic). Recalculates once at the end.
    pub fn fill(
        &mut self,
        src_col: usize,
        src_row: usize,
        raw: &[u8],
        c0: usize,
        r0: usize,
        c1: usize,
        r1: usize,
    ) {
        for row in r0..=r1 {
            for col in c0..=c1 {
                let dcol = col as i32 - src_col as i32;
                let drow = row as i32 - src_row as i32;
                self.set_shifted(col, row, raw, dcol, drow);
            }
        }
        self.recalc();
    }

    /// Empty every cell in a rectangle, then recalc once.
    pub fn clear_rect(&mut self, c0: usize, r0: usize, c1: usize, r1: usize) {
        for row in r0..=r1.min(ROWS - 1) {
            for col in c0..=c1.min(COLS - 1) {
                self.cells[idx(col, row)] = Cell::EMPTY;
            }
        }
        self.recalc();
    }

    /// SUM and numeric-cell COUNT over a rectangle, for the selection readout.
    /// Non-numeric cells contribute nothing. The caller derives AVG from these
    /// (`sum / count`), so an all-text selection reports COUNT 0 rather than a
    /// misleading average.
    pub fn stats(&self, c0: usize, r0: usize, c1: usize, r1: usize) -> (i64, u32) {
        let mut sum = 0i64;
        let mut count = 0u32;
        for row in r0..=r1.min(ROWS - 1) {
            for col in c0..=c1.min(COLS - 1) {
                let cell = &self.cells[idx(col, row)];
                if matches!(cell.kind, Kind::Num) {
                    sum = sum.saturating_add(cell.value);
                    count += 1;
                }
            }
        }
        (sum, count)
    }

    /// Evaluate `raw` against the sheet's stored values *without* storing it,
    /// for the live result readout while a formula is being typed.
    ///
    /// Shares the recalc snapshot statics; safe because both run to completion
    /// on the single-threaded PS1 and neither is reentrant.
    pub fn preview(&self, raw: &[u8]) -> (i64, Kind) {
        let snap = unsafe { &mut *core::ptr::addr_of_mut!(SNAP) };
        let nums = unsafe { &mut *core::ptr::addr_of_mut!(NUMS) };
        for i in 0..NCELLS {
            snap[i] = self.cells[i].value;
            nums[i] = matches!(self.cells[i].kind, Kind::Num);
        }
        classify(raw, snap, nums)
    }

    /// Insert a blank row above `at`, pushing everything below it down one and
    /// dropping whatever fell off the bottom.
    pub fn insert_row(&mut self, at: usize) {
        if at >= ROWS {
            return;
        }
        for row in (at + 1..ROWS).rev() {
            for col in 0..COLS {
                self.cells[idx(col, row)] = self.cells[idx(col, row - 1)];
            }
        }
        for col in 0..COLS {
            self.cells[idx(col, at)] = Cell::EMPTY;
        }
        self.rewrite_all(Shift::Rows { at, delta: 1 });
        self.recalc();
    }

    /// Delete row `at`, pulling everything below it up one.
    pub fn delete_row(&mut self, at: usize) {
        if at >= ROWS {
            return;
        }
        for row in at..ROWS - 1 {
            for col in 0..COLS {
                self.cells[idx(col, row)] = self.cells[idx(col, row + 1)];
            }
        }
        for col in 0..COLS {
            self.cells[idx(col, ROWS - 1)] = Cell::EMPTY;
        }
        self.rewrite_all(Shift::Rows { at, delta: -1 });
        self.recalc();
    }

    /// Insert a blank column left of `at`, pushing the rest right one.
    pub fn insert_col(&mut self, at: usize) {
        if at >= COLS {
            return;
        }
        for col in (at + 1..COLS).rev() {
            for row in 0..ROWS {
                self.cells[idx(col, row)] = self.cells[idx(col - 1, row)];
            }
        }
        for row in 0..ROWS {
            self.cells[idx(at, row)] = Cell::EMPTY;
        }
        self.rewrite_all(Shift::Cols { at, delta: 1 });
        self.recalc();
    }

    /// Delete column `at`, pulling the rest left one.
    pub fn delete_col(&mut self, at: usize) {
        if at >= COLS {
            return;
        }
        for col in at..COLS - 1 {
            for row in 0..ROWS {
                self.cells[idx(col, row)] = self.cells[idx(col + 1, row)];
            }
        }
        for row in 0..ROWS {
            self.cells[idx(COLS - 1, row)] = Cell::EMPTY;
        }
        self.rewrite_all(Shift::Cols { at, delta: -1 });
        self.recalc();
    }

    /// Re-point every formula in the sheet under `shift`. Non-formula cells are
    /// left alone.
    fn rewrite_all(&mut self, shift: Shift) {
        for i in 0..NCELLS {
            let src = self.cells[i]; // Cell is Copy: read before writing back
            if src.raw().first() != Some(&b'=') {
                continue;
            }
            let mut buf = [0u8; INPUT_CAP];
            let n = rewrite_refs(src.raw(), shift, &mut buf).min(INPUT_CAP);
            let cell = &mut self.cells[i];
            cell.buf[..n].copy_from_slice(&buf[..n]);
            cell.len = n as u8;
        }
    }

    /// Serialize non-empty cells into `out` for a memory-card save. Compact
    /// record stream: `u16 count`, then `col, row, len, raw[len]` per cell.
    /// Returns the byte length, or `None` if `out` is too small.
    pub fn serialize(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < 2 {
            return None;
        }
        let mut p = 2;
        let mut count: u16 = 0;
        for i in 0..NCELLS {
            let c = &self.cells[i];
            if c.len == 0 {
                continue;
            }
            let need = 3 + c.len as usize;
            if p + need > out.len() {
                return None;
            }
            out[p] = (i % COLS) as u8;
            out[p + 1] = (i / COLS) as u8;
            out[p + 2] = c.len;
            out[p + 3..p + 3 + c.len as usize].copy_from_slice(c.raw());
            p += need;
            count += 1;
        }
        out[0..2].copy_from_slice(&count.to_le_bytes());
        Some(p)
    }

    /// Replace the whole sheet from a [`Sheet::serialize`] byte stream, then
    /// recalculate. Returns `false` on a malformed/truncated stream (the sheet
    /// is left cleared in that case).
    pub fn deserialize(&mut self, data: &[u8]) -> bool {
        for c in self.cells.iter_mut() {
            *c = Cell::EMPTY;
        }
        if data.len() < 2 {
            return false;
        }
        let count = u16::from_le_bytes([data[0], data[1]]) as usize;
        let mut p = 2;
        for _ in 0..count {
            if p + 3 > data.len() {
                return false;
            }
            let col = data[p] as usize;
            let row = data[p + 1] as usize;
            let len = data[p + 2] as usize;
            p += 3;
            if p + len > data.len() {
                return false;
            }
            if col < COLS && row < ROWS && len <= INPUT_CAP {
                let cell = &mut self.cells[idx(col, row)];
                cell.buf[..len].copy_from_slice(&data[p..p + len]);
                cell.len = len as u8;
            }
            p += len;
        }
        self.recalc();
        true
    }

    /// Fixpoint recalculation. Snapshot every value, re-evaluate every cell
    /// against the snapshot, repeat until nothing changes. An acyclic
    /// dependency chain of any depth resolves in one `set`; a cycle simply
    /// stops at the iteration cap and leaves the last values.
    // ponytail: naive fixpoint over all cells, no dependency graph. Runs only
    // on edit, not per frame; NCELLS is small. Add a topo-sorted dep graph only
    // if edits ever feel slow.
    pub fn recalc(&mut self) {
        // The i64 snapshot (10 KiB) lives in a static, not on the small PS1
        // stack. recalc is not reentrant (single-threaded, runs on edit).
        let snap = unsafe { &mut *core::ptr::addr_of_mut!(SNAP) };
        let nums = unsafe { &mut *core::ptr::addr_of_mut!(NUMS) };
        for _pass in 0..(ROWS + 2) {
            for i in 0..NCELLS {
                snap[i] = self.cells[i].value;
                nums[i] = matches!(self.cells[i].kind, Kind::Num);
            }
            let mut changed = false;
            for i in 0..NCELLS {
                let (val, kind) = classify(self.cells[i].raw(), snap, nums);
                if val != self.cells[i].value {
                    changed = true;
                }
                self.cells[i].value = val;
                self.cells[i].kind = kind;
            }
            if !changed {
                break;
            }
        }
    }
}

/// Decide what a raw cell is and its numeric value.
/// - empty            -> (0, Empty)
/// - leading '='      -> formula, evaluated against `snap`
/// - parses as number -> that value, Num
/// - anything else    -> text (0, Text)
fn classify(raw: &[u8], snap: &[i64], nums: &[bool]) -> (i64, Kind) {
    if raw.is_empty() {
        return (0, Kind::Empty);
    }
    if raw[0] == b'=' {
        let mut p = Parser {
            b: &raw[1..],
            pos: 0,
            snap,
            nums,
        };
        return match p.parse_full() {
            Some(v) => (v, Kind::Num),
            None => (0, Kind::Err),
        };
    }
    match parse_number_full(raw) {
        Some(v) => (v, Kind::Num),
        None => (0, Kind::Text),
    }
}

// --------------------------------------------------------------------------
// Number parse / format (fixed-point hundredths)
// --------------------------------------------------------------------------

/// Parse a bare decimal number consuming the WHOLE slice, else `None`.
fn parse_number_full(b: &[u8]) -> Option<i64> {
    let mut p = 0usize;
    let v = read_number(b, &mut p)?;
    if p == b.len() {
        Some(v)
    } else {
        None
    }
}

/// Read `[-]?digits[.digits]?` starting at `*p`, advancing `*p`. Value returned
/// scaled by `ONE`. Requires at least one digit somewhere.
fn read_number(b: &[u8], p: &mut usize) -> Option<i64> {
    let start = *p;
    let mut neg = false;
    if *p < b.len() && (b[*p] == b'-' || b[*p] == b'+') {
        neg = b[*p] == b'-';
        *p += 1;
    }
    let mut int_part: i64 = 0;
    let mut any = false;
    while *p < b.len() && b[*p].is_ascii_digit() {
        int_part = int_part.checked_mul(10)?.checked_add((b[*p] - b'0') as i64)?;
        *p += 1;
        any = true;
    }
    let mut frac: i64 = 0;
    if *p < b.len() && b[*p] == b'.' {
        *p += 1;
        // Weight of the first fractional digit (ONE/10), stepping down each digit;
        // digits past DECIMALS places have place 0 and are ignored.
        let mut place = ONE / 10;
        while *p < b.len() && b[*p].is_ascii_digit() {
            if place >= 1 {
                frac += (b[*p] - b'0') as i64 * place;
                place /= 10;
            }
            *p += 1;
            any = true;
        }
    }
    if !any {
        *p = start;
        return None;
    }
    let mag = int_part.checked_mul(ONE)?.checked_add(frac)?;
    Some(if neg { -mag } else { mag })
}

/// Format an `i64` fixed-point value into `out`, returning the written `&str`.
/// Integers print without a decimal point; fractions print up to `DECIMALS`
/// places with trailing zeros trimmed ("12.5000" -> "12.5", "12.0000" -> "12").
pub fn fmt_value<'a>(v: i64, out: &'a mut [u8]) -> &'a str {
    let mut n = 0usize;
    let neg = v < 0;
    let mag = v.unsigned_abs(); // u64; handles i64::MIN
    let one = ONE as u64;
    let int_part = mag / one; // u64 divide (works; signed i64 divide does not)
    let mut frac = (mag % one) as u32; // < ONE, fits u32 for DECIMALS <= 9

    if neg {
        out[n] = b'-';
        n += 1;
    }
    n += u64_dec(&mut out[n..], int_part).len();
    if frac != 0 {
        out[n] = b'.';
        n += 1;
        // Zero-pad the fraction to DECIMALS digits, then trim trailing zeros.
        let mut digits = [0u8; DECIMALS];
        for d in digits.iter_mut().rev() {
            *d = b'0' + (frac % 10) as u8;
            frac /= 10;
        }
        let mut end = DECIMALS;
        while end > 0 && digits[end - 1] == b'0' {
            end -= 1;
        }
        for &d in &digits[..end] {
            out[n] = d;
            n += 1;
        }
    }
    // SAFETY: only ASCII digits, '-' and '.' written.
    unsafe { core::str::from_utf8_unchecked(&out[..n]) }
}

/// Format `v` to fit within `width` characters. If the full number fits it is
/// returned as-is; otherwise it falls back to compact scientific notation
/// (`1.23e8`) so a wide value shows its magnitude instead of being truncated to
/// a wrong number. `width` should be >= 4; `out` must hold at least `width` bytes.
pub fn fmt_value_fit<'a>(v: i64, width: usize, out: &'a mut [u8]) -> &'a str {
    let mut full = [0u8; 24];
    let s = fmt_value(v, &mut full);
    let sb = s.as_bytes();
    if sb.len() <= width {
        out[..sb.len()].copy_from_slice(sb);
        // SAFETY: fmt_value only writes ASCII.
        return unsafe { core::str::from_utf8_unchecked(&out[..sb.len()]) };
    }

    // Compact scientific: <sign> D [.frac] e EXP, sized to `width`.
    let neg = v < 0;
    let int_mag = v.unsigned_abs() / ONE as u64; // integer part (>= 1 here)
    let mut digs = [0u8; 20];
    let ndig = u64_dec(&mut digs, int_mag).len();
    let exp = ndig - 1;

    let mut es = [0u8; 8];
    es[0] = b'e';
    let en = 1 + u64_dec(&mut es[1..], exp as u64).len();

    let overhead = en + neg as usize;
    let mant_budget = width.saturating_sub(overhead).max(1);
    // First significant digit, then up to (mant_budget - 2) fractional digits.
    let mut mant = [0u8; 8];
    let mut mn = 0;
    mant[mn] = digs[0];
    mn += 1;
    if mant_budget >= 3 {
        let slots = mant_budget - 2;
        let mut fbuf = [0u8; 8];
        let mut fnn = 0;
        for i in 0..slots {
            if 1 + i < ndig {
                fbuf[fnn] = digs[1 + i];
                fnn += 1;
            }
        }
        while fnn > 0 && fbuf[fnn - 1] == b'0' {
            fnn -= 1;
        }
        if fnn > 0 {
            mant[mn] = b'.';
            mn += 1;
            for &d in &fbuf[..fnn] {
                mant[mn] = d;
                mn += 1;
            }
        }
    }

    let mut o = 0;
    if neg {
        out[o] = b'-';
        o += 1;
    }
    for &d in &mant[..mn] {
        out[o] = d;
        o += 1;
    }
    for &d in &es[..en] {
        out[o] = d;
        o += 1;
    }
    // SAFETY: only ASCII digits, '-', '.', 'e' written.
    unsafe { core::str::from_utf8_unchecked(&out[..o]) }
}

/// How [`rewrite_refs`] moves each reference it finds.
#[derive(Copy, Clone)]
enum Shift {
    /// Fill / paste: every reference moves by `(dcol, drow)`, clamped to the grid.
    Fill { dcol: i32, drow: i32 },
    /// A row was inserted at `at` (`delta` = +1) or deleted (`delta` = -1).
    /// References below the line follow their data; on a delete, references
    /// *to* the removed row are invalidated.
    Rows { at: usize, delta: i32 },
    /// The column equivalent of [`Shift::Rows`].
    Cols { at: usize, delta: i32 },
}

/// Where a reference to 0-based `(col, row)` ends up. `None` means the
/// reference pointed at a row/column that has just been deleted.
fn shift_ref(shift: Shift, col: i32, row: i32) -> Option<(i32, i32)> {
    let cmax = COLS as i32 - 1;
    let rmax = ROWS as i32 - 1;
    match shift {
        Shift::Fill { dcol, drow } => {
            Some(((col + dcol).clamp(0, cmax), (row + drow).clamp(0, rmax)))
        }
        Shift::Rows { at, delta } => {
            let at = at as i32;
            if delta < 0 && row == at {
                None
            } else if row >= at {
                Some((col, (row + delta).clamp(0, rmax)))
            } else {
                Some((col, row))
            }
        }
        Shift::Cols { at, delta } => {
            let at = at as i32;
            if delta < 0 && col == at {
                None
            } else if col >= at {
                Some(((col + delta).clamp(0, cmax), row))
            } else {
                Some((col, row))
            }
        }
    }
}

/// Rewrite every relative cell reference in `src` under `shift`. A reference is
/// a single letter immediately followed by digits, so multi-letter function
/// names like `SUM`/`IF` are copied verbatim, as are operators, numbers and
/// `:`. Returns the number of bytes written to `out` (truncated to fit).
///
/// A reference whose row/column was deleted is written with row `0`, which the
/// parser rejects -- so the formula shows `#ERR` instead of silently reading
/// whatever slid into that slot. (Excel writes `#REF!`; same intent, and this
/// needs no extra sentinel in the grammar.)
fn rewrite_refs(src: &[u8], shift: Shift, out: &mut [u8]) -> usize {
    let mut i = 0;
    let mut o = 0;
    while i < src.len() {
        let c = src[i];
        if c.is_ascii_alphabetic() {
            // Consume the alphabetic run.
            let a0 = i;
            while i < src.len() && src[i].is_ascii_alphabetic() {
                i += 1;
            }
            if i - a0 == 1 && i < src.len() && src[i].is_ascii_digit() {
                // Relative reference: <letter><digits>.
                let col = (src[a0].to_ascii_uppercase() - b'A') as i32;
                let mut row1: i32 = 0;
                while i < src.len() && src[i].is_ascii_digit() {
                    row1 = row1 * 10 + (src[i] - b'0') as i32;
                    i += 1;
                }
                let (ncol, nrow) = match shift_ref(shift, col, row1 - 1) {
                    Some((c, r)) => (c, r + 1),
                    None => (col, 0), // deleted target -> "A0" -> #ERR
                };
                if o < out.len() {
                    out[o] = b'A' + ncol as u8;
                    o += 1;
                }
                let mut tmp = [0u8; 6];
                for &d in u64_dec(&mut tmp, nrow as u64).as_bytes() {
                    if o < out.len() {
                        out[o] = d;
                        o += 1;
                    }
                }
            } else {
                // Function name / keyword: copy the run verbatim.
                for &b in &src[a0..i] {
                    if o < out.len() {
                        out[o] = b;
                        o += 1;
                    }
                }
            }
        } else {
            if o < out.len() {
                out[o] = c;
                o += 1;
            }
            i += 1;
        }
    }
    o
}

// --------------------------------------------------------------------------
// Formula evaluator: recursive descent over the cell text.
//
//   cmp    := expr (('=' | '<' | '>' | '<=' | '>=' | '<>') expr)?
//   expr   := term (('+' | '-') term)*
//   term   := factor (('*' | '/') factor)*
//   factor := number | cellref | func | '(' cmp ')' | ('-' | '+') factor
//   func   := (SUM|AVG|MIN|MAX|COUNT) '(' ref ':' ref ')'
//           | (ABS|ROUND|INT) '(' cmp ')'
//           | MOD '(' cmp ',' cmp ')'
//           | IF  '(' cmp ',' cmp ',' cmp ')'
//   ref    := [A-Z][0-9]+
//
// All arithmetic is scaled by ONE. A comparison yields 1.0 (true) or 0 (false);
// IF treats any non-zero as true.
// --------------------------------------------------------------------------

/// Boolean truth as a fixed-point value (1.0).
const TRUE_V: i64 = ONE;

/// Floor to a whole number (rounds toward negative infinity, so `INT(-2.5) = -3`).
fn fint(v: i64) -> i64 {
    let q = (v.unsigned_abs() / ONE as u64) as i64;
    if v < 0 {
        // Round down: subtract one more unit if there was any fraction.
        let frac = v.unsigned_abs() % ONE as u64 != 0;
        (-q - frac as i64) * ONE
    } else {
        q * ONE
    }
}

/// Modulo with the sign of the divisor (Excel `MOD`): `MOD(-3,2) = 1`. All in
/// `u64` magnitude to stay off the (fixed but conventionally-avoided) signed
/// 64-bit path. `ONE` divides out, so the scaled remainder is exact.
fn fmod(a: i64, b: i64) -> Option<i64> {
    if b == 0 {
        return None;
    }
    let r_mag = (a.unsigned_abs() % b.unsigned_abs()) as i64;
    let mut r = if a < 0 { -r_mag } else { r_mag };
    if r != 0 && (r < 0) != (b < 0) {
        r += b;
    }
    Some(r)
}

/// Round a fixed-point value to the nearest whole number (half away from zero).
fn round_int(v: i64) -> i64 {
    let bump = if v >= 0 { ONE / 2 } else { -(ONE / 2) };
    ((v + bump) / ONE) * ONE
}

#[derive(Copy, Clone)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

struct Parser<'a> {
    b: &'a [u8],
    pos: usize,
    snap: &'a [i64],
    nums: &'a [bool],
}

impl<'a> Parser<'a> {
    fn parse_full(&mut self) -> Option<i64> {
        self.skip_ws();
        let v = self.cmp()?;
        self.skip_ws();
        if self.pos == self.b.len() {
            Some(v)
        } else {
            None
        }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.b.len() && self.b[self.pos] == b' ' {
            self.pos += 1;
        }
    }

    /// Consume `ch` (after whitespace), or fail the parse.
    fn eat(&mut self, ch: u8) -> Option<()> {
        self.skip_ws();
        if self.peek() == Some(ch) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    /// A single optional comparison over two arithmetic expressions.
    fn cmp(&mut self) -> Option<i64> {
        let a = self.expr()?;
        self.skip_ws();
        let op = match self.peek() {
            Some(b'=') => {
                self.pos += 1;
                Cmp::Eq
            }
            Some(b'<') => {
                self.pos += 1;
                match self.peek() {
                    Some(b'=') => {
                        self.pos += 1;
                        Cmp::Le
                    }
                    Some(b'>') => {
                        self.pos += 1;
                        Cmp::Ne
                    }
                    _ => Cmp::Lt,
                }
            }
            Some(b'>') => {
                self.pos += 1;
                match self.peek() {
                    Some(b'=') => {
                        self.pos += 1;
                        Cmp::Ge
                    }
                    _ => Cmp::Gt,
                }
            }
            _ => return Some(a),
        };
        let b = self.expr()?;
        let t = match op {
            Cmp::Eq => a == b,
            Cmp::Ne => a != b,
            Cmp::Lt => a < b,
            Cmp::Le => a <= b,
            Cmp::Gt => a > b,
            Cmp::Ge => a >= b,
        };
        Some(if t { TRUE_V } else { 0 })
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn expr(&mut self) -> Option<i64> {
        let mut acc = self.term()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'+') => {
                    self.pos += 1;
                    acc = acc.checked_add(self.term()?)?;
                }
                Some(b'-') => {
                    self.pos += 1;
                    acc = acc.checked_sub(self.term()?)?;
                }
                _ => return Some(acc),
            }
        }
    }

    fn term(&mut self) -> Option<i64> {
        let mut acc = self.factor()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some(b'*') => {
                    self.pos += 1;
                    acc = fmul(acc, self.factor()?)?;
                }
                Some(b'/') => {
                    self.pos += 1;
                    acc = fdiv(acc, self.factor()?)?;
                }
                _ => return Some(acc),
            }
        }
    }

    fn factor(&mut self) -> Option<i64> {
        self.skip_ws();
        match self.peek()? {
            b'(' => {
                self.pos += 1;
                let v = self.cmp()?;
                self.eat(b')')?;
                Some(v)
            }
            b'-' => {
                self.pos += 1;
                Some(self.factor()?.checked_neg()?)
            }
            b'+' => {
                self.pos += 1;
                self.factor()
            }
            c if c.is_ascii_digit() || c == b'.' => read_number(self.b, &mut self.pos),
            c if c.is_ascii_alphabetic() => self.name(),
            _ => None,
        }
    }

    /// A leading letter is either a function call or a single-letter cell
    /// reference. Case-insensitive: `a1` == `A1`, `if` == `IF`, so the PS4-style
    /// lowercase-default keyboard just works.
    fn name(&mut self) -> Option<i64> {
        let start = self.pos;
        while self.pos < self.b.len() && self.b[self.pos].is_ascii_alphabetic() {
            self.pos += 1;
        }
        let word = &self.b[start..self.pos];
        if self.peek() == Some(b'(') {
            // Fold the name to uppercase for dispatch (max function name = 5).
            let mut fold = [0u8; 5];
            if word.len() > fold.len() {
                return None;
            }
            for (i, &c) in word.iter().enumerate() {
                fold[i] = c.to_ascii_uppercase();
            }
            let name = &fold[..word.len()];
            self.pos += 1; // consume '('
            return match name {
                b"IF" => {
                    let cond = self.cmp()?;
                    self.eat(b',')?;
                    let then = self.cmp()?;
                    self.eat(b',')?;
                    let els = self.cmp()?;
                    self.eat(b')')?;
                    Some(if cond != 0 { then } else { els })
                }
                b"ABS" => {
                    let v = self.cmp()?;
                    self.eat(b')')?;
                    v.checked_abs()
                }
                b"ROUND" => {
                    let v = self.cmp()?;
                    self.eat(b')')?;
                    Some(round_int(v))
                }
                b"INT" => {
                    let v = self.cmp()?;
                    self.eat(b')')?;
                    Some(fint(v))
                }
                b"MOD" => {
                    let a = self.cmp()?;
                    self.eat(b',')?;
                    let b = self.cmp()?;
                    self.eat(b')')?;
                    fmod(a, b)
                }
                b"SUM" | b"AVG" | b"MIN" | b"MAX" | b"COUNT" => {
                    let (c0, r0) = self.cellref_coords()?;
                    self.eat(b':')?;
                    let (c1, r1) = self.cellref_coords()?;
                    self.eat(b')')?;
                    self.range_fn(name, c0, r0, c1, r1)
                }
                _ => None,
            };
        }
        // Cell reference: single letter followed by row digits.
        if word.len() != 1 {
            return None;
        }
        let col = (word[0].to_ascii_uppercase() - b'A') as usize;
        let row = self.read_row()?;
        cell_at(self.snap, col, row)
    }

    /// Parse a "A12" style ref starting at `pos`, returning 0-based (col,row).
    fn cellref_coords(&mut self) -> Option<(usize, usize)> {
        self.skip_ws();
        let c = self.peek()?;
        if !c.is_ascii_alphabetic() {
            return None;
        }
        self.pos += 1;
        let col = (c.to_ascii_uppercase() - b'A') as usize;
        let row = self.read_row()?;
        if col >= COLS || row >= ROWS {
            return None;
        }
        Some((col, row))
    }

    /// Read a 1-based row number after a column letter, return 0-based index.
    fn read_row(&mut self) -> Option<usize> {
        let mut r: usize = 0;
        let mut any = false;
        while self.pos < self.b.len() && self.b[self.pos].is_ascii_digit() {
            r = r * 10 + (self.b[self.pos] - b'0') as usize;
            self.pos += 1;
            any = true;
        }
        if !any || r == 0 {
            return None;
        }
        Some(r - 1)
    }

    fn range_fn(&self, word: &[u8], c0: usize, r0: usize, c1: usize, r1: usize) -> Option<i64> {
        let (ca, cb) = (c0.min(c1), c0.max(c1));
        let (ra, rb) = (r0.min(r1), r0.max(r1));
        let mut sum: i64 = 0;
        let mut count: i64 = 0;
        let mut numeric: i64 = 0;
        let mut mn = i64::MAX;
        let mut mx = i64::MIN;
        for row in ra..=rb {
            for col in ca..=cb {
                let i = idx(col, row);
                let v = self.snap[i];
                sum = sum.checked_add(v)?;
                count += 1;
                if self.nums[i] {
                    numeric += 1;
                }
                if v < mn {
                    mn = v;
                }
                if v > mx {
                    mx = v;
                }
            }
        }
        match word {
            b"SUM" => Some(sum),
            b"AVG" => {
                if count == 0 {
                    None
                } else {
                    Some(sum / count)
                }
            }
            b"MIN" => Some(mn),
            b"MAX" => Some(mx),
            // COUNT of numeric cells, returned as a whole number (scaled).
            b"COUNT" => numeric.checked_mul(ONE),
            _ => None,
        }
    }
}

/// Read a referenced cell's cached numeric value from the snapshot.
fn cell_at(snap: &[i64], col: usize, row: usize) -> Option<i64> {
    if col >= COLS || row >= ROWS {
        return None;
    }
    Some(snap[idx(col, row)])
}

// --------------------------------------------------------------------------
// Self-check (host-only): `make test` from the repo root runs this suite on
// the host target. Kept out of the no_std PSX build.
// --------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn eval(expr: &str) -> (i64, Kind) {
        classify(expr.as_bytes(), &[0i64; NCELLS], &[false; NCELLS])
    }

    #[test]
    fn numbers_and_arithmetic() {
        assert_eq!(eval("12.5").0, 12 * ONE + ONE / 2);
        assert_eq!(eval("=2+3").0, 5 * ONE);
        assert_eq!(eval("=2*3.5").0, 7 * ONE);
        assert_eq!(eval("=10/4").0, 5 * ONE / 2); // 2.5
        assert_eq!(eval("=(1+2)*3").0, 9 * ONE);
        assert_eq!(eval("=-5+2").0, -3 * ONE);
        // Values well beyond the old i32 hundredths range now fit.
        assert_eq!(eval("=100000*5").0, 500_000 * ONE);
        assert!(matches!(eval("hello").1, Kind::Text));
        assert!(matches!(eval("=1/0").1, Kind::Err));
    }

    #[test]
    fn comparisons_and_conditionals() {
        assert_eq!(eval("=2<3").0, TRUE_V);
        assert_eq!(eval("=3<2").0, 0);
        assert_eq!(eval("=5=5").0, TRUE_V);
        assert_eq!(eval("=5<>5").0, 0);
        assert_eq!(eval("=3>=3").0, TRUE_V);
        assert_eq!(eval("=4<=3").0, 0);
        assert_eq!(eval("=IF(1,10,20)").0, 10 * ONE);
        assert_eq!(eval("=IF(0,10,20)").0, 20 * ONE);
        assert_eq!(eval("=IF(2>1,100,200)").0, 100 * ONE);
        assert_eq!(eval("=IF(2>3,100,200)").0, 200 * ONE);
        // Case-insensitive + nested arithmetic in branches.
        assert_eq!(eval("=if(1<0, 1+1, 3*2)").0, 6 * ONE);
    }

    #[test]
    fn abs_and_round() {
        assert_eq!(eval("=ABS(-5)").0, 5 * ONE);
        assert_eq!(eval("=ABS(5)").0, 5 * ONE);
        assert_eq!(eval("=ROUND(2.7)").0, 3 * ONE);
        assert_eq!(eval("=ROUND(2.4)").0, 2 * ONE);
        assert_eq!(eval("=ROUND(2.5)").0, 3 * ONE);
        assert_eq!(eval("=ROUND(-2.6)").0, -3 * ONE);
        // Composes with the rest of the grammar.
        assert_eq!(eval("=ABS(3-8)+1").0, 6 * ONE);
    }

    #[test]
    fn int_and_mod() {
        assert_eq!(eval("=INT(2.7)").0, 2 * ONE);
        assert_eq!(eval("=INT(-2.5)").0, -3 * ONE); // floor toward -inf
        assert_eq!(eval("=INT(5)").0, 5 * ONE);
        assert_eq!(eval("=MOD(7,3)").0, ONE); // 1
        assert_eq!(eval("=MOD(5.5,2)").0, 3 * ONE / 2); // 1.5
        assert_eq!(eval("=MOD(-3,2)").0, ONE); // Excel: sign of divisor -> 1
        assert_eq!(eval("=MOD(3,-2)").0, -ONE); // -> -1
        assert!(matches!(eval("=MOD(5,0)").1, Kind::Err));
    }

    #[test]
    fn count_numeric_cells() {
        let mut snap = [0i64; NCELLS];
        let mut nums = [false; NCELLS];
        // A1=1 (num), A2 text, A3=3 (num), A4 empty.
        snap[idx(0, 0)] = ONE;
        nums[idx(0, 0)] = true;
        snap[idx(0, 2)] = 3 * ONE;
        nums[idx(0, 2)] = true;
        let mut p = Parser {
            b: b"COUNT(A1:A4)",
            pos: 0,
            snap: &snap,
            nums: &nums,
        };
        assert_eq!(p.parse_full(), Some(2 * ONE)); // two numeric cells
    }

    #[test]
    fn fit_formatting() {
        let mut b = [0u8; 16];
        // Fits: returned as-is.
        assert_eq!(fmt_value_fit(12 * ONE + ONE / 2, 6, &mut b), "12.5");
        assert_eq!(fmt_value_fit(999_999 * ONE, 6, &mut b), "999999");
        // Too wide -> compact scientific (never a truncated wrong number).
        assert_eq!(fmt_value_fit(1_000_000 * ONE, 6, &mut b), "1e6");
        assert_eq!(fmt_value_fit(123_456_789 * ONE, 6, &mut b), "1.23e8");
        assert_eq!(fmt_value_fit(-(123_456_789 * ONE), 6, &mut b), "-1.2e8");
        // All results stay within the width.
        for v in [1_i64, 50, 999, 1_000_000, 123_456_789, -987_654_321] {
            assert!(fmt_value_fit(v * ONE, 6, &mut b).len() <= 6);
        }
    }

    #[test]
    fn refs_and_ranges() {
        let mut snap = [0i64; NCELLS];
        snap[idx(0, 0)] = ONE; // A1 = 1
        snap[idx(0, 1)] = 2 * ONE; // A2 = 2
        snap[idx(0, 2)] = 3 * ONE; // A3 = 3
        let mut p = Parser {
            b: b"A1+A2*A3",
            pos: 0,
            snap: &snap,
            nums: &[false; NCELLS],
        };
        assert_eq!(p.parse_full(), Some(7 * ONE)); // 1 + 2*3
        let mut p = Parser {
            b: b"SUM(A1:A3)",
            pos: 0,
            snap: &snap,
            nums: &[false; NCELLS],
        };
        assert_eq!(p.parse_full(), Some(6 * ONE));
        let mut p = Parser {
            b: b"AVG(A1:A3)",
            pos: 0,
            snap: &snap,
            nums: &[false; NCELLS],
        };
        assert_eq!(p.parse_full(), Some(2 * ONE));
    }

    #[test]
    fn case_insensitive_refs_and_funcs() {
        let mut snap = [0i64; NCELLS];
        snap[idx(0, 0)] = 5 * ONE; // A1 = 5
        snap[idx(0, 1)] = 3 * ONE; // A2 = 3
        let mut p = Parser {
            b: b"a1+a2",
            pos: 0,
            snap: &snap,
            nums: &[false; NCELLS],
        };
        assert_eq!(p.parse_full(), Some(8 * ONE));
        let mut p = Parser {
            b: b"sum(a1:a2)",
            pos: 0,
            snap: &snap,
            nums: &[false; NCELLS],
        };
        assert_eq!(p.parse_full(), Some(8 * ONE));
    }

    fn adj(src: &str, dc: i32, dr: i32) -> String {
        rw(src, Shift::Fill { dcol: dc, drow: dr })
    }

    fn rw(src: &str, shift: Shift) -> String {
        let mut out = [0u8; 32];
        let n = rewrite_refs(src.as_bytes(), shift, &mut out);
        String::from_utf8(out[..n].to_vec()).unwrap()
    }

    fn raw_at(sh: &Sheet, col: usize, row: usize) -> String {
        String::from_utf8(sh.cells[idx(col, row)].raw().to_vec()).unwrap()
    }

    #[test]
    fn relative_ref_adjust() {
        assert_eq!(adj("=B2*C2", 0, 1), "=B3*C3");
        assert_eq!(adj("=A1+5", 1, 0), "=B1+5");
        assert_eq!(adj("=SUM(A1:A3)", 0, 2), "=SUM(A3:A5)");
        assert_eq!(adj("=IF(D5>5,1,0)", 1, 0), "=IF(E5>5,1,0)");
        // Clamped at the grid edge instead of going negative.
        assert_eq!(adj("=A1", -1, 0), "=A1");
        // Row digit growth (9 -> 12) is handled.
        assert_eq!(adj("=A9+A9", 0, 3), "=A12+A12");
    }

    #[test]
    fn fill_down_shifts_refs() {
        let mut sh = Sheet::new();
        sh.set(1, 1, b"3"); // B2
        sh.set(2, 1, b"1.50"); // C2
        sh.set(1, 2, b"2"); // B3
        sh.set(2, 2, b"2.25"); // C3
        sh.set(3, 1, b"=B2*C2"); // D2 = 4.5
        // Fill D2's formula down into D2:D3.
        sh.fill(3, 1, b"=B2*C2", 3, 1, 3, 2);
        assert_eq!(sh.cells[idx(3, 1)].raw(), b"=B2*C2");
        assert_eq!(sh.cells[idx(3, 1)].value, 9 * ONE / 2); // 4.5
        assert_eq!(sh.cells[idx(3, 2)].raw(), b"=B3*C3"); // shifted
        assert_eq!(sh.cells[idx(3, 2)].value, 9 * ONE / 2); // 2 * 2.25
    }

    #[test]
    fn row_insert_shifts_refs_below() {
        assert_eq!(rw("=A5+A2", Shift::Rows { at: 3, delta: 1 }), "=A6+A2");
        assert_eq!(rw("=SUM(A4:A9)", Shift::Rows { at: 3, delta: 1 }), "=SUM(A5:A10)");
        // A reference above the inserted line is untouched.
        assert_eq!(rw("=A1", Shift::Rows { at: 3, delta: 1 }), "=A1");
    }

    #[test]
    fn row_delete_invalidates_refs_to_that_row() {
        // Rows below the deletion follow their data up...
        assert_eq!(rw("=A9", Shift::Rows { at: 3, delta: -1 }), "=A8");
        // ...and a reference to the deleted row itself becomes unparseable.
        assert_eq!(rw("=A4", Shift::Rows { at: 3, delta: -1 }), "=A0");
        assert!(matches!(eval("=A0").1, Kind::Err));
    }

    #[test]
    fn col_insert_and_delete_shift_refs() {
        assert_eq!(rw("=C1+A1", Shift::Cols { at: 1, delta: 1 }), "=D1+A1");
        assert_eq!(rw("=C1", Shift::Cols { at: 1, delta: -1 }), "=B1");
        assert_eq!(rw("=B1", Shift::Cols { at: 1, delta: -1 }), "=B0"); // deleted
    }

    #[test]
    fn insert_row_moves_data_and_repoints_formulas() {
        let mut sh = Sheet::new();
        sh.set(0, 0, b"10"); // A1
        sh.set(0, 1, b"20"); // A2
        sh.set(1, 1, b"=A2*2"); // B2 -> 40
        sh.insert_row(1); // blank row between A1 and the old A2
        assert_eq!(raw_at(&sh, 0, 0), "10"); // A1 stayed
        assert!(sh.cells[idx(0, 1)].is_empty()); // A2 is the new blank row
        assert_eq!(raw_at(&sh, 0, 2), "20"); // data slid to A3
        assert_eq!(raw_at(&sh, 1, 2), "=A3*2"); // formula followed its data
        assert_eq!(sh.cells[idx(1, 2)].value, 40 * ONE);
    }

    #[test]
    fn delete_row_pulls_up_and_repoints_formulas() {
        let mut sh = Sheet::new();
        sh.set(0, 0, b"1");
        sh.set(0, 1, b"2");
        sh.set(0, 2, b"3");
        sh.set(1, 0, b"=A3"); // B1 -> 3
        sh.delete_row(1); // remove the "2"
        assert_eq!(raw_at(&sh, 0, 1), "3"); // pulled up to A2
        assert_eq!(raw_at(&sh, 1, 0), "=A2"); // still points at the 3
        assert_eq!(sh.cells[idx(1, 0)].value, 3 * ONE);
    }

    #[test]
    fn delete_column_orphans_a_reference_to_it() {
        let mut sh = Sheet::new();
        sh.set(1, 0, b"7"); // B1
        sh.set(2, 0, b"=B1"); // C1 -> 7
        sh.delete_col(1); // B is gone; the formula slides into B1
        assert_eq!(raw_at(&sh, 1, 0), "=B0");
        assert!(matches!(sh.cells[idx(1, 0)].kind, Kind::Err));
    }

    #[test]
    fn stats_ignore_non_numeric_cells() {
        let mut sh = Sheet::new();
        sh.set(0, 0, b"10");
        sh.set(0, 1, b"NAME"); // text: contributes nothing
        sh.set(0, 2, b"20");
        // A4 left empty.
        let (sum, count) = sh.stats(0, 0, 0, 3);
        assert_eq!(sum, 30 * ONE);
        assert_eq!(count, 2); // so AVG = 15, not 7.5
        assert_eq!(sum / count as i64, 15 * ONE);
    }

    #[test]
    fn preview_evaluates_without_storing() {
        let mut sh = Sheet::new();
        sh.set(0, 0, b"6");
        let (v, kind) = sh.preview(b"=A1*7");
        assert_eq!(v, 42 * ONE);
        assert!(matches!(kind, Kind::Num));
        // The sheet is untouched: B1 is still empty.
        assert!(sh.cells[idx(1, 0)].is_empty());
        // A half-typed formula reports Err, which is what greys out the readout.
        assert!(matches!(sh.preview(b"=A1*").1, Kind::Err));
    }

    #[test]
    fn clear_rect_empties_only_the_rectangle() {
        let mut sh = Sheet::new();
        for row in 0..4 {
            sh.set(0, row, b"1");
            sh.set(1, row, b"2");
        }
        sh.clear_rect(0, 1, 0, 2); // A2:A3
        assert_eq!(raw_at(&sh, 0, 0), "1");
        assert!(sh.cells[idx(0, 1)].is_empty());
        assert!(sh.cells[idx(0, 2)].is_empty());
        assert_eq!(raw_at(&sh, 0, 3), "1");
        assert_eq!(raw_at(&sh, 1, 1), "2"); // column B untouched
    }

    #[test]
    fn serialize_roundtrip() {
        let mut a = Sheet::new();
        a.set(0, 0, b"ITEM");
        a.set(1, 1, b"3");
        a.set(2, 1, b"1.50");
        a.set(3, 1, b"=B2*C2");
        a.set(25, 49, b"corner");
        let mut buf = [0u8; 2048];
        let n = a.serialize(&mut buf).unwrap();

        let mut b = Sheet::new();
        assert!(b.deserialize(&buf[..n]));
        assert_eq!(b.cells[idx(0, 0)].raw(), b"ITEM");
        assert_eq!(b.cells[idx(3, 1)].raw(), b"=B2*C2");
        assert_eq!(b.cells[idx(3, 1)].value, 9 * ONE / 2); // 4.5, recomputed after load
        assert_eq!(b.cells[idx(25, 49)].raw(), b"corner");
        // Cells not saved come back empty.
        assert!(b.cells[idx(5, 5)].is_empty());
    }

    #[test]
    fn serialize_out_of_space() {
        let mut a = Sheet::new();
        a.set(0, 0, b"hello");
        let mut tiny = [0u8; 4];
        assert!(a.serialize(&mut tiny).is_none());
    }

    #[test]
    fn formatting() {
        let mut b = [0u8; 16];
        assert_eq!(fmt_value(12 * ONE + ONE / 2, &mut b), "12.5");
        assert_eq!(fmt_value(12 * ONE, &mut b), "12");
        assert_eq!(fmt_value(-(3 * ONE + ONE / 20), &mut b), "-3.05"); // 0.05 = ONE/20
        assert_eq!(fmt_value(0, &mut b), "0");
        // A four-decimal value now round-trips through the formatter.
        assert_eq!(fmt_value(ONE / 8, &mut b), "0.125"); // 0.1250 -> "0.125"
        // A value far beyond the old i32 range formats fine.
        assert_eq!(fmt_value(1_234_567 * ONE, &mut b), "1234567");
    }
}
