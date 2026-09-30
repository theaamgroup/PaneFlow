//! `paneflow self-test glyphs` (issue #1116): rasterize known text through
//! this binary's own platform text system and exit non-zero when the glyphs
//! come back empty.
//!
//! The check targets the "boxes drawn, no glyphs" regression (v0.2.12,
//! c3e2331). Without `gpui_platform`'s `font-kit` feature, `gpui_macos` swaps
//! its Core Text text system for GPUI's `NoopTextSystem`: fonts still
//! "register", `font_id` still resolves, the window still opens, and every
//! glyph bitmap is empty. A font-resolution log line cannot tell the two
//! builds apart; a rasterized bitmap can. The render smoke lane in
//! `.github/workflows/run_tests.yml` runs this against the bundled release
//! binary as its hard gate.
//!
//! Offline: it builds a headless platform in this process, never connects
//! to a running instance, and never opens a window.

use clap::Subcommand;
use gpui::{PlatformTextSystem, RenderGlyphParams, font, point, px};

use super::{EXIT_OK, EXIT_RUNTIME};
use crate::assets::Assets;
use crate::terminal::element::{DEFAULT_FONT_SIZE, EMBEDDED_MONO_FAMILY, EMBEDDED_SANS_FAMILY};

/// The checks `paneflow self-test` runs.
#[derive(Subcommand, Debug)]
pub(super) enum SelfTestCheck {
    /// Rasterize sample text in the bundled terminal and UI fonts and fail
    /// when any glyph bitmap is empty.
    Glyphs,
}

/// Characters rasterized per family. Letters and a digit only: each has ink
/// in both bundled faces, so an empty bitmap is always a failure.
const PROBE_TEXT: &str = "PaneFlow0";

/// Retina scale, matching the displays PaneFlow ships to.
const PROBE_SCALE_FACTOR: f32 = 2.0;

/// The bundled terminal face (the default `font_family`) and the bundled UI
/// face, the two families every PaneFlow window paints with.
const PROBE_FAMILIES: [&str; 2] = [EMBEDDED_MONO_FAMILY, EMBEDDED_SANS_FAMILY];

/// One rasterized glyph: its bitmap size and how many bytes carry coverage.
#[derive(Debug)]
struct GlyphProbe {
    family: &'static str,
    ch: char,
    width: i32,
    height: i32,
    inked: usize,
}

pub(super) fn run(check: SelfTestCheck) -> i32 {
    match check {
        SelfTestCheck::Glyphs => run_glyphs(),
    }
}

fn run_glyphs() -> i32 {
    let platform = gpui_platform::current_platform(true);
    let text_system = platform.text_system();
    let fonts = match Assets.embedded_font_data() {
        Ok(fonts) if !fonts.is_empty() => fonts,
        Ok(_) => {
            eprintln!("paneflow self-test glyphs: FAILED: no embedded fonts to register");
            return EXIT_RUNTIME;
        }
        Err(e) => {
            eprintln!("paneflow self-test glyphs: FAILED: reading embedded fonts: {e:#}");
            return EXIT_RUNTIME;
        }
    };
    if let Err(e) = text_system.add_fonts(fonts) {
        eprintln!("paneflow self-test glyphs: FAILED: registering embedded fonts: {e:#}");
        return EXIT_RUNTIME;
    }
    let verdict = probe_glyphs(text_system.as_ref(), &PROBE_FAMILIES).and_then(|p| verdict(&p));
    match verdict {
        Ok(summary) => {
            println!("{summary}");
            EXIT_OK
        }
        Err(message) => {
            eprintln!("{message}");
            EXIT_RUNTIME
        }
    }
}

