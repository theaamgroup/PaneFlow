//! Color conversion + APCA contrast enforcement for the terminal renderer.
//!
//! Two responsibilities, related only because both translate terminal colors
//! into final `Hsla` values for painting:
//!
//! 1. **APCA contrast** (`ensure_minimum_contrast`): fixes foreground/background
//!    pairs that fail the APCA (Accessible Perceptual Contrast Algorithm) Lc
//!    threshold. Polarity-aware and perceptually uniform - more accurate than
//!    WCAG 2.0 on dark backgrounds. Matches Zed's algorithm.
//! 2. **Color resolution** (`convert_color`, `named_color`, `indexed_color`):
//!    translates the neutral [`crate::terminal::types::Color`] (Named / Spec /
//!    Indexed) into themed `Hsla`, covering the xterm-256color palette.
//!
//! Extracted from `terminal_element.rs` per US-009 of the src-app refactor PRD.

use gpui::{Hsla, Rgba};

use crate::terminal::types::{Color, NamedColor};
use crate::theme::TerminalTheme;

// ---------------------------------------------------------------------------
// Minimum contrast (APCA - Accessible Perceptual Contrast Algorithm)
// ---------------------------------------------------------------------------

/// APCA constants (0.0.98G-4g W3 compatible).
/// https://github.com/Myndex/apca-w3
struct ApcaConstants {
    main_trc: f32,
    s_rco: f32,
    s_gco: f32,
    s_bco: f32,
    norm_bg: f32,
    norm_txt: f32,
    rev_txt: f32,
    rev_bg: f32,
    blk_thrs: f32,
    blk_clmp: f32,
    scale_bow: f32,
    scale_wob: f32,
    lo_bow_offset: f32,
    lo_wob_offset: f32,
    delta_y_min: f32,
    lo_clip: f32,
}

const APCA: ApcaConstants = ApcaConstants {
    main_trc: 2.4,
    s_rco: 0.2126729,
    s_gco: 0.7151522,
    s_bco: 0.0721750,
    norm_bg: 0.56,
    norm_txt: 0.57,
    rev_txt: 0.62,
    rev_bg: 0.65,
    blk_thrs: 0.022,
    blk_clmp: 1.414,
    scale_bow: 1.14,
    scale_wob: 1.14,
    lo_bow_offset: 0.027,
    lo_wob_offset: 0.027,
    delta_y_min: 0.0005,
    lo_clip: 0.1,
};

fn srgb_to_y(color: Hsla) -> f32 {
    let rgba = Rgba::from(color);
    let r_linear = rgba.r.powf(APCA.main_trc);
    let g_linear = rgba.g.powf(APCA.main_trc);
    let b_linear = rgba.b.powf(APCA.main_trc);
    APCA.s_rco * r_linear + APCA.s_gco * g_linear + APCA.s_bco * b_linear
}

/// APCA Lightness Contrast (`Lc`) between `text` foreground and `bg`. Sign
/// indicates polarity (positive = light text on dark bg, negative = dark
/// text on light bg). Tests and theme-load code assert on `.abs() >= 45.0`.
///
/// Visibility: `pub(crate)` (was private pre-US-007) so theme tests can
/// verify the `selection_foreground` invariant directly.
pub(crate) fn apca_contrast(text: Hsla, bg: Hsla) -> f32 {
    let text_y = srgb_to_y(text);
    let bg_y = srgb_to_y(bg);

    let text_y = if text_y > APCA.blk_thrs {
        text_y
    } else {
        text_y + (APCA.blk_thrs - text_y).powf(APCA.blk_clmp)
    };
    let bg_y = if bg_y > APCA.blk_thrs {
        bg_y
    } else {
        bg_y + (APCA.blk_thrs - bg_y).powf(APCA.blk_clmp)
    };

    if (bg_y - text_y).abs() < APCA.delta_y_min {
        return 0.0;
    }

    let (sapc, offset) = if bg_y > text_y {
        let s = (bg_y.powf(APCA.norm_bg) - text_y.powf(APCA.norm_txt)) * APCA.scale_bow;
        (s, APCA.lo_bow_offset)
    } else {
        let s = (bg_y.powf(APCA.rev_bg) - text_y.powf(APCA.rev_txt)) * APCA.scale_wob;
        (s, -APCA.lo_wob_offset)
    };

    if sapc.abs() < APCA.lo_clip {
        0.0
    } else {
        (sapc - offset) * 100.0
    }
}

