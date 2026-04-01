use std::path::Path;

use devicons::{icon_for_file, File, FileIcon, Theme};
use crate::view::graphics::Color;

/// An icon with its associated foreground color.
pub struct Icon {
    pub icon: char,
    pub color: Color,
}

const DIR_ICON: Icon = Icon {
    icon: '\u{f115}',
    color: Color::Reset,
};

fn parse_hex_color(hex: &str) -> Color {
    Color::from_hex(hex).unwrap_or(Color::Reset)
}

fn file_icon_from(fi: FileIcon) -> Icon {
    Icon {
        icon: fi.icon,
        color: parse_hex_color(fi.color),
    }
}

/// Returns a nerdfont icon and color for the given file path.
pub fn file_icon(path: &Path) -> Icon {
    file_icon_from(icon_for_file(File::new(path), &Some(Theme::Dark)))
}

/// Returns a nerdfont directory icon.
pub fn directory_icon() -> &'static Icon {
    &DIR_ICON
}