/// Rasterize every [`PROBE_TEXT`] character in each family the way GPUI's
/// sprite atlas does: raster bounds first, then the bitmap. A zero-area
/// bound is recorded as an empty glyph rather than rasterized, since the
/// atlas skips painting those.
fn probe_glyphs(
    text_system: &dyn PlatformTextSystem,
    families: &[&'static str],
) -> Result<Vec<GlyphProbe>, String> {
    let mut probes = Vec::new();
    for &family in families {
        let font_id = text_system
            .font_id(&font(family))
            .map_err(|e| format!("paneflow self-test glyphs: FAILED: font '{family}': {e:#}"))?;
        for ch in PROBE_TEXT.chars() {
            let glyph_id = text_system.glyph_for_char(font_id, ch).ok_or_else(|| {
                format!("paneflow self-test glyphs: FAILED: '{family}' has no glyph for {ch:?}")
            })?;
            let params = RenderGlyphParams {
                font_id,
                glyph_id,
                font_size: px(DEFAULT_FONT_SIZE),
                subpixel_variant: point(0, 0),
                scale_factor: PROBE_SCALE_FACTOR,
                is_emoji: false,
                subpixel_rendering: false,
                dilation: 0,
            };
            let bounds = text_system.glyph_raster_bounds(&params).map_err(|e| {
                format!("paneflow self-test glyphs: FAILED: '{family}' {ch:?} raster bounds: {e:#}")
            })?;
            let (size, bytes) = if bounds.size.width.0 > 0 && bounds.size.height.0 > 0 {
                text_system.rasterize_glyph(&params, bounds).map_err(|e| {
                    format!("paneflow self-test glyphs: FAILED: '{family}' {ch:?} rasterize: {e:#}")
                })?
            } else {
                (bounds.size, Vec::new())
            };
            probes.push(GlyphProbe {
                family,
                ch,
                width: size.width.0,
                height: size.height.0,
                inked: bytes.iter().filter(|&&b| b != 0).count(),
            });
        }
    }
    Ok(probes)
}

/// `Ok(summary)` when every probe has a non-empty bitmap with ink in it,
/// otherwise `Err` naming each empty glyph.
fn verdict(probes: &[GlyphProbe]) -> Result<String, String> {
    if probes.is_empty() {
        return Err("paneflow self-test glyphs: FAILED: no glyphs were probed".to_string());
    }
    let empty: Vec<String> = probes
        .iter()
        .filter(|p| p.width <= 0 || p.height <= 0 || p.inked == 0)
        .map(|p| {
            format!(
                "  '{}' {:?}: {}x{} bitmap, {} inked bytes",
                p.family, p.ch, p.width, p.height, p.inked
            )
        })
        .collect();
    if !empty.is_empty() {
        return Err(format!(
            "paneflow self-test glyphs: FAILED: {} of {} glyphs rasterized empty. \
             Text in this build paints as blank cells; check that gpui_platform \
             carries the `font-kit` feature (src-app/Cargo.toml).\n{}",
            empty.len(),
            probes.len(),
            empty.join("\n")
        ));
    }
    let min_inked = probes.iter().map(|p| p.inked).min().unwrap_or(0);
    let mut families: Vec<&str> = probes.iter().map(|p| p.family).collect();
    families.dedup();
    Ok(format!(
        "paneflow self-test glyphs: ok ({} glyphs in {}; fewest inked bytes {min_inked})",
        probes.len(),
        families
            .iter()
            .map(|f| format!("'{f}'"))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(inked: usize, width: i32) -> GlyphProbe {
        GlyphProbe {
            family: EMBEDDED_MONO_FAMILY,
            ch: 'P',
            width,
            height: 20,
            inked,
        }
    }

    /// The text system a build without `font-kit` gets. Every glyph must
    /// come back as a failure, which is the whole point of the check.
    #[test]
    fn noop_text_system_fails_the_glyph_self_test() {
        let noop = gpui::NoopTextSystem::new();
        let probes = probe_glyphs(&noop, &PROBE_FAMILIES).expect("noop resolves every font");
        assert_eq!(
            probes.len(),
            PROBE_TEXT.chars().count() * PROBE_FAMILIES.len()
        );
        let err = verdict(&probes).expect_err("empty glyphs must fail");
        assert!(err.contains("18 of 18 glyphs rasterized empty"), "{err}");
    }

    #[test]
    fn verdict_fails_on_any_empty_glyph() {
        assert!(verdict(&[probe(40, 12), probe(0, 12)]).is_err());
        assert!(verdict(&[probe(40, 12), probe(40, 0)]).is_err());
        assert!(verdict(&[]).is_err());
        let ok = verdict(&[probe(40, 12), probe(7, 12)]).expect("inked glyphs pass");
        assert!(ok.contains("ok (2 glyphs"), "{ok}");
        assert!(ok.contains("fewest inked bytes 7"), "{ok}");
    }
}
