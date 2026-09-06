//! The app-scoped theme signal every [`crate::terminal::TerminalView`]
//! observes (issue #429).
//!
//! `Pane::render` hosts its terminal behind `Entity::cached`, so a view that
//! is not notified keeps last frame's glyphs. `invalidate_theme_cache` only
//! touches the application entity, which is why a theme switch also has to
//! publish the new generation through this entity: the theme picker and
//! `process_config_changes` call [`publish_theme_generation`], and each view's
//! `cx.observe` turns that into its own `cx.notify()`.

use gpui::{App, AppContext, Entity, Global};

#[derive(Default)]
pub struct ThemeSignal {
    generation: u64,
}

impl ThemeSignal {
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

pub struct ThemeSignalGlobal(pub Entity<ThemeSignal>);

impl Global for ThemeSignalGlobal {}

pub fn install_theme_signal(cx: &mut App) {
    if cx.has_global::<ThemeSignalGlobal>() {
        return;
    }
    let signal = cx.new(|_| ThemeSignal {
        generation: super::theme_generation(),
    });
    cx.set_global(ThemeSignalGlobal(signal));
}

pub fn theme_signal(cx: &App) -> Option<Entity<ThemeSignal>> {
    cx.try_global::<ThemeSignalGlobal>()
        .map(|global| global.0.clone())
}

pub fn publish_theme_generation(cx: &mut App) {
    let generation = super::theme_generation();
    let Some(signal) = theme_signal(cx) else {
        return;
    };
    signal.update(cx, |signal, cx| {
        if signal.generation == generation {
            return;
        }
        signal.generation = generation;
        cx.notify();
    });
}
