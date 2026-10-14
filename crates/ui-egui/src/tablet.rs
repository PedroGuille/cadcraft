//! Tablet controls around the same drawing engine and canvas used on the desktop.

use egui::{Button, Modifiers, Pos2, vec2};
use serde_json::json;

use crate::{
    CadApp,
    icons::{self, Icon},
    theme::Tokens,
};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub pen_only: bool,
    pub double_tap_cancel: bool,
    pub double_tap_ms: u32,
    pub barrel_cancel: bool,
    pub compact: bool,
    pub cache_mib: u32,
    pub recovery_seconds: u64,
}

impl Default for Preferences {
    fn default() -> Self {
        Self { pen_only: false, double_tap_cancel: true, double_tap_ms: 300, barrel_cancel: true, compact: false, cache_mib: 24, recovery_seconds: 5 }
    }
}

impl Preferences {
    pub fn validate(&mut self) {
        self.double_tap_ms = self.double_tap_ms.clamp(50, 500);
        self.cache_mib = self.cache_mib.clamp(8, 64);
        self.recovery_seconds = self.recovery_seconds.clamp(2, 60);
    }
    pub fn height(&self) -> f32 {
        if self.compact { 44.0 } else { 48.0 }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum FileAction {
    Drawings,
    Save,
    SaveAs,
    Export,
    Pdf,
    Share,
    SharePdf,
    Link,
    Project,
}

pub(crate) fn route(app: &mut CadApp, id: &str, params: &serde_json::Value) -> Option<Result<serde_json::Value, String>> {
    if id.starts_with("ui.tablet.") && (!app.tablet_mode || !params.is_null()) {
        return Some(Err("This command requires the tablet host and an interactive invocation".into()));
    }
    if !app.tablet_mode || !params.is_null() {
        return None;
    }
    let action = match id {
        "ui.tablet.drawings" => FileAction::Drawings,
        "qsave" | "save" => FileAction::Save,
        "saveas" | "ui.saveas" => FileAction::SaveAs,
        "exportpdf" => FileAction::Pdf,
        "ui.tablet.export" => FileAction::Export,
        "ui.tablet.share" => FileAction::Share,
        "ui.tablet.sharepdf" => FileAction::SharePdf,
        "ui.tablet.link" => FileAction::Link,
        "ui.tablet.project" => FileAction::Project,
        "ui.tablet.settings" => {
            app.tablet_settings_open = true;
            return Some(Ok(serde_json::Value::Null));
        }
        _ => return None,
    };
    app.tablet_file_action = Some(action);
    Some(Ok(serde_json::Value::Null))
}

/// The Android host opts in; an ordinary desktop session keeps its existing layout.
pub fn configure(app: &mut CadApp) {
    app.tablet_mode = true;
    app.ui.system_cursor = true;
    app.ui.show_toolbar = false;
    app.ui.show_toolsets = false;
    app.ui.show_palettes = false;
    app.ui.show_status_bar = false;
    app.ui.show_command_line = false;
    app.ui.history_lines = 1;
    app.session.settings.pickbox = app.session.settings.pickbox.max(12.0);
    app.session.settings.aperture = app.session.settings.aperture.max(18.0);
}

/// Delay canvas picks until a tap is known to be single. Cancelling must not first place a
/// point (which can finish a circle or add a polyline segment).
#[derive(Default)]
pub(crate) struct Taps {
    pending: Option<Tap>,
}

struct Tap {
    pos: Pos2,
    modifiers: Modifiers,
    time: f64,
    doc: u64,
    command: Option<String>,
}

pub(crate) enum TapAction {
    None,
    Pick(Pos2, Modifiers),
    Cancel,
}

impl Taps {
    pub(crate) fn update(
        &mut self,
        ctx: &egui::Context,
        doc: u64,
        command: Option<&str>,
        click: Option<(Pos2, Modifiers)>,
        double: bool,
        clear: bool,
    ) -> TapAction {
        if clear {
            self.pending = None;
            return TapAction::None;
        }
        if self.pending.as_ref().is_some_and(|p| p.doc != doc || p.command.as_deref() != command) {
            self.pending = None;
        }
        if double {
            self.pending = None;
            return TapAction::Cancel;
        }
        let time = ctx.input(|i| i.time);
        if !time.is_finite() {
            self.pending = None;
            return TapAction::None;
        }
        let delay = ctx.options(|o| o.input_options.max_double_click_delay);
        let delay = if delay.is_finite() { delay.clamp(0.05, 0.5) } else { 0.3 };
        let expired = self.pending.as_ref().is_some_and(|p| time - p.time >= delay);
        let previous = if click.is_some() || expired { self.pending.take() } else { None };
        if let Some((pos, modifiers)) = click.filter(|(pos, _)| pos.is_finite()) {
            self.pending = Some(Tap { pos, modifiers, time, doc, command: command.map(str::to_owned) });
        }
        if let Some(tap) = &self.pending {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64((delay - (time - tap.time)).clamp(0.0, 0.5)));
        }
        match previous {
            Some(tap) => TapAction::Pick(tap.pos, tap.modifiers),
            None => TapAction::None,
        }
    }
}

fn button(ui: &mut egui::Ui, text: &str) -> bool {
    ui.add(Button::new(crate::i18n::t(text)).min_size(vec2(36.0, ui.spacing().interact_size.y))).clicked()
}

pub fn chrome(app: &mut CadApp, ui: &mut egui::Ui) {
    app.tablet_prefs.validate();
    ui.ctx().options_mut(|o| o.input_options.max_double_click_delay = f64::from(app.tablet_prefs.double_tap_ms) / 1000.0);
    if app.tablet_settings_open {
        egui::Window::new(crate::i18n::t("Tablet Settings"))
            .id(egui::Id::new("tablet-settings"))
            .open(&mut app.tablet_settings_open)
            .resizable(false)
            .show(ui.ctx(), |ui| {
                ui.checkbox(&mut app.tablet_prefs.pen_only, crate::i18n::t("Draw with pen only"));
                ui.checkbox(&mut app.tablet_prefs.double_tap_cancel, crate::i18n::t("Double tap to cancel"));
                ui.checkbox(&mut app.tablet_prefs.barrel_cancel, crate::i18n::t("Pen button to cancel"));
                ui.checkbox(&mut app.tablet_prefs.compact, crate::i18n::t("Compact controls"));
                ui.add(egui::Slider::new(&mut app.tablet_prefs.double_tap_ms, 50..=500).text(crate::i18n::t("Double tap delay (ms)")));
                ui.add(egui::Slider::new(&mut app.tablet_prefs.cache_mib, 8..=64).text(crate::i18n::t("Render cache (MB)")));
                ui.add(egui::Slider::new(&mut app.tablet_prefs.recovery_seconds, 2..=60).text(crate::i18n::t("Recovery interval (s)")));
            });
    }
    let t = Tokens::get();
    egui::Panel::top("tablet-tools").frame(egui::Frame::NONE.fill(t.chrome).inner_margin(6.0)).show(ui, |ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.menu_button(crate::i18n::t("More"), |ui| {
                for (label, command) in [
                    ("Drawings", "ui.tablet.drawings"),
                    ("Tool Sets", "ui.toggle.toolsets"),
                    ("Drafting Settings", "ui.dialog.dsettings"),
                    ("Options", "options"),
                    ("Save As", "ui.saveas"),
                    ("Export file", "ui.tablet.export"),
                    ("Share drawing", "ui.tablet.share"),
                    ("Share PDF", "ui.tablet.sharepdf"),
                    ("Open linked drawing", "ui.tablet.link"),
                    ("Open project folder", "ui.tablet.project"),
                    ("Tablet Settings", "ui.tablet.settings"),
                    ("Help", "ui.dialog.about"),
                ] {
                    if button(ui, label) {
                        app.start(command);
                        ui.close();
                    }
                }
            });
            egui::ScrollArea::horizontal().id_salt("tablet-tool-scroll").auto_shrink([false, true]).show(ui, |ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    for (icon, command, label) in [
                        (Icon::New, "new", "New Drawing"),
                        (Icon::Open, "ui.open", "Open"),
                        (Icon::Save, "qsave", "Save"),
                        (Icon::Undo, "u", "Undo"),
                        (Icon::Redo, "redo", "Redo"),
                        (Icon::ZoomExtents, "zoom.extents", "Zoom Extents"),
                    ] {
                        if icons::button(ui, icon, app.tablet_prefs.height(), label, false).clicked() {
                            app.start(command);
                        }
                    }
                    ui.separator();
                    for (label, command) in [
                        ("Line", "line"),
                        ("Polyline", "pline"),
                        ("Circle", "circle"),
                        ("Rectangle", "rectang"),
                        ("Move", "move"),
                        ("Offset", "offset"),
                        ("Trim", "trim"),
                        ("Measure", "dist"),
                        ("Properties", "ui.toggle.palettes"),
                        ("Layers", "ui.dialog.layers"),
                        ("Export PDF", "exportpdf"),
                    ] {
                        if button(ui, label) {
                            app.start(command);
                        }
                    }
                });
            });
        });
    });
    egui::Panel::bottom("tablet-command").frame(egui::Frame::NONE.fill(t.chrome).inner_margin(6.0)).show(ui, |ui| {
        let prompt = app.session.current_prompt();
        ui.horizontal_wrapped(|ui| {
            if let Some(prompt) = &prompt {
                ui.label(crate::i18n::t(&prompt.message));
                for keyword in &prompt.keywords {
                    if button(ui, keyword)
                        && let Err(error) = app.session.input(cadcraft_engine::Input::Keyword(keyword.clone()))
                    {
                        app.set_status(error.to_string());
                    }
                }
            } else {
                ui.label("CADCraft");
                for (name, value) in
                    [("Grid", app.session.settings.gridmode), ("Snap", app.session.settings.snapmode), ("Ortho", app.session.settings.orthomode)]
                {
                    if ui.add(Button::new(crate::i18n::t(name)).selected(value).min_size(vec2(36.0, app.tablet_prefs.height()))).clicked() {
                        let var = match name {
                            "Grid" => "GRIDMODE",
                            "Snap" => "SNAPMODE",
                            _ => "ORTHOMODE",
                        };
                        let _ = app.run("setvar", json!({"name": var, "value": i32::from(!value)}));
                    }
                }
            }
            if let Some((status, _)) = &app.status {
                ui.label(status);
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let cancel = button(ui, "Cancel");
            let enter = button(ui, "Enter");
            let keyboard = cfg!(target_os = "android") && button(ui, "Keyboard");
            let response = ui.add_sized(
                vec2(ui.available_width().max(80.0), app.tablet_prefs.height()),
                egui::TextEdit::singleline(&mut app.cmd.buffer).hint_text(crate::i18n::t("Command")).font(crate::theme::mono()),
            );
            let submit = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if keyboard {
                app.tablet_keyboard_requested = true;
                response.surrender_focus();
            }
            if enter || submit {
                let text = std::mem::take(&mut app.cmd.buffer);
                let _ = app.cmdline(&text);
                response.surrender_focus();
            }
            if cancel {
                app.session.cancel();
                app.cmd.buffer.clear();
                response.surrender_focus();
            }
        });
    });
}
