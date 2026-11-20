use dream::ui::font::{FontFamily, Fonts};
use dream::ui::widgets::Ui;

/// The whole app uses Roboto. Regular, Medium and Bold are embedded so the
/// installer and the app carry the same typography on every machine.
const REGULAR: &[u8] = include_bytes!("../../../assets/fonts/Roboto-Regular.ttf");
const MEDIUM: &[u8] = include_bytes!("../../../assets/fonts/Roboto-Medium.ttf");
const BOLD: &[u8] = include_bytes!("../../../assets/fonts/Roboto-Bold.ttf");

/// Load Roboto into `fonts` and make it the family every weight resolves to.
fn load(fonts: &mut Fonts) {
    let regular = fonts
        .load_bytes("Roboto-Regular", REGULAR)
        .expect("embedded Roboto Regular");
    let medium = fonts
        .load_bytes("Roboto-Medium", MEDIUM)
        .expect("embedded Roboto Medium");
    let bold = fonts
        .load_bytes("Roboto-Bold", BOLD)
        .expect("embedded Roboto Bold");
    fonts.set_family(FontFamily {
        regular,
        medium,
        semibold: bold,
        mono: regular,
    });
}

/// A font set for the headless app and the render-check harness, which build
/// their own UI.
pub fn roboto() -> Fonts {
    let mut fonts = Fonts::new();
    load(&mut fonts);
    fonts
}

/// The runner builds the UI with Dream's bundled faces; the app swaps its own
/// family in before the first frame.
pub fn install(ui: &mut Ui) {
    load(ui.fonts_mut());
}