/// Adjust `fg` lightness using APCA so that perceptual contrast against `bg`
/// meets `min_lc`. Returns `fg` unchanged if contrast is already sufficient.
///
/// Three-stage fallback matching Zed's approach:
/// 1. Adjust lightness only (preserves hue + saturation)
/// 2. Reduce saturation + adjust lightness
/// 3. Fall back to black or white
///
/// Visibility: `pub(crate)` (was `pub(super)` pre-US-007) so theme code can
/// derive a contrast-validated `selection_foreground` at theme-load time.
pub(crate) fn ensure_minimum_contrast(fg: Hsla, bg: Hsla, min_lc: f32) -> Hsla {
    if min_lc <= 0.0 {
        return fg;
    }
    contrast_cache_get_or_insert(fg, bg, min_lc)
}

/// Number of slots in the direct-mapped contrast cache.
///
/// A terminal grid holds tens of thousands of cells drawn from a handful of
/// distinct (foreground, background) pairs, so a small table absorbs
/// essentially all of the traffic. Direct-mapped rather than a `HashMap`
/// because a collision here costs one recomputation, which is exactly what the
/// uncached path already did, while a map would cost an allocation and
/// unbounded growth over a long session.
const CONTRAST_CACHE_SLOTS: usize = 128;

#[derive(Clone, Copy)]
struct ContrastEntry {
    key: [u32; 9],
    value: Hsla,
}

thread_local! {
    /// Thread-local because the layout pass is single-threaded per window;
    /// sharing it would trade `powf` calls for lock traffic on the hottest
    /// loop in the renderer.
    static CONTRAST_CACHE: std::cell::RefCell<[Option<ContrastEntry>; CONTRAST_CACHE_SLOTS]> =
        const { std::cell::RefCell::new([None; CONTRAST_CACHE_SLOTS]) };
}

/// Exact bit pattern of the three inputs.
///
/// Compared for equality, never interpreted, so `-0.0` versus `0.0` and NaN
/// only ever cost a miss.
fn contrast_key(fg: Hsla, bg: Hsla, min_lc: f32) -> [u32; 9] {
    [
        fg.h.to_bits(),
        fg.s.to_bits(),
        fg.l.to_bits(),
        fg.a.to_bits(),
        bg.h.to_bits(),
        bg.s.to_bits(),
        bg.l.to_bits(),
        bg.a.to_bits(),
        min_lc.to_bits(),
    ]
}

/// FNV-1a over the key, folded to a slot index.
fn contrast_slot(key: &[u32; 9]) -> usize {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for word in key {
        hash ^= u64::from(*word);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash as usize) % CONTRAST_CACHE_SLOTS
}

fn contrast_cache_get_or_insert(fg: Hsla, bg: Hsla, min_lc: f32) -> Hsla {
    let key = contrast_key(fg, bg, min_lc);
    let slot = contrast_slot(&key);
    CONTRAST_CACHE.with(|cache| {
        if let Ok(cache) = cache.try_borrow()
            && let Some(entry) = cache[slot].as_ref()
            && entry.key == key
        {
            return entry.value;
        }
        let value = compute_minimum_contrast(fg, bg, min_lc);
        if let Ok(mut cache) = cache.try_borrow_mut() {
            cache[slot] = Some(ContrastEntry { key, value });
        }
        value
    })
}

