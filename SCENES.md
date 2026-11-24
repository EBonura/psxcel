# PSXcel on the scene core

**Window update (11-23):** the branch now links for the 8,192 B window, `RAM_SIZE_OVERRIDE = 0x1FE000`, core at **0x801FDF00..0x801FFF00**. The figures below were taken at the earlier 10,240 B cap (0x1FD800, SP 0x801FD700) and are kept as measured; at 0x1FE000, computed, not built: free 1,638,552 B (1,636,504 + 2,048), SP 0x801FDF00; text, data and bss unchanged.

How PSXcel maps onto the SDK's scene system (resident core at 0x801FD700..0x801FFF00, handoff block
at 0x801FFF00..0x80200000, `SCENES.TBL` at member LBA 22). The row format and the link rules are the
core lane's CORE-ABI.md (campaign `scene-system-1123`, day 0, 11-23). Everything here is written
against that page and is provisional with it. Line numbers are at commit 9be0467 of this repo; SDK
lines are at the pin 6bfe478ff0e0 (`components.lock.json`).

## What the player sees

PSXcel is one engine scene, `Editor` (`game/src/main.rs:287`), switched between ten modes by a
`Mode` field (`main.rs:179-200`, dispatched in `update` at `main.rs:435-445`). Every mode shares the
same RAM image, the same VRAM and the same (empty) SPU RAM, and changing mode loads and frees nothing.

| What the player sees | In the program | Entered from | Resident in |
|---|---|---|---|
| Boot: black, then the engine's loading pass (no text) | `GameApp` loading state (SDK `engine/crates/psx-engine/src/game_app.rs:2238-2273`) | power-on, or a core switch | scene 0 |
| Splash | none: `boot_splash` is the default `None` (SDK `psx-engine/src/scene.rs:527`); no psx-splash symbol in the map | | n/a |
| Title menu: seven samples, plus "Browse saves" when slot 1 has a save | `Mode::Menu`, `draw_menu` (`main.rs:892`) | boot; command menu "Main menu" (`main.rs:1091-1094`) | scene 0 |
| Sheet grid | `Mode::Nav` | a sample (`open_selection`, `main.rs:873`), a loaded save | scene 0 |
| On-screen keyboard (cell edit, save name) | `Mode::Edit` | X / Triangle in the grid, "Save" | scene 0 |
| Point at a cell (formula reference, chart range) | `Mode::Pick` | R1 while editing, chart range pick | scene 0 |
| Chart (bar, line, area, pie) | `Mode::Chart` | "Chart", the Sales and Expenses samples | scene 0 |
| Command menu (the pause-like overlay) | `Mode::Cmd` | SELECT in the grid | scene 0 |
| Help | `Mode::Help` | "Help" | scene 0 |
| Memory card browser | `Mode::Browse`, `enter_browse` (`main.rs:1342`) | "Browse saves", "Load" | scene 0 |
| Delete confirm | `Mode::Confirm` | Square in the browser | scene 0 |
| Exit | none: `main` never returns (`main.rs:2649-2700`, `App::run` is `-> !`) | | n/a |

Rule this follows (Manny, 11-23): scene boundaries only where the player already waits today, and
every in-game screen stays resident in the playing scene. The keyboard, charts, command menu, help and
card browser are all resident in scene 0 with no disc read.

## Scene table rows

One row. The modes above are in-program states of that row, not table rows: they share all code and
data and nothing is loaded between them.

| id | kind | What it is | Fields that matter | return scene |
|---|---|---|---|---|
| 0 | 0 Program (boot scene: header field 0x10 = 0) | `PSX.EXE`, the whole program | `t_addr` 0x80010000, `t_size` 247,808 (0x3C800), file 122 sectors with the header, entry PC `__core_boot` once linked with psoxide-core.ld (`_start` today), SP 0x801FD700, name `PSXCEL` | 0xFFFF (none: CORE-ABI row 0x24, Manny 11-23 "no return-to-launcher") |

