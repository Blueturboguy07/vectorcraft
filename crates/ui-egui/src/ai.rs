//! AI through publik API or the person's own key, desktop only. The setup flow, the credential
//! file and the HTTP calls live in the `craft-publik` crate, which the Craft apps share. This module
//! is VectorCraft's thin host: the first-launch dialog, Preferences › AI, and Object › AI ›
//! Generate Image.
//!
//! Nothing is sent, and nothing is charged, until the person clicks Generate image or Generate and
//! trace in the Generate Image dialog. The image joins the document through the app's own
//! commands: `file.place` puts it on the artboard in view as one embedded image (one undo step),
//! and Generate and trace then runs `imageTrace.make` with the Default preset, as Object › Image
//! Trace › Make does. A generated image is paid for, so it is never dropped: when it can't be
//! placed it waits behind the dialog's Place image button.
//!
//! The text of these dialogs is English, like the shared setup flow's; the menu items and the
//! Preferences category that lead to them are translated.

use std::sync::mpsc::{Receiver, TryRecvError};

pub use craft_publik::AppInfo;
use craft_publik::client::{self, Fail, Reply};
use craft_publik::ui::{Action, AiState, spawn};
use craft_publik::{Line, Provider, WHY_IT_COSTS};
use egui::{RichText, Ui};
use serde_json::json;
use vectorcraft_geom::{Point, Rect};

use crate::VectorcraftApp;
use crate::dialogs::ai::{GENERATE_IMAGE, SETUP};
use crate::state::{Dialog, View};

/// What the AI dialogs and Preferences › AI say in a run without preferences.
const OFF: &str = "AI is off in this session: VectorCraft runs without preferences (VECTORCRAFT_NO_PREFS).";
const NEEDS_SETUP: &str = "Generate Image needs AI. Set it up first: publik API (no key to paste) or your own key.";
const NO_DOCUMENT: &str = "Open or create a document first. The image goes on its artboard.";
const STOPPED: &str = "The request stopped unexpectedly. Try again.";
const PLACED: &str = "The image is on the artboard. Edit › Undo removes it.";
const TRACED: &str = "The image is on the artboard, traced with Image Trace (Default preset). Edit › Undo removes the trace, then the image.";
/// The part of the canvas in view that a generated image may fill: a margin stays round it.
const FIT: f64 = 0.8;
/// The side of the box an image fills when the view isn't known (pt).
const FALLBACK_SIDE: f64 = 512.0;
/// The placed image's name (the Layers and Links panels show it).
const IMAGE_NAME: &str = "generated-image.png";
/// The Image Trace preset of Generate and trace (Object › Image Trace › Make uses it too).
const TRACE_PRESET: &str = "Default";

pub struct Ai {
    pub state: AiState,
    frames: u32,
    first_run_checked: bool,
    /// The first-launch dialog is due; it opens when no other dialog shows.
    setup_due: bool,
    /// Preferences › AI shows the setup flow in place of the status.
    inline_setup: bool,
    /// The Generate Image dialog's prompt and shape, kept while the app runs.
    pub prompt: String,
    shape: Shape,
    /// The request in flight, and where its image goes.
    job: Option<(Receiver<Result<Reply, Fail>>, Target)>,
    /// Generated images that are not in a document yet, each with its Trace choice. They were paid
    /// for, so they are never dropped: they wait behind Place image.
    waiting: Vec<(Vec<u8>, bool)>,
    /// The last outcome and the last failure, shown in the dialog.
    note: Option<String>,
    error: Option<String>,
    /// Open Generate Image again once no other dialog shows: a request ended while it was closed,
    /// and its outcome needs the person (a failure, or an image that waits).
    reopen: bool,
}

/// Where a generated image goes, chosen at the click.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Target {
    /// The document ([`vectorcraft_engine::DocState::uid`]).
    doc: u64,
    /// The box the image is fitted into, centred, in document points.
    rect: Rect,
    /// Trace the image with Image Trace once it is placed (Generate and trace).
    trace: bool,
}

/// The image's proportions: the sizes the image line accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Square,
    Landscape,
    Portrait,
}

impl Shape {
    const ALL: [Shape; 3] = [Shape::Square, Shape::Landscape, Shape::Portrait];