/// The uncached three-stage search. See [`ensure_minimum_contrast`].
fn compute_minimum_contrast(fg: Hsla, bg: Hsla, min_lc: f32) -> Hsla {
    if apca_contrast(fg, bg).abs() >= min_lc {
        return fg;
    }

    // Stage 1: adjust lightness only
    let adjusted = adjust_lightness_for_apca(fg, bg, min_lc);
    if apca_contrast(adjusted, bg).abs() >= min_lc {
        return adjusted;
    }

    // Stage 2: reduce saturation + adjust lightness
    for &sat_mult in &[0.8, 0.6, 0.4, 0.2, 0.0] {
        let desat = Hsla {
            s: fg.s * sat_mult,
            ..fg
        };
        let adjusted = adjust_lightness_for_apca(desat, bg, min_lc);
        if apca_contrast(adjusted, bg).abs() >= min_lc {
            return adjusted;
        }
    }

    // Stage 3: black or white
    let black = Hsla {
        h: 0.0,
        s: 0.0,
        l: 0.0,
        a: fg.a,
    };
    let white = Hsla {
        h: 0.0,
        s: 0.0,
        l: 1.0,
        a: fg.a,
    };
    if apca_contrast(white, bg).abs() > apca_contrast(black, bg).abs() {
        white
    } else {
        black
    }
}

fn adjust_lightness_for_apca(fg: Hsla, bg: Hsla, min_lc: f32) -> Hsla {
    let bg_lum = srgb_to_y(bg);
    let should_darken = bg_lum > 0.5;

    let (mut lo, mut hi) = if should_darken {
        (0.0, fg.l)
    } else {
        (fg.l, 1.0)
    };
    let mut best_l = fg.l;

    for _ in 0..20 {
        let mid = (lo + hi) * 0.5;
        let test = Hsla { l: mid, ..fg };
        let contrast = apca_contrast(test, bg).abs();

        if contrast >= min_lc {
            best_l = mid;
            if should_darken {
                lo = mid;
            } else {
                hi = mid;
            }
        } else if should_darken {
            hi = mid;
        } else {
            lo = mid;
        }

        if (contrast - min_lc).abs() < 1.0 {
            best_l = mid;
            break;
        }
    }

    Hsla { l: best_l, ..fg }
}

// ---------------------------------------------------------------------------
// Color conversion
// ---------------------------------------------------------------------------

pub(super) fn convert_color(color: Color, theme: &TerminalTheme) -> Hsla {
    match color {
        Color::Named(name) => named_color(name, theme),
        // Truecolor RGB values are kept as-is. A previous special case mapped
        // `Spec(0,0,0)` to `theme.black`, but that silently hijacked apps that
        // chose a literal `#000000` (intentional pure black) and replaced it
        // with the slightly-lighter ANSI "black" slot from the theme.
        Color::Spec(rgb) => rgb_to_hsla(rgb.r, rgb.g, rgb.b),
        Color::Indexed(i) => indexed_color(i, theme),
    }
}

fn named_color(name: NamedColor, theme: &TerminalTheme) -> Hsla {
    match name {
        NamedColor::Black => theme.black,
        NamedColor::Red => theme.red,
        NamedColor::Green => theme.green,
        NamedColor::Yellow => theme.yellow,
        NamedColor::Blue => theme.blue,
        NamedColor::Magenta => theme.magenta,
        NamedColor::Cyan => theme.cyan,
        NamedColor::White => theme.white,
        NamedColor::BrightBlack => theme.bright_black,
        NamedColor::BrightRed => theme.bright_red,
        NamedColor::BrightGreen => theme.bright_green,
        NamedColor::BrightYellow => theme.bright_yellow,
        NamedColor::BrightBlue => theme.bright_blue,
        NamedColor::BrightMagenta => theme.bright_magenta,
        NamedColor::BrightCyan => theme.bright_cyan,
        NamedColor::BrightWhite => theme.bright_white,
        NamedColor::Foreground => theme.foreground,
        NamedColor::Background => theme.ansi_background,
    }
}

