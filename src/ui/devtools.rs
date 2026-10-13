//! Debug builds only: drive the interface through a scene and save a
//! screenshot, for checking the look without clicking by hand.
//!
//! ```sh
//! VIOLET_HOME=/tmp/violet VIOLET_SNAPSHOT=/tmp/shot.png VIOLET_SCENE=browse cargo run
//! ```
//!
//! Scenes: `welcome` (default), `browse` (connect the first profile, open
//! a table and its data), `error` (run a broken statement), `form` (the
//! connection dialog). Add `VIOLET_THEME=light` for the light theme.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use violet_core::ObjectKind;

use super::VioletApp;
use super::action::{Action, RunScope};
use super::state::{Link, NodeKey};
use super::workspace::Outcome;

pub struct Snapshot {
    path: PathBuf,
    scene: String,
    started: Instant,
    step: usize,
    requested: bool,
}

impl Snapshot {
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("VIOLET_SNAPSHOT")?.into();
        Some(Self {
            path,
            scene: std::env::var("VIOLET_SCENE").unwrap_or_else(|_| "welcome".to_owned()),
            started: Instant::now(),
            step: 0,
            requested: false,
        })
    }
}

impl VioletApp {
    pub(super) fn drive_snapshot(&mut self, ctx: &egui::Context) {
        let Some(mut snapshot) = self.snapshot.take() else {
            return;
        };
        ctx.request_repaint();
        let elapsed = snapshot.started.elapsed();

        // Save the screenshot once it arrives, then quit.
        let image = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = image {
            let [width, height] = image.size;
            let pixels: Vec<u8> = image
                .pixels
                .iter()
                .flat_map(|pixel| pixel.to_array())
                .collect();
            let saved = image::RgbaImage::from_raw(width as u32, height as u32, pixels)
                .map(|buffer| buffer.save(&snapshot.path));
            log::info!("snapshot saved: {saved:?}");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        let ready = self.advance_scene(&mut snapshot, elapsed);
        if (ready || elapsed > Duration::from_secs(20)) && !snapshot.requested {
            snapshot.requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        self.snapshot = Some(snapshot);
    }

    /// Takes the scene one step further. Returns `true` when it is ready to
    /// be captured.
    fn advance_scene(&mut self, snapshot: &mut Snapshot, elapsed: Duration) -> bool {
        let first = self.profiles.all().first().map(|profile| profile.id);
        let settle = elapsed > Duration::from_millis(1_500);
        match (snapshot.scene.as_str(), snapshot.step) {
            ("form", 0) => {
                self.actions.push(Action::NewConnection);
                snapshot.step += 1;
                false
            }
            ("browse" | "error", 0) => {
                if let Some(id) = first {
                    self.explorer.expanded.insert(NodeKey::Connection(id));
                    self.actions.push(Action::Connect(id));
                }
                snapshot.step += 1;
                false
            }
            ("browse", 1) => {
                let Some(id) = first else { return true };
                let loaded = self
                    .explorer
                    .objects
                    .contains_key(&(id, "public".to_owned()));
                if loaded && self.explorer.link(id).is_online() {
                    self.explorer.expanded.insert(NodeKey::Group(
                        id,
                        "public".into(),
                        ObjectKind::Table,
                    ));
                    self.explorer.expanded.insert(NodeKey::Group(
                        id,
                        "public".into(),
                        ObjectKind::View,
                    ));
                    let customers = NodeKey::Object(id, "public".into(), "customers".into());
                    self.explorer.expanded.insert(customers.clone());
                    self.explorer.selected = Some(customers);
                    self.actions
                        .push(Action::LoadColumns(id, "public".into(), "customers".into()));
                    self.actions.push(Action::OpenEditor {
                        connection: Some(id),
                        sql: "-- Top customers by revenue\nSELECT c.name,\n       c.country,\n       count(o.id)   AS orders,\n       sum(o.total)  AS revenue\nFROM customers c\nJOIN orders o ON o.customer_id = c.id\nWHERE o.status IS NOT NULL\nGROUP BY c.id\nORDER BY revenue DESC\nLIMIT 50;\n".into(),
                    });
                    snapshot.step += 1;
                }
                false
            }
            ("browse", 2) => {
                if let Some(tab) = self.workspace.active {
                    self.actions.push(Action::Run(tab, RunScope::Script));
                }
                snapshot.step += 1;
                false
            }
            ("error", 1) => {
                let Some(id) = first else { return true };
                if matches!(self.explorer.link(id), Link::Online(_)) {
                    self.actions.push(Action::OpenEditor {
                        connection: Some(id),
                        sql: "SELECT id, name, emial\nFROM customers\nWHERE country = 'BR';".into(),
                    });
                    snapshot.step += 1;
                }
                false
            }
            ("error", 2) => {
                if let Some(tab) = self.workspace.active {
                    self.actions.push(Action::Run(tab, RunScope::Script));
                }
                snapshot.step += 1;
                false
            }
            ("browse" | "error", _) => {
                let done = self.workspace.active().is_some_and(|tab| {
                    tab.running.is_none()
                        && matches!(
                            tab.outcome,
                            Some(Outcome::Done { .. } | Outcome::Failed { .. })
                        )
                });
                if done && snapshot.step < 10 {
                    snapshot.step = 10;
                    snapshot.started = Instant::now() - Duration::from_millis(1_000);
                }
                snapshot.step >= 10 && settle
            }
            _ => settle,
        }
    }
}
