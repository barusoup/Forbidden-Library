use dream::ui::font::{FontFamily, Fonts};

/// The whole app uses Roboto. Regular, Medium and Bold are embedded so the
/// installer and the app carry the same typography on every machine.
pub fn roboto() -> Fonts {
    let mut fonts = Fonts::new();
    let regular = fonts
        .load_bytes(
            "Roboto-Regular",
            include_bytes!("../../../assets/fonts/Roboto-Regular.ttf"),
        )
        .expect("embedded Roboto Regular");
    let medium = fonts
        .load_bytes(
            "Roboto-Medium",
            include_bytes!("../../../assets/fonts/Roboto-Medium.ttf"),
        )
        .expect("embedded Roboto Medium");
    let bold = fonts
        .load_bytes(
            "Roboto-Bold",
            include_bytes!("../../../assets/fonts/Roboto-Bold.ttf"),
        )
        .expect("embedded Roboto Bold");
    fonts.set_family(FontFamily {
        regular,
        medium,
        semibold: bold,
        mono: regular,
    });
    fonts
}
