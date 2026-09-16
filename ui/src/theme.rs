//! No `ConfigTheme`, just a name.

use cursive::theme::{BaseColor, BorderStyle, Color, Palette, PaletteColor, Theme};

/// Build a [`Theme`] for the named config theme. Only "default" is defined in
/// M1; unknown names fall back to it (`theme = "default"`).
pub fn load(name: &str) -> Theme {
    let mut palette = Palette::default();

    palette[PaletteColor::Background] = Color::TerminalDefault;
    palette[PaletteColor::View] = Color::TerminalDefault;
    palette[PaletteColor::Primary] = Color::TerminalDefault;
    palette[PaletteColor::Secondary] = Color::Dark(BaseColor::Blue);
    palette[PaletteColor::TitlePrimary] = Color::Dark(BaseColor::Red);
    palette[PaletteColor::Highlight] = Color::Dark(BaseColor::Red);
    palette[PaletteColor::HighlightText] = Color::Dark(BaseColor::White);
    palette[PaletteColor::HighlightInactive] = Color::Dark(BaseColor::Blue);

    palette.set_color("playing", Color::Dark(BaseColor::Blue));
    palette.set_color("statusbar_progress", Color::Dark(BaseColor::Blue));
    palette.set_color("statusbar_progress_bg", Color::Light(BaseColor::Black));

    let borders = match name {
        "none" | "plain" => BorderStyle::None,
        _ => BorderStyle::Simple,
    };

    Theme {
        shadow: false,
        palette,
        borders,
    }
}
