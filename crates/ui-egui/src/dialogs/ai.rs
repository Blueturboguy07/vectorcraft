//! The AI dialogs: the first-launch setup (publik API or your own key) and Object › AI › Generate
//! Image. `crate::ai` draws them (desktop only); the web build has no AI and says so.
//!
//! They have no OK button: their own buttons send a request, and only an explicit click does.
//! Close or Escape closes them; OK from the control channel (`ui.dialog.confirm`) only closes them
//! too, and sends nothing.

use serde_json::{Value, json};

use super::DialogSpec;
use crate::VectorcraftApp;
use crate::state::Dialog;

/// The first-launch dialog (and Set up AI… from Generate Image).
pub const SETUP: &str = "aiSetup";
/// Object › AI › Generate Image….
pub const GENERATE_IMAGE: &str = "aiGenerateImage";

pub(super) const SETUP_SPEC: DialogSpec =
    DialogSpec { heading: |_| tl!("AI").into(), body: setup, confirm, ok: None, min_width: 460.0, max_width: Some(560.0), ..DialogSpec::FORM };

pub(super) const GENERATE_SPEC: DialogSpec = DialogSpec {
    heading: |_| tl!("Generate Image…").trim_end_matches('…').into(),
    body: generate,
    confirm,
    ok: None,
    min_width: 460.0,
    max_width: Some(560.0),
    ..DialogSpec::FORM
};

/// Object › AI › Generate Image… (`ui.aiGenerateImage`): open the dialog. Nothing is sent here.
pub fn open(app: &mut VectorcraftApp) -> Result<Value, String> {
    #[cfg(not(target_arch = "wasm32"))]
    crate::ai::opening(app);
    app.ui.dialog = Some(Dialog::new(GENERATE_IMAGE, json!({})));
    Ok(Value::Null)
}

#[cfg(not(target_arch = "wasm32"))]
fn setup(app: &mut VectorcraftApp, ui: &mut egui::Ui, _d: &mut Dialog) -> bool {
    crate::ai::setup(app, ui)
}

#[cfg(not(target_arch = "wasm32"))]
fn generate(app: &mut VectorcraftApp, ui: &mut egui::Ui, d: &mut Dialog) -> bool {
    crate::ai::generate_image(app, ui, d)
}

#[cfg(target_arch = "wasm32")]
fn setup(_app: &mut VectorcraftApp, ui: &mut egui::Ui, _d: &mut Dialog) -> bool {
    ui.label(tl!("Needs the desktop app"));
    false
}

#[cfg(target_arch = "wasm32")]
fn generate(_app: &mut VectorcraftApp, ui: &mut egui::Ui, _d: &mut Dialog) -> bool {
    ui.label(tl!("Needs the desktop app"));
    false
}

/// OK from the control channel: closes the dialog and sends nothing. Closing the first-launch
/// dialog counts as Later.
fn confirm(app: &mut VectorcraftApp, d: &Dialog) -> Result<Value, String> {
    app.ui.dialog = None;
    if d.kind == SETUP {
        dismissed(app);
    }
    Ok(Value::Null)
}

/// The first-launch dialog closed without its own Later (Close, Escape, `ui.dialog.cancel`): it
/// counts as Later, so it shows once.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn dismissed(app: &mut VectorcraftApp) {
    crate::ai::dismissed(app);
}

#[cfg(target_arch = "wasm32")]
pub(super) fn dismissed(_app: &mut VectorcraftApp) {}
