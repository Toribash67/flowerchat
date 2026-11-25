//! Every colour, size and font the interface uses, in one place.

use std::cell::Cell;
use std::sync::Arc;

use egui::epaint::text::{FontTweak, VariationCoords};
use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, RichText, Stroke,
    TextStyle, Vec2,
};

use crate::palettes;

/// The colours of the interface, by what they are used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colors {
    pub background: Color32,
    /// Behind a shared screen.
    pub stage: Color32,
    /// Side columns and bars.
    pub panel: Color32,
    /// Text fields.
    pub field: Color32,
    /// Buttons.
    pub control: Color32,
    pub control_hover: Color32,
    pub border: Color32,
    /// Lines between table rows.
    pub hairline: Color32,
    pub text: Color32,
    pub muted: Color32,
    /// Outline of an indicator for someone who is not speaking.
    pub idle: Color32,
    pub accent: Color32,
    pub on_accent: Color32,
    /// Muted, deafened and other states the user should not overlook.
    pub warn: Color32,
    pub on_warn: Color32,
}

/// A set of colours the user can pick: see [`crate::palettes`].
#[derive(Debug, PartialEq, Eq)]
pub struct Palette {
    /// What it is saved as.
    pub id: &'static str,
    pub name: &'static str,
    /// Whether it is dark text on a light background.
    pub light: bool,
    pub colors: Colors,
}

thread_local! {
    // The window is drawn on one thread, and so is each test's. Kept per
    // thread, a test that draws in other colours leaves the others alone.
    static CURRENT: Cell<&'static Palette> = Cell::new(palettes::default());
}

/// The palette the interface is drawn in.
pub fn palette() -> &'static Palette {
    CURRENT.get()
}

/// Its colours.
pub fn colors() -> &'static Colors {
    &palette().colors
}

/// Changes the palette, for whatever this thread draws from now on.
/// [`install`] carries it over to the widgets.
pub fn set_palette(palette: &'static Palette) {
    CURRENT.set(palette);
}

pub const BODY: f32 = 13.0;
pub const SMALL: f32 = 12.0;
pub const TINY: f32 = 11.0;
pub const TITLE: f32 = 18.0;

/// Height of buttons and text fields.
pub const CONTROL_HEIGHT: f32 = 28.0;
pub const ROW_HEIGHT: f32 = 34.0;
pub const RADIUS: u8 = 4;

/// Below this window width the columns collapse into one.
pub const NARROW_BELOW: f32 = 900.0;

const INTER: &[u8] = include_bytes!("../assets/InterVariable.ttf");
const SEMIBOLD: &str = "semibold";

fn semibold_family() -> FontFamily {
    FontFamily::Name(SEMIBOLD.into())
}

/// The heavier weight of the interface font, for text built by hand.
pub fn semibold() -> FontFamily {
    semibold_family()
}

/// Body text in the heavier weight.
pub fn strong(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .family(semibold_family())
        .color(colors().text)
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).size(SMALL).color(colors().muted)
}

pub fn title(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .family(semibold_family())
        .size(TITLE)
        .color(colors().text)
}

/// Applies fonts, colours and spacing. Safe to call every frame: the fonts
/// are only set up once per context, and the rest again whenever the
/// palette has changed.
///
/// Returns `true` when it has just set up the fonts. New fonts only become
/// usable on the following frame, so a caller that gets `true` in the middle
/// of a frame must not draw text with them until the next one.
pub fn install(ctx: &egui::Context) -> bool {
    let installed = egui::Id::new("flowerchat-theme-installed");
    let first = !ctx.data(|data| data.get_temp::<bool>(installed).unwrap_or(false));
    if first {
        ctx.data_mut(|data| data.insert_temp(installed, true));
        install_fonts(ctx);
    }
    let painted = egui::Id::new("flowerchat-theme-palette");
    let palette = palette();
    if ctx.data(|data| data.get_temp::<&'static str>(painted)) != Some(palette.id) {
        ctx.data_mut(|data| data.insert_temp(painted, palette.id));
        install_style(ctx, palette);
        // What is being drawn right now began in the old colours.
        ctx.request_repaint();
    }
    first
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts
        .font_data
        .insert("inter".into(), Arc::new(FontData::from_static(INTER)));
    // The same variable font file again, at a heavier weight.
    fonts.font_data.insert(
        "inter-semibold".into(),
        Arc::new(FontData::from_static(INTER).tweak(FontTweak {
            coords: VariationCoords::new([("wght", 600.0)]),
            ..Default::default()
        })),
    );
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "inter".into());
    egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
    fonts.families.insert(
        semibold_family(),
        vec!["inter-semibold".into(), "phosphor".into()],
    );
    ctx.set_fonts(fonts);
}

fn install_style(ctx: &egui::Context, palette: &Palette) {
    let colors = palette.colors;
    ctx.all_styles_mut(|style| {
        style.text_styles = [
            (TextStyle::Body, FontId::proportional(BODY)),
            (TextStyle::Button, FontId::proportional(BODY)),
            (TextStyle::Small, FontId::proportional(SMALL)),
            (TextStyle::Heading, FontId::new(TITLE, semibold_family())),
            (TextStyle::Monospace, FontId::monospace(SMALL)),
        ]
        .into();
        style.spacing.item_spacing = Vec2::new(8.0, 6.0);
        style.spacing.button_padding = Vec2::new(10.0, 4.0);
        style.spacing.interact_size.y = CONTROL_HEIGHT;
        style.spacing.slider_rail_height = 4.0;

        let visuals = &mut style.visuals;
        *visuals = if palette.light {
            egui::Visuals::light()
        } else {
            egui::Visuals::dark()
        };
        visuals.panel_fill = colors.background;
        visuals.window_fill = colors.panel;
        visuals.extreme_bg_color = colors.field;
        visuals.faint_bg_color = colors.hairline;
        visuals.override_text_color = Some(colors.text);
        visuals.weak_text_color = Some(colors.muted);
        visuals.hyperlink_color = colors.text;
        visuals.selection.bg_fill = colors.accent;
        visuals.selection.stroke = Stroke::new(1.0, colors.on_accent);
        visuals.slider_trailing_fill = true;
        visuals.handle_shape = egui::style::HandleShape::Circle;

        let radius = CornerRadius::same(RADIUS);
        let border = Stroke::new(1.0, colors.border);
        let widgets = &mut visuals.widgets;
        widgets.noninteractive.bg_fill = colors.background;
        widgets.noninteractive.bg_stroke = border;
        widgets.noninteractive.fg_stroke = Stroke::new(1.0, colors.text);
        widgets.noninteractive.corner_radius = radius;
        for (state, fill) in [
            (&mut widgets.inactive, colors.control),
            (&mut widgets.hovered, colors.control_hover),
            (&mut widgets.active, colors.control_hover),
            (&mut widgets.open, colors.control_hover),
        ] {
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
            state.bg_stroke = border;
            state.fg_stroke = Stroke::new(1.0, colors.text);
            state.corner_radius = radius;
            state.expansion = 0.0;
        }
    });
}