/// Convert the xterm-256color indexed palette to HSLA.
fn indexed_color(i: u8, theme: &TerminalTheme) -> Hsla {
    if i < 16 {
        // Standard 16 colors - map to named
        return named_color(
            match i {
                0 => NamedColor::Black,
                1 => NamedColor::Red,
                2 => NamedColor::Green,
                3 => NamedColor::Yellow,
                4 => NamedColor::Blue,
                5 => NamedColor::Magenta,
                6 => NamedColor::Cyan,
                7 => NamedColor::White,
                8 => NamedColor::BrightBlack,
                9 => NamedColor::BrightRed,
                10 => NamedColor::BrightGreen,
                11 => NamedColor::BrightYellow,
                12 => NamedColor::BrightBlue,
                13 => NamedColor::BrightMagenta,
                14 => NamedColor::BrightCyan,
                15 => NamedColor::BrightWhite,
                _ => unreachable!(),
            },
            theme,
        );
    }

    if i < 232 {
        // 6x6x6 color cube (indices 16-231)
        let idx = i - 16;
        let r_idx = idx / 36;
        let g_idx = (idx % 36) / 6;
        let b_idx = idx % 6;
        let r = if r_idx == 0 { 0 } else { 55 + 40 * r_idx };
        let g = if g_idx == 0 { 0 } else { 55 + 40 * g_idx };
        let b = if b_idx == 0 { 0 } else { 55 + 40 * b_idx };
        return rgb_to_hsla(r, g, b);
    }

    // Grayscale ramp (indices 232-255)
    let gray = 8 + 10 * (i - 232);
    rgb_to_hsla(gray, gray, gray)
}

