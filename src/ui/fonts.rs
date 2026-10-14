//! Fonts for the scripts the interface face does not draw (Han, Hangul,
//! Arabic, Thai...) come from the system and are large: together about
//! 50 MB, Hangul alone 27 MB. Most sessions never show such text, so
//! Violet starts without them and reads them, on a background thread, the
//! first time text arrives that the bundled faces cannot draw.

use std::hash::{Hash, Hasher};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use egui::{FontDefinitions, FontFamily, FontId};
use violet_app::Event;
use violet_core::{DbError, ResultSet};

use super::theme;

#[derive(Default)]
enum State {
    #[default]
    Bundled,
    Loading(Receiver<FontDefinitions>),
    Complete,
}

/// Whether the system's fallback fonts are in, and what has been checked.
#[derive(Default)]
pub struct Fallbacks {
    state: State,
    /// The last editor text checked, by hash, so typing re-checks it once.
    editor: u64,
}

impl Fallbacks {
    /// Loads the fallbacks if `text` has a character the installed fonts
    /// cannot draw.
    pub fn cover(&mut self, ctx: &egui::Context, text: &str) {
        if !matches!(self.state, State::Bundled) || text.is_ascii() {
            return;
        }
        let font = FontId::new(13.0, FontFamily::Proportional);
        let missing = ctx.fonts_mut(|fonts| {
            text.chars()
                .any(|c| !c.is_ascii() && !c.is_whitespace() && !fonts.has_glyph(&font, c))
        });
        if missing {
            self.load(ctx);
        }
    }

    /// [`Self::cover`] for the editor's text, checked again only when it
    /// changes.
    pub fn cover_editor(&mut self, ctx: &egui::Context, text: &str) {
        if !matches!(self.state, State::Bundled) || text.is_ascii() {
            return;
        }
        let mut hasher = std::hash::DefaultHasher::new();
        text.hash(&mut hasher);
        let hash = hasher.finish();
        if hash != self.editor {
            self.editor = hash;
            self.cover(ctx, text);
        }
    }

    /// Covers the text an event brings: results, names, server messages.
    pub fn cover_event(&mut self, ctx: &egui::Context, event: &Event) {
        if !matches!(self.state, State::Bundled) {
            return;
        }
        match event {
            Event::Connected { server, .. }
            | Event::TestFinished {
                result: Ok(server), ..
            } => {
                self.cover(ctx, &server.version);
            }
            Event::ConnectFailed { error, .. }
            | Event::TestFinished {
                result: Err(error), ..
            }
            | Event::Counted {
                result: Err(error), ..
            } => self.cover_error(ctx, error),
            Event::Schemas { result, .. } => match result {
                Ok(schemas) => schemas.iter().for_each(|name| self.cover(ctx, name)),
                Err(error) => self.cover_error(ctx, error),
            },
            Event::Objects { result, .. } => match result {
                Ok(objects) => objects
                    .iter()
                    .for_each(|object| self.cover(ctx, &object.name)),
                Err(error) => self.cover_error(ctx, error),
            },
            Event::Columns { result, .. } => match result {
                Ok(columns) => columns
                    .iter()
                    .for_each(|column| self.cover(ctx, &column.name)),
                Err(error) => self.cover_error(ctx, error),
            },
            Event::QueryFinished { result, .. } => match result {
                Ok(output) => output.sets.iter().for_each(|set| self.cover_set(ctx, set)),
                Err(error) => self.cover_error(ctx, error),
            },
            _ => {}
        }
    }

    fn cover_error(&mut self, ctx: &egui::Context, error: &DbError) {
        self.cover(ctx, &error.message);
        for text in [&error.detail, &error.hint].into_iter().flatten() {
            self.cover(ctx, text);
        }
    }

    fn cover_set(&mut self, ctx: &egui::Context, set: &ResultSet) {
        for column in &set.columns {
            self.cover(ctx, &column.name);
        }
        for row in set.rows.iter() {
            for text in row.iter().flatten() {
                self.cover(ctx, text);
                if !matches!(self.state, State::Bundled) {
                    return;
                }
            }
        }
    }

    fn load(&mut self, ctx: &egui::Context) {
        log::info!("text needs the system's fallback fonts; loading them");
        let (sender, receiver) = mpsc::channel();
        let repaint = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("violet-fonts".into())
            .spawn(move || {
                let _ = sender.send(theme::font_definitions(true));
                repaint.request_repaint();
            });
        self.state = match spawned {
            Ok(_) => State::Loading(receiver),
            Err(error) => {
                log::warn!("could not load the fallback fonts: {error}");
                State::Complete
            }
        };
    }

    /// Installs the fallbacks once they are read. Call every frame.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let State::Loading(receiver) = &self.state else {
            return;
        };
        match receiver.try_recv() {
            Ok(definitions) => {
                log::info!("system fallback fonts installed");
                ctx.set_fonts(definitions);
                self.state = State::Complete;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.state = State::Complete,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        ctx.set_fonts(theme::font_definitions(false));
        frame(&ctx);
        ctx
    }

    /// Runs a frame; fonts exist, or change, from one on.
    fn frame(ctx: &egui::Context) {
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {});
        output.textures_delta.clear();
    }

    #[test]
    fn text_the_bundled_fonts_draw_loads_nothing() {
        let ctx = context();
        let mut fallbacks = Fallbacks::default();
        for text in ["SELECT 1", "Olá, ação — “aspas” · €", "Привет", "Γειά"]
        {
            fallbacks.cover(&ctx, text);
        }
        assert!(matches!(fallbacks.state, State::Bundled));
    }

    #[test]
    fn other_scripts_load_the_fallbacks_once() {
        let ctx = context();
        let mut fallbacks = Fallbacks::default();
        fallbacks.cover(&ctx, "中文 한국어");
        assert!(matches!(fallbacks.state, State::Loading(_)));

        let started = std::time::Instant::now();
        while !matches!(fallbacks.state, State::Complete) {
            assert!(started.elapsed().as_secs() < 30, "the fonts never arrived");
            fallbacks.poll(&ctx);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        frame(&ctx);
        #[cfg(target_os = "macos")]
        assert!(ctx.fonts_mut(|fonts| {
            fonts.has_glyphs(&FontId::new(13.0, FontFamily::Proportional), "中文 한국어")
        }));
    }
}