    /// The shapes a provider can draw: publik API draws 1024x1024 only; a person's own key has all three.
    fn offered(publik: bool) -> &'static [Shape] {
        if publik { &Self::ALL[..1] } else { &Self::ALL[..] }
    }

    fn size(self) -> &'static str {
        match self {
            Shape::Square => "1024x1024",
            Shape::Landscape => "1536x1024",
            Shape::Portrait => "1024x1536",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Shape::Square => "Square",
            Shape::Landscape => "Landscape",
            Shape::Portrait => "Portrait",
        }
    }

    /// The shape nearest a box's proportions.
    fn for_size(width: f64, height: f64) -> Shape {
        if !(width.is_finite() && height.is_finite()) || width <= 0.0 || height <= 0.0 {
            return Shape::Square;
        }
        let ratio = width / height;
        if ratio > 1.2 {
            Shape::Landscape
        } else if ratio < 1.0 / 1.2 {
            Shape::Portrait
        } else {
            Shape::Square
        }
    }
}

impl Ai {
    pub fn new(app: AppInfo) -> Self {
        Ai {
            state: AiState::new(app),
            frames: 0,
            first_run_checked: false,
            setup_due: false,
            inline_setup: false,
            prompt: String::new(),
            shape: Shape::Square,
            job: None,
            waiting: vec![],
            note: None,
            error: None,
            reopen: false,
        }
    }

    fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// The image of the request in flight, once it arrived. A failure goes to `error`, or to the
    /// notice bar (one message, one link) when it is a 402 or a key problem, never to both.
    fn poll(&mut self) -> Option<(Vec<u8>, Target)> {
        let (rx, target) = self.job.take()?;
        match rx.try_recv() {
            Ok(Ok(r)) => {
                self.state.note_reply(&r);
                self.error = None;
                Some((r.bytes, target))
            }
            Ok(Err(f)) => {
                self.state.note_fail(&f);
                if !(f.status == 402 || f.status == 401 || f.reprovision) {
                    self.error = Some(f.message);
                }
                None
            }
            Err(TryRecvError::Empty) => {
                self.job = Some((rx, target));
                None
            }
            Err(TryRecvError::Disconnected) => {
                self.error = Some(STOPPED.into());
                None
            }
        }
    }
}

/// `PUBLIK_SELF_TEST=1`: provision for real and print one line (publikhq.com/skills/setup.md).
pub fn self_test(app: &AppInfo) -> String {
    craft_publik::self_test(app)
}

/// Once per frame, before drawing: finish background work, place an image that arrived, and open
/// the first-launch dialog (once the window is up) or Generate Image again when they are due.
pub fn frame(app: &mut VectorcraftApp) {
    // Not in the middle of a drag: the image waits in its channel until the tool is done.
    let tool_busy = app.session.tool_busy();
    let showing = showing_generate(app);
    let Some(ai) = app.ai.as_mut() else { return };
    ai.state.poll();
    let was_busy = ai.busy();
    let arrived = if tool_busy { None } else { ai.poll() };
    if was_busy && !ai.busy() && arrived.is_none() && !showing {
        // The request failed while its dialog was closed: show why.
        ai.reopen = true;
    }
    ai.frames = ai.frames.saturating_add(1);
    // The first launch asks once the window is up (a few frames in). Later ends that for good.
    if !ai.first_run_checked && ai.frames > 3 {
        ai.first_run_checked = true;
        ai.setup_due = ai.state.wants_first_run() || ai.state.owes_first_card();
    }
    // An image that could not be placed while its dialog was closed: show it waiting.
    if let Some((bytes, target)) = arrived
        && !place_generated(app, bytes, Some(target), target.trace)
        && !showing_generate(app)
        && let Some(ai) = app.ai.as_mut()
    {
        ai.reopen = true;
    }
    let Some(ai) = app.ai.as_mut() else { return };
    if app.ui.dialog.is_some() {
        return;
    }
    if ai.setup_due {
        ai.setup_due = false;
        ai.state.begin();
        app.ui.dialog = Some(Dialog::new(SETUP, json!({})));
    } else if ai.reopen {
        ai.reopen = false;
        app.ui.dialog = Some(Dialog::new(GENERATE_IMAGE, json!({})));
    }
}

/// Is the Generate Image dialog open?
fn showing_generate(app: &VectorcraftApp) -> bool {
    app.ui.dialog.as_ref().is_some_and(|d| d.kind == GENERATE_IMAGE)
}