Data, Image, Module and Call rows: none. PSXcel never reads the disc after the BIOS load: every asset
(the SPLEEN 5x8 font, the sin table, the seven sample sheets) is compiled in, and saves go to the
memory card, not the disc.

On a demo disc PSXcel would be one Program row in the host's table with its own `lba_offset`. The Private
pressing leaves it out (`LEAVE_OUT="ARCADE PSXCEL"`, demo disc Makefile:410).

## Scene table v2 (v2, row layout pending)

The table moves to version 2 with two row kinds: IMAGE rows are load units (LBA and flags, sectors, load address, size, FNV, residency class BOOT_RESIDENT, RESIDENT_SET n or SWAP) and SCENE rows are entry points (scene id, image index, entry, name). Several scenes can share an image, several images can be in RAM at once, and switching to a scene whose image is loaded reads nothing. The byte layout is not fixed yet; the v1 rows above stay as the record of what was measured.

In v2 terms the program is one base image, `PSX.EXE` (image 0, BOOT_RESIDENT) and one scene, 0 `PSXCEL`. Every mode is an in-program state of image 0 and nothing is read from the disc after the BIOS load, so everything reachable in play is resident. No data rows.

## Boot splash: via .boot, one per disc (built)

Decision (scene-system plan, "Splash: one mechanism, no extra load"): the splash is not dropped and is not a separate scene. psx-splash links into the boot-only `.boot` section of the boot image (sdk/psoxide-boot.ld): stored after `.data`, run from the start of the heap, moved there by psx-rt's `__boot_entry` before `_start`, played once. Only a disc's first program plays it.

Built on SDK lane/disc-scenes-1123 610b97fb661c (pin commit 35b7a84), behind the off-by-default `scene-core` feature (commit 4ce62fb). `Editor::boot_splash` (`main.rs:431`) asks the engine for the splash, which plays it before `init` on a fresh GPU, so PSXcel's SPLEEN font (uploaded in `init` at x 320, where the splash puts BASIC) is untouched. PSXcel keeps no brightness setting, so the splash runs at the default step. `game/build.rs` links `-T psoxide-boot.ld` ahead of psoxide.ld as an ELF, and the Makefile writes the EXE from it with the SDK's `psx-exe` (`BOOT_EXE`, for `build` and `linked`). `make linked FEATURES=scene-core` builds it.