pub(super) fn rgb_to_hsla(r: u8, g: u8, b: u8) -> Hsla {
    Hsla::from(Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a: 1.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache is transparent or it is a rendering bug: every hit must
    /// return exactly what the uncached search would have. Sweeps enough
    /// distinct pairs to overflow `CONTRAST_CACHE_SLOTS`, so evictions and
    /// index collisions are covered too.
    #[test]
    fn the_contrast_cache_never_changes_the_answer() {
        let min_lc = 45.0;
        let mut pairs = Vec::new();
        for step in 0..(CONTRAST_CACHE_SLOTS * 3) {
            let t = step as f32 / (CONTRAST_CACHE_SLOTS * 3) as f32;
            let fg = Hsla {
                h: t,
                s: 0.6,
                l: 0.5 + t * 0.4,
                a: 1.0,
            };
            let bg = Hsla {
                h: 1.0 - t,
                s: 0.3,
                l: 0.5 - t * 0.4,
                a: 1.0,
            };
            pairs.push((fg, bg));
        }

        for (fg, bg) in &pairs {
            let cached = ensure_minimum_contrast(*fg, *bg, min_lc);
            let direct = compute_minimum_contrast(*fg, *bg, min_lc);
            assert_eq!(
                (cached.h, cached.s, cached.l, cached.a),
                (direct.h, direct.s, direct.l, direct.a),
                "cache diverged for fg={fg:?} bg={bg:?}"
            );
        }
        // Second pass: now served from the table rather than computed.
        for (fg, bg) in &pairs {
            let cached = ensure_minimum_contrast(*fg, *bg, min_lc);
            let direct = compute_minimum_contrast(*fg, *bg, min_lc);
            assert_eq!(
                (cached.h, cached.s, cached.l),
                (direct.h, direct.s, direct.l)
            );
        }
    }

    /// `min_lc` is part of the key: two call sites asking for different
    /// thresholds on the same colors must not share an entry.
    #[test]
    fn the_contrast_cache_keys_on_the_threshold_too() {
        let fg = Hsla {
            h: 0.1,
            s: 0.5,
            l: 0.52,
            a: 1.0,
        };
        let bg = Hsla {
            h: 0.1,
            s: 0.5,
            l: 0.48,
            a: 1.0,
        };
        let lenient = ensure_minimum_contrast(fg, bg, 15.0);
        let strict = ensure_minimum_contrast(fg, bg, 75.0);
        assert_ne!(
            (lenient.l, lenient.s),
            (strict.l, strict.s),
            "a stricter threshold must move the foreground further"
        );
        assert_eq!(strict.l, compute_minimum_contrast(fg, bg, 75.0).l);
    }

    fn bits(color: Hsla) -> [u32; 4] {
        [
            color.h.to_bits(),
            color.s.to_bits(),
            color.l.to_bits(),
            color.a.to_bits(),
        ]
    }

    /// Every foreground slot of every bundled theme against every background
    /// the renderer can put behind it: the theme grounds, the selection, and
    /// the sixteen ANSI colors a program can paint as a cell background.
    fn bundled_theme_pairs() -> Vec<(&'static str, Hsla, Hsla)> {
        let mut pairs = Vec::new();
        for (name, build) in crate::theme::THEMES {
            let theme = build();
            let ansi = [
                theme.black,
                theme.red,
                theme.green,
                theme.yellow,
                theme.blue,
                theme.magenta,
                theme.cyan,
                theme.white,
                theme.bright_black,
                theme.bright_red,
                theme.bright_green,
                theme.bright_yellow,
                theme.bright_blue,
                theme.bright_magenta,
                theme.bright_cyan,
                theme.bright_white,
            ];
            let mut foregrounds = vec![
                theme.foreground,
                theme.bright_foreground,
                theme.dim_foreground,
                theme.dim_black,
                theme.dim_red,
                theme.dim_green,
                theme.dim_yellow,
                theme.dim_blue,
                theme.dim_magenta,
                theme.dim_cyan,
                theme.dim_white,
            ];
            foregrounds.extend(ansi);
            let mut backgrounds = vec![theme.background, theme.ansi_background, theme.selection];
            backgrounds.extend(ansi);
            for fg in &foregrounds {
                for bg in &backgrounds {
                    pairs.push((*name, *fg, *bg));
                }
            }
        }
        pairs
    }

    /// The cache must be invisible on the pairs the renderer actually feeds
    /// it: every bundled theme, at the threshold `build_layout` passes. The
    /// sweep holds more distinct keys than the table has slots and lands
    /// several of them in one slot, so it exercises eviction and index
    /// collisions rather than just warm hits.
    #[test]
    fn the_contrast_cache_matches_the_uncached_search_across_the_bundled_themes() {
        let min_lc = crate::terminal::element::MIN_APCA_CONTRAST;
        let pairs = bundled_theme_pairs();

        let mut keys: Vec<[u32; 9]> = pairs
            .iter()
            .map(|(_, fg, bg)| contrast_key(*fg, *bg, min_lc))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        assert!(
            keys.len() > CONTRAST_CACHE_SLOTS,
            "sweep must overflow the table: {} distinct keys for {CONTRAST_CACHE_SLOTS} slots",
            keys.len()
        );
        let mut occupancy = [0usize; CONTRAST_CACHE_SLOTS];
        for key in &keys {
            occupancy[contrast_slot(key)] += 1;
        }
        let colliding_slots = occupancy.iter().filter(|count| **count > 1).count();
        assert!(
            colliding_slots > 0,
            "sweep must collide inside the table to cover the miss-on-occupied path"
        );

        // Pass 0 populates, pass 1 replays in the same order (hits where the
        // slot survived, recomputation where it was evicted), pass 2 replays
        // reversed so the eviction order differs.
        for pass in 0..3 {
            let ordered: Box<dyn Iterator<Item = &(&str, Hsla, Hsla)>> = if pass == 2 {
                Box::new(pairs.iter().rev())
            } else {
                Box::new(pairs.iter())
            };
            for (name, fg, bg) in ordered {
                let cached = ensure_minimum_contrast(*fg, *bg, min_lc);
                let direct = compute_minimum_contrast(*fg, *bg, min_lc);
                assert_eq!(
                    bits(cached),
                    bits(direct),
                    "pass {pass}: cache diverged for theme {name:?} fg={fg:?} bg={bg:?}"
                );
            }
        }
    }

    /// A theme hot-reload (`theme/watcher.rs`) swaps every color the renderer
    /// feeds this cache but never clears the table - there is no clear API.
    /// That is only sound because a hit needs the exact bit pattern of both
    /// colors and the threshold, so an entry computed for the old theme can
    /// never answer for the new one. Proven two ways rather than assumed:
    /// every ordered pair of bundled themes, priming with the old one and
    /// sweeping the new one against the uncached search; and an adversarial
    /// neighbour - the same background and threshold, a foreground one or
    /// more ULPs away in lightness, hashed to the same slot - which is the
    /// closest a reload can come to a false hit.
    #[test]
    fn a_theme_reload_never_serves_a_stale_contrast_entry() {
        let min_lc = crate::terminal::element::MIN_APCA_CONTRAST;
        let pairs = bundled_theme_pairs();
        let names: Vec<&str> = crate::theme::THEMES.iter().map(|(n, _)| *n).collect();

        for old in &names {
            for new in &names {
                if old == new {
                    continue;
                }
                for (name, fg, bg) in &pairs {
                    if name == old {
                        ensure_minimum_contrast(*fg, *bg, min_lc);
                    }
                }
                for (name, fg, bg) in &pairs {
                    if name == new {
                        assert_eq!(
                            bits(ensure_minimum_contrast(*fg, *bg, min_lc)),
                            bits(compute_minimum_contrast(*fg, *bg, min_lc)),
                            "reload {old:?} -> {new:?} served a stale entry for fg={fg:?} bg={bg:?}"
                        );
                    }
                }
            }
        }

        let theme = crate::theme::paneflow_dark();
        let (fg, bg) = (theme.foreground, theme.background);
        assert!(
            apca_contrast(fg, bg).abs() >= min_lc,
            "the default theme's foreground passes through uncorrected"
        );
        let key = contrast_key(fg, bg, min_lc);
        let slot = contrast_slot(&key);
        let neighbour = (1u32..100_000)
            .map(|ulps| Hsla {
                l: f32::from_bits(fg.l.to_bits() + ulps),
                ..fg
            })
            .find(|candidate| contrast_slot(&contrast_key(*candidate, bg, min_lc)) == slot)
            .expect("a 128-slot table has a same-slot neighbour within a few hundred ULPs");
        assert_ne!(bits(neighbour), bits(fg));
        assert!(apca_contrast(neighbour, bg).abs() >= min_lc);

        let before = ensure_minimum_contrast(fg, bg, min_lc);
        let occupant = CONTRAST_CACHE.with(|cache| cache.borrow()[slot].map(|entry| entry.key));
        assert_eq!(occupant, Some(key), "priming must land in the shared slot");

        let after = ensure_minimum_contrast(neighbour, bg, min_lc);
        assert_eq!(
            bits(after),
            bits(compute_minimum_contrast(neighbour, bg, min_lc))
        );
        assert_ne!(
            bits(after),
            bits(before),
            "the neighbour's own answer differs from the occupant's, so a stale hit would be visible"
        );
        let occupant = CONTRAST_CACHE.with(|cache| cache.borrow()[slot].map(|entry| entry.key));
        assert_eq!(
            occupant,
            Some(contrast_key(neighbour, bg, min_lc)),
            "the colliding slot was overwritten, not served"
        );
    }

    #[test]
    fn default_ground_colors_use_the_terminal_theme_slots() {
        let theme = crate::theme::paneflow_dark();

        assert_eq!(
            convert_color(Color::Named(NamedColor::Foreground), &theme),
            theme.foreground
        );
        assert_eq!(
            convert_color(Color::Named(NamedColor::Background), &theme),
            theme.ansi_background
        );
    }
}