/// Object › AI › Generate Image… is opening: the shape starts as the box in view's, unless a request
/// is in flight. Nothing is sent here.
pub fn opening(app: &mut VectorcraftApp) {
    let shape = target_rect(app).map(|(_, r)| Shape::for_size(r.width(), r.height()));
    if let Some(ai) = app.ai.as_mut()
        && !ai.busy()
    {
        ai.shape = shape.unwrap_or(Shape::Square);
        ai.note = None;
        ai.error = None;
    }
}

/// The first-launch dialog closed (Close, Escape, or the control channel's OK): it counts as
/// Later, so it shows once.
pub fn dismissed(app: &mut VectorcraftApp) {
    let Some(ai) = app.ai.as_mut() else { return };
    if !ai.state.settings.setup_done && !ai.state.busy() {
        ai.state.settings.setup_done = true;
        let path = ai.state.app.settings_path();
        if let Err(e) = ai.state.settings.save(path.as_deref()) {
            log::warn!("AI settings: {e}");
        }
    }
}

fn open_url(app: &mut VectorcraftApp, action: Action) -> bool {
    match action {
        Action::None => false,
        Action::OpenUrl(url) => {
            app.open_url(&url);
            false
        }
        Action::Close => true,
    }
}

fn error_line(ui: &mut Ui, error: Option<&str>) {
    if let Some(e) = error {
        ui.add_space(4.0);
        ui.colored_label(ui.visuals().error_fg_color, e);
    }
}

// ---- the setup flow (first launch) ---------------------------------------------------------------

/// The first-launch dialog's body: the shared setup flow. `true` closes the dialog (Later, Done).
pub fn setup(app: &mut VectorcraftApp, ui: &mut Ui) -> bool {
    let Some(ai) = app.ai.as_mut() else {
        ui.label(OFF);
        return false;
    };
    let action = ai.state.setup_ui(ui);
    open_url(app, action)
}

// ---- Generate Image ------------------------------------------------------------------------------

/// Where an image generated now goes in the active document: `(document uid, box)`. The box is
/// centred in the view and fills most of it, no bigger than the artboard in view and moved onto it
/// when the view shows somewhere else. None without a document.
fn target_rect(app: &VectorcraftApp) -> Option<(u64, Rect)> {
    let st = app.session.active()?;
    let view = app.view();
    let usable = |r: &Rect| [r.x0, r.y0, r.x1, r.y1].iter().all(|v| v.is_finite()) && r.width() > 0.0 && r.height() > 0.0;
    let board = view.and_then(|v| st.doc.artboards.get(v.artboard)).or_else(|| st.doc.artboards.first()).map(|a| a.rect).filter(usable);
    let centre = view.map(|v| v.center).filter(|c| c.x.is_finite() && c.y.is_finite());
    Some((st.uid, fit_box(centre, visible_size(app.canvas_rect, view), board)))
}

/// The canvas's size in document points; with the view rotated, the largest upright square inside
/// it.
fn visible_size(canvas: Option<egui::Rect>, view: Option<&View>) -> Option<(f64, f64)> {
    let (r, v) = (canvas?, view?);
    if !(v.zoom.is_finite() && v.zoom > 0.0) {
        return None;
    }
    let (w, h) = (f64::from(r.width()) / v.zoom, f64::from(r.height()) / v.zoom);
    let turn = if v.rotation.is_finite() { v.rotation.to_radians() } else { 0.0 };
    let (cos, sin) = (turn.cos().abs(), turn.sin().abs());
    let (w, h) = if sin < 1e-9 { (w, h) } else { (w.min(h) / (cos + sin), w.min(h) / (cos + sin)) };
    (w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0).then_some((w, h))
}

/// The box an image is fitted into: centred on `centre` (else the artboard's centre), [`FIT`] of
/// the `visible` size, no bigger than `board` and moved inside it.
fn fit_box(centre: Option<Point>, visible: Option<(f64, f64)>, board: Option<Rect>) -> Rect {
    let (mut w, mut h) = visible.map_or((FALLBACK_SIDE, FALLBACK_SIDE), |(w, h)| (w * FIT, h * FIT));
    let mut c = centre.or(board.map(|b| b.center())).unwrap_or(Point::new(FALLBACK_SIDE / 2.0, FALLBACK_SIDE / 2.0));
    if let Some(b) = board {
        w = w.min(b.width());
        h = h.min(b.height());
        // Not `clamp`: rounding can put the low end a hair past the high one.
        c.x = c.x.max(b.x0 + w / 2.0).min(b.x1 - w / 2.0);
        c.y = c.y.max(b.y0 + h / 2.0).min(b.y1 - h / 2.0);
    }
    Rect::new(c.x - w / 2.0, c.y - h / 2.0, c.x + w / 2.0, c.y + h / 2.0)
}

