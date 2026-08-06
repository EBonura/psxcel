//! Colour themes. A compact semantic palette (11 colours) keeps both the dark
//! and light variants readable and lets every draw call pull from one place.
//! The active theme lives in a global toggled from the Settings screen.

pub type Rgb = (u8, u8, u8);

/// Convert a palette colour into a psx-font *tint*.
///
/// The GPU modulates a textured primitive per texel: `out = texel * tint / 128`.
/// The glyph atlas is white, so passing a palette colour straight to
/// `draw_text` renders it at **2x** -- which washed out the light theme's dark
/// text and clamped every dark-theme colour to pure white. Halving here makes
/// one palette mean the same thing for text as it does for [`crate::fill`]'s
/// flat rects, which are already 1:1.
pub fn ink(c: Rgb) -> Rgb {
    (c.0 >> 1, c.1 >> 1, c.2 >> 1)
}

#[derive(Copy, Clone)]
pub struct Theme {
    pub bg: Rgb,          // screen background
    pub panel: Rgb,       // formula bar / header / keyboard backdrops
    pub key: Rgb,         // keyboard key face
    pub accent: Rgb,      // cursor cell + selected key
    pub accent_text: Rgb, // text drawn over `accent`
    pub text: Rgb,        // primary text
    pub dim: Rgb,         // headers, hints, secondary text
    pub good: Rgb,        // cell ref tag + formula result (green)
    pub bad: Rgb,         // errors (red)
    pub hot: Rgb,         // highlighted header / active toggle key
    pub sep: Rgb,         // grid separators
}

impl Theme {
    /// The psx-osk keyboard palette for this theme. Built per draw call so the
    /// dark/light toggle stays live while the keyboard is open.
    ///
    /// psx-osk fills key faces with `panel`/`key`/`hot`/`accent` and *tints
    /// text* with `accent_text`/`text`/`dim`, so only the latter three get the
    /// [`ink`] halving.
    pub fn osk(&self) -> psx_osk::Palette {
        psx_osk::Palette {
            panel: self.panel,
            key: self.key,
            hot: self.hot,
            accent: self.accent,
            accent_text: ink(self.accent_text),
            text: ink(self.text),
            dim: ink(self.dim),
        }
    }
}

// Spreadsheet-green identity (Excel / Google Sheets both lead with green): the
// selection/cursor accent and the chrome are green, with a gold highlight for
// the active row/column header and a mint "= result" echo.
static DARK: Theme = Theme {
    bg: (10, 16, 13),          // near-black, faint green
    panel: (20, 34, 26),       // dark green chrome (formula bar / headers / kbd)
    key: (38, 54, 44),         // green-gray key face
    accent: (34, 150, 82),     // spreadsheet-green selection
    accent_text: (255, 255, 255),
    text: (224, 230, 224),
    dim: (128, 150, 134),      // green-gray secondary
    good: (150, 235, 168),     // bright mint for = results / ref tag
    bad: (240, 110, 110),
    hot: (245, 214, 120),      // gold: active row/column header
    sep: (30, 48, 38),
};

static LIGHT: Theme = Theme {
    bg: (236, 243, 238),       // near-white, faint green
    panel: (198, 224, 206),    // soft green chrome
    key: (240, 246, 241),      // light key face
    accent: (30, 140, 74),     // green selection
    accent_text: (255, 255, 255),
    text: (22, 34, 26),        // near-black, slightly green
    dim: (86, 112, 94),        // green-gray secondary
    good: (22, 110, 52),       // deep green for = results / ref tag
    bad: (188, 42, 42),
    hot: (150, 110, 20),       // dark gold: active row/column header
    sep: (176, 198, 182),
};

// ponytail: one bool, not a settings struct. Add fields here if settings grow.
static mut DARK_MODE: bool = true;

pub fn theme() -> &'static Theme {
    if unsafe { DARK_MODE } {
        &DARK
    } else {
        &LIGHT
    }
}

pub fn is_dark() -> bool {
    unsafe { DARK_MODE }
}

pub fn set_dark(on: bool) {
    unsafe { DARK_MODE = on }
}
