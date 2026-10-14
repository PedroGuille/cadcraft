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
    if id == "ui.tablet.pan" && app.tablet_mode {
        let active = params.get("on").and_then(serde_json::Value::as_bool).unwrap_or(!app.canvas.tablet_pan);
        app.canvas.set_tablet_pan(active);
        if active {
            app.session.pending_window = None;
        }
        return Some(Ok(json!({"on": active})));
    }
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
    labeled_button(ui, text, crate::i18n::t(text))
}

fn labeled_button(ui: &mut egui::Ui, text: &str, caption: &str) -> bool {
    let response = ui.add(Button::new(caption).min_size(vec2(36.0, ui.spacing().interact_size.y))).on_hover_text(crate::i18n::t(text));
    crate::control::record_widget(ui, &response, text, "button", None);
    response.clicked()
}

fn toggle(ui: &mut egui::Ui, app: &mut CadApp, name: &str, var: &str, value: bool) {
    let response = ui.add(Button::new(crate::i18n::t(name)).selected(value).min_size(vec2(36.0, ui.spacing().interact_size.y)));
    crate::control::record_widget(ui, &response, name, "button", Some(value));
    if response.clicked() {
        let _ = app.run("setvar", json!({"name": var, "value": i32::from(!value)}));
    }
}

fn button_width(ui: &egui::Ui, text: &str) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let width = ui.fonts_mut(|fonts| fonts.layout_no_wrap(crate::i18n::t(text).into(), font, egui::Color32::PLACEHOLDER).size().x);
    (width + 2.0 * ui.spacing().button_padding.x).max(36.0)
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
    egui::Panel::top("tablet-tools").frame(egui::Frame::NONE.fill(t.chrome).inner_margin(egui::Margin::symmetric(6, 0))).show(ui, |ui| {
        let height = if app.tablet_prefs.compact { 28.0 } else { 32.0 };
        ui.spacing_mut().interact_size.y = height;
        ui.spacing_mut().button_padding.y = 0.0;
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
            egui::ScrollArea::horizontal()
                .id_salt("tablet-tool-scroll")
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        for (icon, command, label) in [
                            (Icon::New, "new", "New Drawing"),
                            (Icon::Open, "ui.open", "Open"),
                            (Icon::Save, "qsave", "Save"),
                            (Icon::Undo, "u", "Undo"),
                            (Icon::Redo, "redo", "Redo"),
                            (Icon::ZoomExtents, "zoom.extents", "Zoom Extents"),
                        ] {
                            if icons::button(ui, icon, height, label, false).clicked() {
                                app.start(command);
                            }
                        }
                        ui.separator();
                        if icons::button(ui, Icon::Pan, height, "Pan", app.canvas.tablet_pan).clicked() {
                            app.start("ui.tablet.pan");
                        }
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
                            ("Share PDF", "ui.tablet.sharepdf"),
                            ("Drafting Settings", "ui.dialog.dsettings"),
                            ("Options", "options"),
                            ("Tablet Settings", "ui.tablet.settings"),
                        ] {
                            if labeled_button(ui, label, crate::i18n::tablet(label)) {
                                app.start(command);
                            }
                        }
                    });
                });
        });
    });
    egui::Panel::bottom("tablet-command").frame(egui::Frame::NONE.fill(t.chrome).inner_margin(egui::Margin::symmetric(6, 0))).show(ui, |ui| {
        ui.spacing_mut().button_padding.x *= 0.5;
        let prompt = app.session.current_prompt();
        let mut controls = button_width(ui, "Enter") + button_width(ui, "Cancel") + 2.0 * ui.spacing().item_spacing.x;
        if cfg!(target_os = "android") {
            controls += button_width(ui, "Keyboard") + ui.spacing().item_spacing.x;
        }
        let width = ((ui.available_width() - controls).max(80.0) * 0.3).max(80.0);
        egui::ScrollArea::horizontal()
            .id_salt("tablet-command-scroll")
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.label("CADCraft");
                    toggle(ui, app, "Grid", "GRIDMODE", app.session.settings.gridmode);
                    toggle(ui, app, "Snap", "SNAPMODE", app.session.settings.snapmode);
                    let hint = prompt.as_ref().map_or("Command", |p| p.message.as_str());
                    let response = ui
                        .add_sized(
                            vec2(width, app.tablet_prefs.height()),
                            egui::TextEdit::singleline(&mut app.cmd.buffer)
                                .hint_text(crate::i18n::t(hint))
                                .font(crate::theme::mono())
                                .margin(egui::Margin::symmetric(2, 2)),
                        )
                        .on_hover_text(crate::i18n::t(hint));
                    crate::control::record_widget(ui, &response, "Command", "textbox", None);
                    let submit = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let keyboard = cfg!(target_os = "android") && button(ui, "Keyboard");
                    let enter = button(ui, "Enter");
                    let cancel = button(ui, "Cancel");
                    toggle(ui, app, "Ortho", "ORTHOMODE", app.session.settings.orthomode);
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
                        app.canvas.set_tablet_pan(false);
                        app.session.cancel();
                        app.cmd.buffer.clear();
                        response.surrender_focus();
                    }
                    if let Some(prompt) = &prompt {
                        for keyword in &prompt.keywords {
                            if button(ui, keyword)
                                && let Err(error) = app.session.input(cadcraft_engine::Input::Keyword(keyword.clone()))
                            {
                                app.set_status(error.to_string());
                            }
                        }
                    }
                    if let Some((status, _)) = &app.status {
                        ui.add(egui::Label::new(status).truncate()).on_hover_text(status);
                    }
                });
            });
    });
}