/// Put generated PNG `bytes` in a document, as `target` says: in its document (made active again
/// when another one is), else centred in the view of the active document. `file.place` embeds it
/// (one undo step); with `trace`, `imageTrace.make` traces it (another). The note to show.
fn place(app: &mut VectorcraftApp, bytes: &[u8], target: Option<Target>, trace: bool) -> Result<String, String> {
    let index = target.and_then(|t| app.session.documents().iter().position(|d| d.uid == t.doc));
    if let Some(i) = index
        && app.session.active_index() != Some(i)
    {
        app.run("document.activate", json!({ "index": i }))?;
    }
    let rect = match target.filter(|_| index.is_some()) {
        Some(t) => t.rect,
        None => target_rect(app).map(|(_, r)| r).ok_or_else(|| NO_DOCUMENT.to_string())?,
    };
    let placed = app.run(
        "file.place",
        json!({
            "name": IMAGE_NAME,
            "dataBase64": vectorcraft_format::base64_encode(bytes),
            "link": false,
            "rect": [rect.x0, rect.y0, rect.width(), rect.height()],
        }),
    )?;
    let id = placed["ids"][0].as_u64();
    match id.filter(|_| trace) {
        None => Ok(PLACED.into()),
        Some(id) => match app.run("imageTrace.make", json!({ "id": id, "preset": TRACE_PRESET })) {
            Ok(_) => Ok(TRACED.into()),
            Err(e) => Ok(format!("The image is on the artboard, but Image Trace did not trace it: {e}")),
        },
    }
}

/// [`place`], keeping the image to Place later when that fails. Whether it was placed.
fn place_generated(app: &mut VectorcraftApp, bytes: Vec<u8>, target: Option<Target>, trace: bool) -> bool {
    let result = place(app, &bytes, target, trace);
    let Some(ai) = app.ai.as_mut() else { return result.is_ok() };
    match result {
        Ok(note) => {
            ai.error = None;
            ai.note = Some(note);
            true
        }
        Err(e) => {
            ai.waiting.push((bytes, trace));
            ai.note = None;
            ai.error = Some(format!("The image is ready, but it could not be placed: {e}"));
            false
        }
    }
}

/// The cost sentence shown before a request: publik's (from the credential, or the crate's own), or
/// who bills an own key.
fn cost_line(state: &AiState, provider: &Provider) -> String {
    match provider {
        Provider::Publik { .. } => match &state.credential {
            Some(c) if !c.cost_sentence.trim().is_empty() => c.cost_sentence.clone(),
            _ => WHY_IT_COSTS.to_string(),
        },
        Provider::Own { service, .. } => format!("Your own key's service ({}) bills this request, not publik.", service.name()),
        Provider::NotSetUp => String::new(),
    }
}

