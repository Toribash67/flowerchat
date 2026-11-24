//! Every colour, size and font the interface uses, in one place.

use std::sync::Arc;

use egui::epaint::text::{FontTweak, VariationCoords};
use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, RichText, Stroke,
    TextStyle, Vec2,
};

pub const BACKGROUND: Color32 = Color32::from_rgb(0x15, 0x17, 0x1B);
/// Behind a shared screen.
pub const STAGE: Color32 = Color32::from_rgb(0x0B, 0x0C, 0x0E);
/// Side columns and bars.
pub const PANEL: Color32 = Color32::from_rgb(0x1A, 0x1C, 0x21);
/// Text fields.
pub const FIELD: Color32 = Color32::from_rgb(0x1D, 0x20, 0x25);
/// Buttons.
pub const CONTROL: Color32 = Color32::from_rgb(0x23, 0x27, 0x2E);
pub const CONTROL_HOVER: Color32 = Color32::from_rgb(0x2C, 0x30, 0x38);
pub const BORDER: Color32 = Color32::from_rgb(0x30, 0x35, 0x3E);
/// Lines between table rows.
pub const HAIRLINE: Color32 = Color32::from_rgb(0x23, 0x27, 0x2E);
pub const TEXT: Color32 = Color32::from_rgb(0xE8, 0xEA, 0xED);
pub const MUTED: Color32 = Color32::from_rgb(0xA3, 0xAA, 0xB5);
/// Outline of an indicator for someone who is not speaking.
pub const IDLE: Color32 = Color32::from_rgb(0x6B, 0x72, 0x80);
pub const ACCENT: Color32 = Color32::from_rgb(0x4C, 0xC3, 0x8A);
pub const ON_ACCENT: Color32 = Color32::from_rgb(0x0E, 0x15, 0x12);
/// Muted, deafened and other states the user should not overlook.
pub const WARN: Color32 = Color32::from_rgb(0xF2, 0x99, 0x4A);
pub const ON_WARN: Color32 = Color32::from_rgb(0x1A, 0x12, 0x06);

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
    RichText::new(text).family(semibold_family()).color(TEXT)
}

pub fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).size(SMALL).color(MUTED)
}

pub fn title(text: impl Into<String>) -> RichText {
    RichText::new(text)
        .family(semibold_family())
        .size(TITLE)
        .color(TEXT)
}

/// Applies fonts, colours and spacing. Safe to call every frame; it only
/// does the work once per context.
///
/// Returns `true` when it has just done so. New fonts only become usable on
/// the following frame, so a caller that gets `true` in the middle of a
/// frame must not draw text with them until the next one.
pub fn install(ctx: &egui::Context) -> bool {
    let installed = egui::Id::new("flowerchat-theme-installed");
    if ctx.data(|data| data.get_temp::<bool>(installed).unwrap_or(false)) {
        return false;
    }
    ctx.data_mut(|data| data.insert_temp(installed, true));

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
        *visuals = egui::Visuals::dark();
        visuals.panel_fill = BACKGROUND;
        visuals.window_fill = PANEL;
        visuals.extreme_bg_color = FIELD;
        visuals.faint_bg_color = HAIRLINE;
        visuals.override_text_color = Some(TEXT);
        visuals.weak_text_color = Some(MUTED);
        visuals.hyperlink_color = TEXT;
        visuals.selection.bg_fill = ACCENT;
        visuals.selection.stroke = Stroke::new(1.0, ON_ACCENT);
        visuals.slider_trailing_fill = true;
        visuals.handle_shape = egui::style::HandleShape::Circle;

        let radius = CornerRadius::same(RADIUS);
        let border = Stroke::new(1.0, BORDER);
        let widgets = &mut visuals.widgets;
        widgets.noninteractive.bg_fill = BACKGROUND;
        widgets.noninteractive.bg_stroke = border;
        widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        widgets.noninteractive.corner_radius = radius;
        for (state, fill) in [
            (&mut widgets.inactive, CONTROL),
            (&mut widgets.hovered, CONTROL_HOVER),
            (&mut widgets.active, CONTROL_HOVER),
            (&mut widgets.open, CONTROL_HOVER),
        ] {
            state.bg_fill = fill;
            state.weak_bg_fill = fill;
            state.bg_stroke = border;
            state.fg_stroke = Stroke::new(1.0, TEXT);
            state.corner_radius = radius;
            state.expansion = 0.0;
        }
    });
    true
}