Measured (maps in the campaign's `itch-1123/core-window-keep/psxcel-610b/`: `off.map` is the default build after the feature commit, `scene-core.map`):

| | default | scene-core |
|---|---|---|
| exe header PC | `_start` | `__boot_entry` 0x80010674 |
| .text / .data / .bss | 235,136 / 11,224 / 105,476 | 238,096 / 11,240 / 105,476 |
| `.boot` | none | 8,600 B, runs at 0x80066A00..0x80068B98 (`ALIGN(__bss_end, 16)`), stored at 0x8004CDF8 (`__data_end`); `.boot_bss` empty |
| in `.boot` | | `psx_splash::play` 3,860, `logo::expand` 600, `draw_line` closure 264, `show::<App::run_with_flow::{closure}>` 44 (text); `logo::CODED` 2,746, `FONT_BITMAP` 1,024, `FONT` 28, `logo::PALETTE` 32 (rodata) |
| `__heap_start` (= `__bss_end`) | 0x80065E5C | 0x800669FC |
| resident free RAM (to `__heap_end` 0x801F5F00) | 1,638,564 | **1,635,588 (-2,976)** |
| exe stored size (header) | 247,808 B, 121 sectors | 258,048 B, 126 sectors (**+5 sectors**) |

The splash is not free in resident memory: 2,976 B stay. By symbol (map diff): `FontAtlas::upload` 424 to 1,460 (+1,036: a second caller, so it is no longer specialised for SPLEEN alone); the engine's splash path in `App::run_with_flow` +672 and its pad closure 48; psx-rt's mover `__boot_entry` 44, `__boot_entry_rs` 240 and the I-cache range flush 284; psx-gpu's `draw::<QuadTexturedGouraud>` 236, `draw::<QuadTexturedMaterial>` 200, `DoubleBuffer::clear` 120 and `FontAtlas::draw_text` 40, which only the splash calls but whose names do not carry `psx_splash`, so psoxide-boot.ld leaves them resident (596 B an SDK-side pattern could move); `upload_16bpp` +24, `enter_flow_state` +16, data +16. PSXcel links no heap allocator, so the `.boot` range sits unused above `__bss_end` after boot. `psoxide-size ram` (tools-hc-894d16439831) reports 1,626,984 for the scene-core map: it reads `__heap_start` from the map row's address column, the location counter after `.boot_bss` (0x80068B98), not the symbol (0x800669FC, `llvm-nm`).

Default build: the exe built after the feature commit is byte-identical to the exe of the repin commit 35b7a84 (sha256 6ac826f6...).

Run (frontend 31982f1e5c8d, headless, no input, 600 M instructions, 3,518 route ticks, 3,509 pad polls): the splash plays (tick 80) and the title menu is up at tick 170 with its own font. Frames kept as `core-window-keep/psxcel-boot-splash.png` and `psxcel-boot-first-screen.png`.

## Shared code versus per scene

With one row there is nothing per scene. Inside the row, everything is shared by every mode: the
engine loop (`psx_engine::App::run` wrapping `Editor` in `GameApp`, `main.rs:2695-2699`), the font,
the formula engine (`game/src/sheet.rs`), the memory card code (`psx_mc`, `main.rs:167-176` and
`main.rs:1303-1407`) and the statics (`SHEET` 52,000 B, `UNDO_RING` 24,576 B, `SNAP` 10,400 B,
`SAVE_BUF` and `COMP_SCRATCH` 8,192 B each). What the core holds instead of PSXcel: nothing PSXcel
uses today, since PSXcel never drives the CD.

## The relink (step 1, done)

`game/build.rs` writes `RAM_SIZE_OVERRIDE = 0x1FD800;` into `$OUT_DIR/ram-size.ld` and passes it as a
`-T` file before psoxide.ld (psoxide.ld:16-21), so the link map records it (map line 2). The value is
one constant in `game/build.rs`, marked provisional. Every target that links (`make build`, `linked`,
`gate`, `disc`) goes through build.rs, so all of them carry it.

Measured on `make linked OUT_DIR=build/ship` (ship disc), against the e482579 ship build kept by the
1122 lane (`psxcel-repin-1122/keep/e482579884e7/`):

| | before (e482579) | after (9be0467) |
|---|---|---|
| header SP (`STACK_INIT`) | 0x801FFF00 | 0x801FD700 |
| `__heap_end` (= `STACK_INIT - STACK_RESERVE`) | 0x801F7F00 | 0x801F5700 |
| `__bss_end`, highest linked byte | 0x80065E68 | 0x80065E68 |
| free RAM (`psoxide-size ram`) | 1,646,744 | 1,636,504 (-10,240) |
| .text / .data / .bss | 235,136 / 11,224 / 105,480 | the same |
| exe bytes that differ | | 1 (header byte 0x31, the SP's third byte) |

Nothing is linked at or above 0x801FD700. There is no heap (the `alloc` feature is off), and the stack
reserve is now 0x801F5700..0x801FD700.

## Conflicts with CORE-ABI.md

- RAM: none found. No PSXcel symbol and no SDK code it links writes above `STACK_INIT` (SDK grep for
  0x801Fxxxx addresses: only diagnostics in psx-rt `diag.rs`, behind `console-diag`, which this build
  does not enable).
- Disc: mkisopsx writes `PSX.EXE` at LBA 22 today. The table wants LBA 22 and the exe at 23, so the
  mkisopsx option in CORE-ABI 1 ("Who writes it") is needed before PSXcel can boot on the core. SDK branch lane/disc-scenes-1123 (d486a1eee) adds `mkisopsx --scenes`, which writes the table at 22 and the boot exe at 23; this repo's pin (6bfe) does not have it. No
  PSXcel code depends on the exe's LBA.
- Boot: `__core_boot` then psx-rt `_start` works as described in CORE-ABI 4.3. `_start` takes the
  stack from the header (psx-rt `lib.rs:164-168`) and reads only `$a0..$a2` (`lib.rs:172`, `lib.rs:199`), so a core
  entry with `$sp` = 0x801FD700 and `$a3` = 0x801FFF00 changes nothing for it.
- SIO0: PSXcel steals the controller port for every card operation (`HardwareCard::new`, five call
  sites: `main.rs:168, 1321, 1347, 1386, 1400`) while the engine polls the pad on the same port. The
  core never touches SIO0 (PLAN 3.9), so this is no conflict with the core, only the existing smell.

## What the core replaces (step 3): TODO, no code changed

PSXcel has no splash, no loader and no CD reader of its own, so there is nothing in this repo to put
behind a `scene-core` feature. The CD code in its image is SDK code that `GameApp` links whether or not
a program uses it, and that no PSXcel path reaches (no music tracks, no UI scenes; the real-model
journeys see no CD command). Measured on the 9be0467 ship map:

| What | Symbols | Bytes |
|---|---|---|
| psx-cdstream (CD IRQ engine) | `psx_cdstream::console::on_cd_interrupt` 2,188, `Engine::settle_drive` 696, `Engine::fail` 656, ... ; `psx_cdstream::console::ENGINE` (.data) 460 | 5,532 text + 504 data + 1 bss |
| psx-io CD and XA | `psx_io::cd::try_command_until_complete_inner` 576, `xa::Engine::begin_read_at` 544, ... | 3,856 text |
| engine CD state | `psx_engine::cd_drive::STATE` | 72 data |
| engine music player (the only caller of the above) | `MusicPlayer::update` 3,248, `MusicPlayer::release_for_data_reads` 580, `GameApp::update_ui_music` 2,444, ... | 6,628 text + 16 data |

That is 9,965 B of CD code plus 6,644 B of music code, all dead in PSXcel. Removing it is an SDK edit,
not a PSXcel one, and it is the same edit as the gameplay-only `App::run` in the audit
(scene-system-1123 `games/psxcel.md` section 8 step 1, about 86,000 B of unreachable `GameApp` front
end in all). The TODO for when the core exists:

1. Link the core: add `-T psoxide-core.ld` after the ram-size file in `game/build.rs` and depend on
   `psx-core` (feature `link`), per CORE-ABI 4.1. The header PC becomes `__core_boot`.
2. SDK, batched move: a gameplay-only `App::run` (or `GameApp` with its UI, music and CD arms compiled
   out under `GAMEPLAY_ONLY`), which drops the table above from PSXcel without a PSXcel change.
3. No exit row: CORE-ABI has `return scene` 0xFFFF and Manny's 11-23 rule is no return to a launcher,
   so PSXcel keeps "Main menu" as its own title and adds no `psx_core_switch` call.

## Load speed (Manny 11-23: every load the same speed or faster)

No code or data that is resident today becomes a disc read: PSXcel is one Program row and never reads
the disc after boot, today or on the core.

| Transition | Disc read under the core | Measured today | Slower? |
|---|---|---|---|
| Boot to first frame | BIOS loads `PSX.EXE`: 121 payload sectors today (0x3C800 B); with the core image stored after `.data` (CORE-ABI 4.1) the payload grows by the core's stored bytes, about 4 to 5 sectors at the 8,192 to 10,240 B cap **(estimate)** | smoke "menu" checkpoint at frame 43 on PCSX-ReARMed, 717-718 on PCSX-Redux (1122 PASS json, `cross`); journey `smoke-boot-nav` 68 ticks (baseline). Boot to first frame alone: not measured | **yes, slightly (estimate):** about 5 more sectors in the BIOS load, about 33 ms at double speed, plus `__core_boot`'s copy and word FNV over the window. Nothing in PSXcel keeps it equal; the core's 8,192 B target keeps it smallest |
| Any mode change (menu, grid, keyboard, chart, browser) | none | none (all in RAM) | no |

## Tests, bars and goldens that move

| Check | Moves? |
|---|---|
| `[[size]] ship` (tolerance 0, baseline `free = 1646744`) | yes, fails: free is 1,636,504 (-10,240). text, data and bss are unchanged. Needs a re-bless with the window as the reason |
| `[maps] ship` hash (baseline `4f1e020a...`) | yes: the map now has the `RAM_SIZE_OVERRIDE` line and its path |
| `[[clean]]` (`psxcel.bin`, `.cue`, `.exe`) | the exe changes by one byte versus the installed disc; a clean rebuild of 9be0467 should equal itself (not run) |
| install facts (`PSXcel.provenance.json`, sha pins) | yes, the exe and disc sha change |
| speed `nav-edit` (vblank histogram, p99) | not expected: no code or data address moves and the R3000 has no data cache, only the stack's address moves. Not measured |
| journeys (3) and `[cross]` | not expected: `PSXCEL_JOURNEY` keeps its address (no .data/.bss move). Not run |
| goldens | none (every checkpoint is `golden = false`) |
| pgo layouts | none (`layout = false`) |

## Correction: where the boot stack really starts

The note above that psx-rt `_start` takes its stack from the exe header is wrong. `_start` never
sets `$sp`; the BIOS sets it, and when it boots the disc it takes `STACK = 801FFFF0` from the
SYSTEM.CNF that psx-iso writes for every disc (`crates/psx-iso/src/iso9660.rs:714`), not the
header's SP. So after the relink the header says 0x801FD700 (or the final window's base) but the
stack still starts at 0x801FFFF0, inside the core window. Measured on Quake's ship-boot profile
(`entry_sp = 0x801ffff0`); not measured here, same boot path. Harmless until the core exists:
`__core_boot` sets `$sp` before `_start`, or SYSTEM.CNF's STACK line has to follow the window.

## On the core (11-24, SDK e7c26f8dc19c, branch lane/scene-core-1124)

PSXcel links psx-core and the scripts in CORE-ABI order in every build (no feature). The disc is a scene
disc from `scenes.manifest`: SCENES.TBL at LBA 22, PSX.EXE at 23 (row: 260,096 B at 0x80010000, core
hash 0x2dd28a19). The splash plays when the BIOS booted PSXcel and not when a core switch entered it
(handoff block flag bit 0). Measured on PSoXide (frontends/31982f1e5c8d), ship builds:

| What | Installed e482579 | On the core |
|---|---|---|
| Boot to the first menu frame (VBlanks) | 14 | 169 with the splash, 14 without (runtime-skipped variant) |
| speed nav-edit (anchored), cycles a frame | 548,948 | 549,051 (+0.02%), vblanks 1:999 both |
| text / free RAM (psoxide-size) | 235,136 / 1,646,744 | 237,916 / 1,635,764 |

Found: psx-splash leaves its 8x8 font in texture page x 320 (its doc says so) and PSXcel's 3x title
sampled one texel row past its own glyphs, showing a stray underline under the "l"; `init` now clears
the page's 64x32 atlas rows before uploading SPLEEN_5X8.

On a collection disc PSXcel is `program PSXCEL.EXE psxcel.exe` plus `scene PSXCEL PSXCEL.EXE start`;
it reads nothing from the disc after its load and switches to nothing.