/// The Generate Image dialog's body. `true` closes the dialog.
pub fn generate_image(app: &mut VectorcraftApp, ui: &mut Ui, d: &mut Dialog) -> bool {
    let target = target_rect(app);
    let ctx = ui.ctx().clone();
    let Some(ai) = app.ai.as_mut() else {
        ui.label(OFF);
        return false;
    };
    let provider = ai.state.provider();
    if !provider.is_set_up() && ai.waiting.is_empty() {
        ui.label(NEEDS_SETUP);
        ui.add_space(6.0);
        if ui.button("Set up AI…").clicked() {
            ai.state.begin();
            // Close and Escape come back here once the setup is done.
            let mut setup = Dialog::new(SETUP, json!({}));
            if let Ok(parent) = serde_json::to_value(&*d) {
                setup.fields.insert(crate::dialogs::PARENT.into(), parent);
            }
            *d = setup;
        }
        return false;
    }
    let images = provider.supports(Line::Images);
    let busy = ai.busy();
    ui.label(if target.is_some() { "The image goes on the artboard in view, centred in the window and sized to fit it." } else { NO_DOCUMENT });
    ui.add_space(6.0);
    ui.label("Describe the image:");
    let edit = ui.add_enabled(
        !busy,
        egui::TextEdit::multiline(&mut ai.prompt)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text("A flat fox logo in two colors on a white background"),
    );
    crate::widgets::take_dialog_focus(ui, edit.id, &ai.prompt);
    ui.add_space(4.0);
    let shapes = Shape::offered(provider.is_publik());
    if !shapes.contains(&ai.shape) {
        ai.shape = Shape::Square;
    }
    ui.horizontal(|ui| {
        ui.label("Shape:");
        for s in shapes.iter().copied() {
            ui.add_enabled_ui(!busy, |ui| ui.radio_value(&mut ai.shape, s, s.label()));
        }
    });
    ui.add_space(6.0);
    if images {
        let cost = cost_line(&ai.state, &provider);
        if !cost.is_empty() {
            ui.label(RichText::new(cost).weak());
        }
    } else if provider.is_set_up() {
        ui.colored_label(ui.visuals().warn_fg_color, Fail::unsupported(Line::Images).message);
    }
    ui.add_space(6.0);
    let mut start = None;
    let mut place_waiting = false;
    ui.horizontal(|ui| {
        if busy {
            ui.spinner();
            ui.label("Generating… This can take a minute.");
        } else {
            let can = images && target.is_some() && !ai.prompt.trim().is_empty();
            if ui.add_enabled(can, egui::Button::new("Generate image")).clicked() {
                start = Some(false);
            }
            if ui
                .add_enabled(can, egui::Button::new("Generate and trace"))
                .on_hover_text("Generate the image, then trace it into vector paths with Image Trace (Default preset).")
                .clicked()
            {
                start = Some(true);
            }
        }
        if !ai.waiting.is_empty() && ui.add_enabled(target.is_some(), egui::Button::new("Place image")).clicked() {
            place_waiting = true;
        }
    });
    match ai.waiting.len() {
        0 => {}
        1 => {
            ui.label("A generated image waits to be placed.");
        }
        n => {
            ui.label(format!("{n} generated images wait to be placed."));
        }
    }
    error_line(ui, ai.error.as_deref());
    if let Some(note) = &ai.note {
        ui.add_space(4.0);
        ui.label(note);
    }
    ui.add_space(4.0);
    let action = ai.state.result_bar(ui);
    if let (Some(trace), Some((doc, rect))) = (start, target) {
        ai.note = None;
        ai.error = None;
        let prompt = ai.prompt.trim().to_string();
        let size = if provider.is_publik() { Shape::Square.size() } else { ai.shape.size() };
        let rx = spawn(&ctx, move || client::image_generate(&provider, &prompt, size));
        ai.job = Some((rx, Target { doc, rect, trace }));
    }
    if place_waiting {
        // Into the view as it is now: the first target is what failed.
        let waiting = std::mem::take(&mut ai.waiting);
        ai.error = None;
        for (bytes, trace) in waiting {
            place_generated(app, bytes, None, trace);
        }
    }
    open_url(app, action)
}

// ---- Preferences › AI ----------------------------------------------------------------------------

/// The AI category of Preferences: status, sign-in and key controls, and the setup flow in place.
/// AI settings take effect at once; the dialog's OK and Cancel don't change them.
pub fn options_section(app: &mut VectorcraftApp, ui: &mut Ui) {
    let Some(ai) = app.ai.as_mut() else {
        ui.label(RichText::new(OFF).weak());
        return;
    };
    ui.label(RichText::new("Object › AI › Generate Image uses publik API or your own key. Changes here take effect at once.").weak());
    ui.add_space(6.0);
    let action = if ai.inline_setup {
        let action = ai.state.setup_ui(ui);
        if action == Action::Close {
            ai.inline_setup = false;
        }
        action
    } else {
        let mut open_setup = false;
        let action = ai.state.options_ui(ui, &mut open_setup);
        ai.inline_setup = open_setup;
        action
    };
    open_url(app, action);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use vectorcraft_engine::Session;

    /// A 1×1 PNG.
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn png() -> Vec<u8> {
        client::unbase64(PNG).unwrap()
    }

    /// A 16×8 PNG, black on its left half and white on its right: something Image Trace traces.
    fn two_tone() -> Vec<u8> {
        let img = image::RgbaImage::from_fn(16, 8, |x, _| if x < 8 { image::Rgba([0, 0, 0, 255]) } else { image::Rgba([255, 255, 255, 255]) });
        let mut out = std::io::Cursor::new(vec![]);
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    fn app() -> VectorcraftApp {
        let mut app = VectorcraftApp::new(Session::new(), crate::Services::default());
        app.run("file.new", json!({"width": 600, "height": 400})).unwrap();
        app
    }

    /// An AI host for tests: no settings folder (nothing is written) and a slug no credential file
    /// has; the own key is a placeholder that no test sends.
    fn ai(own_key: bool) -> Ai {
        let info = AppInfo { slug: "vectorcraft-ui-test", name: "VectorCraft", version: "0", app_token: "", dialects: &["images"], config_dir: None };
        let mut ai = Ai::new(info);
        if own_key {
            ai.state.settings.provider = "own".into();
            ai.state.settings.own_key = "sk-placeholder".into();
        }
        ai
    }

    fn image_info(app: &mut VectorcraftApp, id: u64) -> Value {
        app.run("image.info", json!({ "id": id })).unwrap()
    }

    fn selected(app: &VectorcraftApp) -> Vec<u64> {
        app.session.active().unwrap().selection.objects.iter().map(|id| id.0).collect()
    }

    #[test]
    fn shapes_follow_the_box() {
        assert_eq!(Shape::for_size(300.0, 100.0), Shape::Landscape);
        assert_eq!(Shape::for_size(100.0, 300.0), Shape::Portrait);
        assert_eq!(Shape::for_size(100.0, 110.0), Shape::Square);
        assert_eq!(Shape::for_size(0.0, 100.0), Shape::Square);
        assert_eq!(Shape::for_size(f64::NAN, 100.0), Shape::Square);
        assert_eq!(Shape::Landscape.size(), "1536x1024");
        assert_eq!(Shape::Portrait.size(), "1024x1536");
    }

    #[test]
    fn the_box_is_centred_in_the_view_and_kept_on_the_artboard() {
        let board = Rect::new(0.0, 0.0, 600.0, 400.0);
        // The whole artboard in view, and more: the box is the artboard's size, centred on it.
        assert_eq!(fit_box(Some(Point::new(300.0, 200.0)), Some((2000.0, 1000.0)), Some(board)), board);
        // Zoomed in: most of the view, centred in it.
        let r = fit_box(Some(Point::new(200.0, 150.0)), Some((100.0, 50.0)), Some(board));
        assert_eq!(r, Rect::new(160.0, 130.0, 240.0, 170.0));
        // The view at the artboard's edge: the box moves onto the artboard.
        let r = fit_box(Some(Point::new(590.0, 10.0)), Some((100.0, 100.0)), Some(board));
        assert_eq!(r, Rect::new(520.0, 0.0, 600.0, 80.0));
        // The view far from the artboard: the box lands on it.
        let r = fit_box(Some(Point::new(5000.0, 5000.0)), Some((100.0, 100.0)), Some(board));
        assert!(r.x1 <= 600.0 && r.y1 <= 400.0 && r.x0 >= 0.0 && r.y0 >= 0.0, "{r:?}");
        // No view known: a fallback box on the artboard's centre.
        assert_eq!(fit_box(None, None, Some(board)).center(), board.center());
        assert_eq!(fit_box(None, None, None).width(), FALLBACK_SIDE);
    }

    #[test]
    fn a_rotated_view_fits_an_upright_square() {
        let canvas = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 400.0));
        let mut view = View { zoom: 2.0, ..View::default() };
        assert_eq!(visible_size(Some(canvas), Some(&view)), Some((400.0, 200.0)));
        view.rotation = 45.0;
        let (w, h) = visible_size(Some(canvas), Some(&view)).unwrap();
        assert!((w - 200.0 / 2f64.sqrt()).abs() < 1e-6 && w == h, "{w} {h}");
        view.zoom = 0.0;
        assert_eq!(visible_size(Some(canvas), Some(&view)), None);
        assert_eq!(visible_size(None, Some(&view)), None);
    }

    #[test]
    fn a_generated_image_is_placed_embedded_and_fitted_into_the_box() {
        let mut app = app();
        let (doc, rect) = target_rect(&app).unwrap();
        assert!(rect.width() > 0.0 && rect.width() <= 600.0 && rect.height() <= 400.0, "{rect:?}");
        let note = place(&mut app, &png(), Some(Target { doc, rect: Rect::new(100.0, 100.0, 300.0, 200.0), trace: false }), false).unwrap();
        assert_eq!(note, PLACED);
        let [id] = selected(&app)[..] else { panic!("one object selected") };
        let info = image_info(&mut app, id);
        assert_eq!(info["linked"], json!(false), "{info}");
        // A square image in a 200×100 box: 100×100, centred.
        assert_eq!((info["width"].as_f64().unwrap().round(), info["height"].as_f64().unwrap().round()), (100.0, 100.0), "{info}");
        // One undo step takes it away.
        app.run("edit.undo", json!({})).unwrap();
        assert!(app.run("image.info", json!({ "id": id })).is_err());
    }

    #[test]
    fn generate_and_trace_traces_the_placed_image() {
        let mut app = app();
        let (doc, rect) = target_rect(&app).unwrap();
        let note = place(&mut app, &two_tone(), Some(Target { doc, rect, trace: true }), true).unwrap();
        assert_eq!(note, TRACED);
        // The traced object is what Object › Image Trace › Release gives back as the image.
        app.run("imageTrace.release", json!({})).unwrap();
        let [id] = selected(&app)[..] else { panic!("one object selected") };
        assert_eq!(image_info(&mut app, id)["pixelWidth"], json!(16));
    }

    #[test]
    fn the_image_goes_to_the_document_it_was_asked_for() {
        let mut app = app();
        let (first, rect) = target_rect(&app).unwrap();
        app.run("file.new", json!({"width": 300, "height": 300})).unwrap();
        assert_eq!(app.session.active_index(), Some(1));
        place(&mut app, &png(), Some(Target { doc: first, rect, trace: false }), false).unwrap();
        assert_eq!(app.session.active_index(), Some(0), "the first document is active again");
        // That document is gone: the image goes into the active one, in its view.
        app.run("file.close", json!({})).unwrap();
        let note = place(&mut app, &png(), Some(Target { doc: first, rect, trace: false }), false).unwrap();
        assert_eq!(note, PLACED);
    }

    #[test]
    fn an_image_that_cannot_be_placed_waits() {
        let mut app = VectorcraftApp::new(Session::new(), crate::Services::default());
        app.ai = Some(ai(false));
        assert!(target_rect(&app).is_none());
        assert!(!place_generated(&mut app, png(), None, false));
        let a = app.ai.as_ref().unwrap();
        assert_eq!(a.waiting.len(), 1);
        assert!(a.error.as_deref().unwrap().starts_with("The image is ready"), "{:?}", a.error);
        // Once a document is open, Place image puts it there.
        app.run("file.new", json!({})).unwrap();
        let (bytes, trace) = app.ai.as_mut().unwrap().waiting.remove(0);
        assert!(place_generated(&mut app, bytes, None, trace));
        assert_eq!(app.ai.as_ref().unwrap().note.as_deref(), Some(PLACED));
    }

    #[test]
    fn the_first_launch_dialog_opens_once_and_closing_it_counts_as_later() {
        let mut app = app();
        app.ai = Some(ai(false));
        for _ in 0..3 {
            frame(&mut app);
            assert!(app.ui.dialog.is_none(), "not before the window is up");
        }
        frame(&mut app);
        assert_eq!(app.ui.dialog.as_ref().map(|d| d.kind.as_str()), Some(SETUP));
        crate::dialogs::cancel(&mut app);
        assert!(app.ui.dialog.is_none());
        assert!(app.ai.as_ref().unwrap().state.settings.setup_done);
        for _ in 0..5 {
            frame(&mut app);
        }
        assert!(app.ui.dialog.is_none(), "once");
        // A host with AI set up already never asks.
        let mut app = self::app();
        app.ai = Some(ai(true));
        for _ in 0..5 {
            frame(&mut app);
        }
        assert!(app.ui.dialog.is_none());
    }

    #[test]
    fn generate_image_opens_from_the_object_menu_and_sends_nothing_on_its_own() {
        let mut app = app();
        assert!(crate::menus::enabled(&app, "ui.aiGenerateImage"));
        let object = crate::menus::menu_tree().into_iter().find(|(t, _)| *t == "Object").unwrap().1;
        let in_ai = object.iter().any(|i| {
            matches!(i, crate::menus::Item::Sub("AI", items) if items.iter().any(|i| matches!(i, crate::menus::Item::Cmd(_, "ui.aiGenerateImage", _))))
        });
        assert!(in_ai, "Object › AI › Generate Image…");
        // Without AI (runs without preferences) the dialog opens and says so.
        app.run("ui.aiGenerateImage", json!({})).unwrap();
        assert_eq!(app.ui.dialog.as_ref().map(|d| d.kind.as_str()), Some(GENERATE_IMAGE));
        // OK from the control channel only closes it.
        assert_eq!(crate::dialogs::confirm(&mut app).unwrap(), Value::Null);
        assert!(app.ui.dialog.is_none());
        // With AI: the shape starts as the box in view's (a landscape artboard), and nothing is in
        // flight.
        app.ai = Some(ai(true));
        app.run("ui.aiGenerateImage", json!({})).unwrap();
        let a = app.ai.as_ref().unwrap();
        assert_eq!(a.shape, Shape::Landscape);
        assert!(!a.busy());
        // No document: the item is off.
        let empty = VectorcraftApp::new(Session::new(), crate::Services::default());
        assert!(!crate::menus::enabled(&empty, "ui.aiGenerateImage"));
    }

    fn texts(s: &egui::Shape, out: &mut Vec<String>) {
        match s {
            egui::Shape::Text(t) => out.push(t.galley.text().to_string()),
            egui::Shape::Vec(v) => v.iter().for_each(|s| texts(s, out)),
            _ => {}
        }
    }

    /// The text a frame of the open dialog draws (two frames: the window measures itself first).
    fn dialog_texts(app: &mut VectorcraftApp, ctx: &egui::Context) -> Vec<String> {
        let raw =
            || egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))), ..Default::default() };
        let mut shown = vec![];
        for _ in 0..2 {
            let mut out = ctx.run_ui(raw(), |ui| crate::dialogs::show(app, ui.ctx()));
            out.textures_delta.clear();
            shown.clear();
            out.shapes.iter().for_each(|c| texts(&c.shape, &mut shown));
        }
        shown
    }

    #[test]
    fn the_dialog_shows_where_the_image_goes_its_shapes_and_who_bills_before_any_send() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = app();
        app.ai = Some(ai(true));
        app.run("ui.aiGenerateImage", json!({})).unwrap();
        let shown = dialog_texts(&mut app, &ctx);
        let has = |s: &str| shown.iter().any(|t| t.trim().starts_with(s));
        for expected in ["Generate Image", "Describe the image:", "Square", "Landscape", "Portrait", "Generate image", "Generate and trace", "Close"]
        {
            assert!(has(expected), "{expected} missing from {shown:?}");
        }
        assert!(has("Your own key's service"), "the cost line comes before the button: {shown:?}");
        assert!(!has("OK"), "no OK: only the dialog's own buttons send");
        assert!(!app.ai.as_ref().unwrap().busy(), "drawing sends nothing");
        // Not set up: the dialog offers the setup instead, which goes back to it when closed.
        app.ai = Some(ai(false));
        let shown = dialog_texts(&mut app, &ctx);
        assert!(shown.iter().any(|t| t == NEEDS_SETUP), "{shown:?}");
        assert!(shown.iter().any(|t| t == "Set up AI…"), "{shown:?}");
    }

    #[test]
    fn preferences_list_ai_only_where_the_host_installed_it() {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let mut app = app();
        crate::prefs_dialog::open(&mut app, Some("ai"));
        assert_eq!(app.ui.dialog.as_ref().unwrap().str("__category"), "General", "no AI category without AI");
        app.ai = Some(ai(true));
        crate::prefs_dialog::open(&mut app, Some("ai"));
        assert_eq!(app.ui.dialog.as_ref().unwrap().str("__category"), "AI");
        let mut shown = vec![];
        for _ in 0..2 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| crate::prefs_dialog::show(&mut app, ui.ctx()));
            out.textures_delta.clear();
            shown.clear();
            out.shapes.iter().for_each(|c| texts(&c.shape, &mut shown));
        }
        assert!(shown.iter().any(|t| t.starts_with("Object › AI › Generate Image uses publik API")), "{shown:?}");
        assert!(shown.iter().any(|t| t.starts_with("Your own key")), "the provider's status: {shown:?}");
    }
}

#[cfg(test)]
mod publik_shape_tests {
    use super::Shape;

    #[test]
    fn publik_api_offers_square_only() {
        assert_eq!(Shape::offered(true).len(), 1);
        assert_eq!(Shape::offered(true)[0].size(), "1024x1024");
        assert_eq!(Shape::offered(false).len(), 3);
    }
}
