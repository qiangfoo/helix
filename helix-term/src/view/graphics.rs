use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use std::{
    cmp::min,
    fmt,
    str::FromStr,
};

pub use ratatui::layout::Rect;

#[must_use]
const fn from_nibble(h: u8) -> u8 {
    match h {
        b'A'..=b'F' => h - b'A' + 10,
        b'a'..=b'f' => h - b'a' + 10,
        b'0'..=b'9' => h - b'0',
        _ => 0xff, // Err
    }
}

/// Decodes nibble, repeating its value on each half,
/// i.e. the value is its own padding.
///
/// # Errors
/// If `h` isn't a nibble
#[must_use]
const fn dupe_from_nibble(mut h: u8) -> Option<u8> {
    h = from_nibble(h);
    if h > 0xf {
        return None;
    }
    Some((h << 4) | h)
}

/// Decodes big-endian nibble-pair.
///
/// # Errors
/// If any byte isn't a nibble
const fn byte_from_hex(mut h: [u8; 2]) -> Option<u8> {
    // reuse memory
    h[0] = from_nibble(h[0]);
    h[1] = from_nibble(h[1]);
    // we could split this in 2 `if`s,
    // to avoid calling `from_nibble`,
    // but that might be slower
    if h[0] > 0xf || h[1] > 0xf {
        return None;
    }
    Some((h[0] << 4) | h[1])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
/// UNSTABLE
#[derive(Default)]
pub enum CursorKind {
    /// █
    #[default]
    Block,
    /// |
    Bar,
    /// _
    Underline,
    /// Hidden cursor, can set cursor position with this to let IME have correct cursor position.
    Hidden,
}

pub use ratatui::layout::Margin;

/// Extension trait for ratatui's Rect providing custom methods needed by helix.
pub trait RectExt {
    fn clip_left(self, width: u16) -> Rect;
    fn clip_right(self, width: u16) -> Rect;
    fn clip_top(self, height: u16) -> Rect;
    fn clip_bottom(self, height: u16) -> Rect;
    fn with_height(self, height: u16) -> Rect;
    fn with_width(self, width: u16) -> Rect;

}

impl RectExt for Rect {
    fn clip_left(self, width: u16) -> Rect {
        let width = min(width, self.width);
        Rect {
            x: self.x.saturating_add(width),
            width: self.width.saturating_sub(width),
            ..self
        }
    }

    fn clip_right(self, width: u16) -> Rect {
        Rect {
            width: self.width.saturating_sub(width),
            ..self
        }
    }

    fn clip_top(self, height: u16) -> Rect {
        let height = min(height, self.height);
        Rect {
            y: self.y.saturating_add(height),
            height: self.height.saturating_sub(height),
            ..self
        }
    }

    fn clip_bottom(self, height: u16) -> Rect {
        Rect {
            height: self.height.saturating_sub(height),
            ..self
        }
    }

    fn with_height(self, height: u16) -> Rect {
        Rect::new(self.x, self.y, self.width, height)
    }

    fn with_width(self, width: u16) -> Rect {
        Rect::new(self.x, self.y, width, self.height)
    }

}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Reset,
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    Gray,
    LightRed,
    LightGreen,
    LightYellow,
    LightBlue,
    LightMagenta,
    LightCyan,
    LightGray,
    White,
    Rgb(u8, u8, u8),
    Indexed(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalformedHex {
    NoHash,
    LenOOB,
    NotANibble,
}
impl fmt::Display for MalformedHex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Malformed hex color code: {}",
            match self {
                Self::NoHash => "Missing hash prefix",
                Self::LenOOB => "Must be 12 or 24 bit RGB",
                Self::NotANibble => "One or more chars is not hex digit (nibble)",
            }
        )
    }
}

impl Color {
    /// Creates a `Color` from a hex string of the form
    /// "#RRGGBB" or "#RGB"
    ///
    /// # Examples
    ///
    /// ```rust
    /// use helix_view::theme::Color;
    ///
    /// let color1 = Color::from_hex("#c0ffee").unwrap();
    /// let color2 = Color::Rgb(192, 255, 238);
    ///
    /// assert_eq!(color1, color2);
    ///
    /// let color3 = Color::from_hex("#012").unwrap();
    /// assert_eq!(color3, Color::Rgb(0, 17, 34));
    /// ```
    pub fn from_hex(h: &str) -> Result<Self, MalformedHex> {
        let h = h.as_bytes();
        if !h.starts_with(b"#") {
            return Err(MalformedHex::NoHash);
        }

        use byte_from_hex as pair;
        use dupe_from_nibble as nibble;

        match h.len() {
            7 => match (|| {
                Some(Self::Rgb(
                    pair([h[1], h[2]])?,
                    pair([h[3], h[4]])?,
                    pair([h[5], h[6]])?,
                ))
            })() {
                Some(c) => Ok(c),
                None => Err(MalformedHex::NotANibble),
            },
            4 => match (|| Some(Self::Rgb(nibble(h[1])?, nibble(h[2])?, nibble(h[3])?)))() {
                Some(c) => Ok(c),
                None => Err(MalformedHex::NotANibble),
            },
            _ => Err(MalformedHex::LenOOB),
        }
    }
}

impl From<Color> for crossterm::style::Color {
    fn from(color: Color) -> Self {
        use crossterm::style::Color as CColor;

        match color {
            Color::Reset => CColor::Reset,
            Color::Black => CColor::Black,
            Color::Red => CColor::DarkRed,
            Color::Green => CColor::DarkGreen,
            Color::Yellow => CColor::DarkYellow,
            Color::Blue => CColor::DarkBlue,
            Color::Magenta => CColor::DarkMagenta,
            Color::Cyan => CColor::DarkCyan,
            Color::Gray => CColor::DarkGrey,
            Color::LightRed => CColor::Red,
            Color::LightGreen => CColor::Green,
            Color::LightBlue => CColor::Blue,
            Color::LightYellow => CColor::Yellow,
            Color::LightMagenta => CColor::Magenta,
            Color::LightCyan => CColor::Cyan,
            Color::LightGray => CColor::Grey,
            Color::White => CColor::White,
            Color::Indexed(i) => CColor::AnsiValue(i),
            Color::Rgb(r, g, b) => CColor::Rgb { r, g, b },
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnderlineStyle {
    Reset,
    Line,
    Curl,
    Dotted,
    Dashed,
    DoubleLine,
}

impl FromStr for UnderlineStyle {
    type Err = &'static str;

    fn from_str(modifier: &str) -> Result<Self, Self::Err> {
        match modifier {
            "line" => Ok(Self::Line),
            "curl" => Ok(Self::Curl),
            "dotted" => Ok(Self::Dotted),
            "dashed" => Ok(Self::Dashed),
            "double_line" => Ok(Self::DoubleLine),
            _ => Err("Invalid underline style"),
        }
    }
}

impl From<UnderlineStyle> for crossterm::style::Attribute {
    fn from(style: UnderlineStyle) -> Self {
        match style {
            UnderlineStyle::Line => crossterm::style::Attribute::Underlined,
            UnderlineStyle::Curl => crossterm::style::Attribute::Undercurled,
            UnderlineStyle::Dotted => crossterm::style::Attribute::Underdotted,
            UnderlineStyle::Dashed => crossterm::style::Attribute::Underdashed,
            UnderlineStyle::DoubleLine => crossterm::style::Attribute::DoubleUnderlined,
            UnderlineStyle::Reset => crossterm::style::Attribute::NoUnderline,
        }
    }
}

bitflags! {
    /// Modifier changes the way a piece of text is displayed.
    ///
    /// They are bitflags so they can easily be composed.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// # use helix_view::graphics::Modifier;
    ///
    /// let m = Modifier::BOLD | Modifier::ITALIC;
    /// ```
    #[derive(PartialEq, Eq, Debug, Clone, Copy)]
    pub struct Modifier: u16 {
        const BOLD              = 0b0000_0000_0001;
        const DIM               = 0b0000_0000_0010;
        const ITALIC            = 0b0000_0000_0100;
        const SLOW_BLINK        = 0b0000_0001_0000;
        const RAPID_BLINK       = 0b0000_0010_0000;
        const REVERSED          = 0b0000_0100_0000;
        const HIDDEN            = 0b0000_1000_0000;
        const CROSSED_OUT       = 0b0001_0000_0000;
    }
}

impl FromStr for Modifier {
    type Err = &'static str;

    fn from_str(modifier: &str) -> Result<Self, Self::Err> {
        match modifier {
            "bold" => Ok(Self::BOLD),
            "dim" => Ok(Self::DIM),
            "italic" => Ok(Self::ITALIC),
            "slow_blink" => Ok(Self::SLOW_BLINK),
            "rapid_blink" => Ok(Self::RAPID_BLINK),
            "reversed" => Ok(Self::REVERSED),
            "hidden" => Ok(Self::HIDDEN),
            "crossed_out" => Ok(Self::CROSSED_OUT),
            _ => Err("Invalid modifier"),
        }
    }
}

/// Style let you control the main characteristics of the displayed elements.
///
/// ```rust
/// # use helix_view::graphics::{Color, Modifier, Style};
/// Style::default()
///     .fg(Color::Black)
///     .bg(Color::Green)
///     .add_modifier(Modifier::ITALIC | Modifier::BOLD);
/// ```
///
/// It represents an incremental change. If you apply the styles S1, S2, S3 to a cell of the
/// terminal buffer, the style of this cell will be the result of the merge of S1, S2 and S3, not
/// just S3.
///
/// ```rust
/// # use helix_view::graphics::{Rect, Color, UnderlineStyle, Modifier, Style};
/// # use helix_tui::buffer::Buffer;
/// let styles = [
///     Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD | Modifier::ITALIC),
///     Style::default().bg(Color::Red),
///     Style::default().fg(Color::Yellow).remove_modifier(Modifier::ITALIC),
/// ];
/// let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
/// for style in &styles {
///   buffer[(0, 0)].set_style(*style);
/// }
/// assert_eq!(
///     Style {
///         fg: Some(Color::Yellow),
///         bg: Some(Color::Red),
///         add_modifier: Modifier::BOLD,
///         underline_color: Some(Color::Reset),
///         underline_style: Some(UnderlineStyle::Reset),
///         sub_modifier: Modifier::empty(),
///     },
///     buffer[(0, 0)].style(),
/// );
/// ```
///
/// The default implementation returns a `Style` that does not modify anything. If you wish to
/// reset all properties until that point use [`Style::reset`].
///
/// ```
/// # use helix_view::graphics::{Rect, Color, UnderlineStyle, Modifier, Style};
/// # use helix_tui::buffer::Buffer;
/// let styles = [
///     Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD | Modifier::ITALIC),
///     Style::reset().fg(Color::Yellow),
/// ];
/// let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
/// for style in &styles {
///   buffer[(0, 0)].set_style(*style);
/// }
/// assert_eq!(
///     Style {
///         fg: Some(Color::Yellow),
///         bg: Some(Color::Reset),
///         underline_color: Some(Color::Reset),
///         underline_style: Some(UnderlineStyle::Reset),
///         add_modifier: Modifier::empty(),
///         sub_modifier: Modifier::empty(),
///     },
///     buffer[(0, 0)].style(),
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub underline_color: Option<Color>,
    pub underline_style: Option<UnderlineStyle>,
    pub add_modifier: Modifier,
    pub sub_modifier: Modifier,
}

impl Default for Style {
    fn default() -> Self {
        Self::new()
    }
}

impl Style {
    pub const fn new() -> Self {
        Style {
            fg: None,
            bg: None,
            underline_color: None,
            underline_style: None,
            add_modifier: Modifier::empty(),
            sub_modifier: Modifier::empty(),
        }
    }

    /// Returns a `Style` resetting all properties.
    pub const fn reset() -> Self {
        Self {
            fg: Some(Color::Reset),
            bg: Some(Color::Reset),
            underline_color: None,
            underline_style: None,
            add_modifier: Modifier::empty(),
            sub_modifier: Modifier::all(),
        }
    }

    /// Changes the foreground color.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// # use helix_view::graphics::{Color, Style};
    /// let style = Style::default().fg(Color::Blue);
    /// let diff = Style::default().fg(Color::Red);
    /// assert_eq!(style.patch(diff), Style::default().fg(Color::Red));
    /// ```
    pub const fn fg(mut self, color: Color) -> Style {
        self.fg = Some(color);
        self
    }

    /// Changes the background color.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// # use helix_view::graphics::{Color, Style};
    /// let style = Style::default().bg(Color::Blue);
    /// let diff = Style::default().bg(Color::Red);
    /// assert_eq!(style.patch(diff), Style::default().bg(Color::Red));
    /// ```
    pub const fn bg(mut self, color: Color) -> Style {
        self.bg = Some(color);
        self
    }

    /// Changes the underline color.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// # use helix_view::graphics::{Color, Style};
    /// let style = Style::default().underline_color(Color::Blue);
    /// let diff = Style::default().underline_color(Color::Red);
    /// assert_eq!(style.patch(diff), Style::default().underline_color(Color::Red));
    /// ```
    pub const fn underline_color(mut self, color: Color) -> Style {
        self.underline_color = Some(color);
        self
    }

    /// Changes the underline style.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// # use helix_view::graphics::{UnderlineStyle, Style};
    /// let style = Style::default().underline_style(UnderlineStyle::Line);
    /// let diff = Style::default().underline_style(UnderlineStyle::Curl);
    /// assert_eq!(style.patch(diff), Style::default().underline_style(UnderlineStyle::Curl));
    /// ```
    pub const fn underline_style(mut self, style: UnderlineStyle) -> Style {
        self.underline_style = Some(style);
        self
    }

    /// Changes the text emphasis.
    ///
    /// When applied, it adds the given modifier to the `Style` modifiers.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// # use helix_view::graphics::{Color, Modifier, Style};
    /// let style = Style::default().add_modifier(Modifier::BOLD);
    /// let diff = Style::default().add_modifier(Modifier::ITALIC);
    /// let patched = style.patch(diff);
    /// assert_eq!(patched.add_modifier, Modifier::BOLD | Modifier::ITALIC);
    /// assert_eq!(patched.sub_modifier, Modifier::empty());
    /// ```
    pub fn add_modifier(mut self, modifier: Modifier) -> Style {
        self.sub_modifier.remove(modifier);
        self.add_modifier.insert(modifier);
        self
    }

    /// Changes the text emphasis.
    ///
    /// When applied, it removes the given modifier from the `Style` modifiers.
    ///
    /// ## Examples
    ///
    /// ```rust
    /// # use helix_view::graphics::{Color, Modifier, Style};
    /// let style = Style::default().add_modifier(Modifier::BOLD | Modifier::ITALIC);
    /// let diff = Style::default().remove_modifier(Modifier::ITALIC);
    /// let patched = style.patch(diff);
    /// assert_eq!(patched.add_modifier, Modifier::BOLD);
    /// assert_eq!(patched.sub_modifier, Modifier::ITALIC);
    /// ```
    pub fn remove_modifier(mut self, modifier: Modifier) -> Style {
        self.add_modifier.remove(modifier);
        self.sub_modifier.insert(modifier);
        self
    }

    /// Results in a combined style that is equivalent to applying the two individual styles to
    /// a style one after the other.
    ///
    /// ## Examples
    /// ```
    /// # use helix_view::graphics::{Color, Modifier, Style};
    /// let style_1 = Style::default().fg(Color::Yellow);
    /// let style_2 = Style::default().bg(Color::Red);
    /// let combined = style_1.patch(style_2);
    /// assert_eq!(
    ///     Style::default().patch(style_1).patch(style_2),
    ///     Style::default().patch(combined));
    /// ```
    pub fn patch(mut self, other: Style) -> Style {
        self.fg = other.fg.or(self.fg);
        self.bg = other.bg.or(self.bg);
        self.underline_color = other.underline_color.or(self.underline_color);
        self.underline_style = other.underline_style.or(self.underline_style);

        self.add_modifier.remove(other.sub_modifier);
        self.add_modifier.insert(other.add_modifier);
        self.sub_modifier.remove(other.add_modifier);
        self.sub_modifier.insert(other.sub_modifier);

        self
    }
}

// --- Ratatui conversion traits ---

mod ratatui_conv {
    use super::*;

    impl From<Color> for ratatui::style::Color {
        fn from(color: Color) -> Self {
            use ratatui::style::Color as RC;
            match color {
                Color::Reset => RC::Reset,
                Color::Black => RC::Black,
                Color::Red => RC::Red,
                Color::Green => RC::Green,
                Color::Yellow => RC::Yellow,
                Color::Blue => RC::Blue,
                Color::Magenta => RC::Magenta,
                Color::Cyan => RC::Cyan,
                Color::Gray => RC::DarkGray,
                Color::LightRed => RC::LightRed,
                Color::LightGreen => RC::LightGreen,
                Color::LightYellow => RC::LightYellow,
                Color::LightBlue => RC::LightBlue,
                Color::LightMagenta => RC::LightMagenta,
                Color::LightCyan => RC::LightCyan,
                Color::LightGray => RC::Gray,
                Color::White => RC::White,
                Color::Rgb(r, g, b) => RC::Rgb(r, g, b),
                Color::Indexed(i) => RC::Indexed(i),
            }
        }
    }

    impl From<ratatui::style::Color> for Color {
        fn from(color: ratatui::style::Color) -> Self {
            use ratatui::style::Color as RC;
            match color {
                RC::Reset => Color::Reset,
                RC::Black => Color::Black,
                RC::Red => Color::Red,
                RC::Green => Color::Green,
                RC::Yellow => Color::Yellow,
                RC::Blue => Color::Blue,
                RC::Magenta => Color::Magenta,
                RC::Cyan => Color::Cyan,
                RC::DarkGray => Color::Gray,
                RC::LightRed => Color::LightRed,
                RC::LightGreen => Color::LightGreen,
                RC::LightYellow => Color::LightYellow,
                RC::LightBlue => Color::LightBlue,
                RC::LightMagenta => Color::LightMagenta,
                RC::LightCyan => Color::LightCyan,
                RC::Gray => Color::LightGray,
                RC::White => Color::White,
                RC::Rgb(r, g, b) => Color::Rgb(r, g, b),
                RC::Indexed(i) => Color::Indexed(i),
            }
        }
    }

    impl From<Modifier> for ratatui::style::Modifier {
        fn from(m: Modifier) -> Self {
            use ratatui::style::Modifier as RM;
            let mut result = RM::empty();
            if m.contains(Modifier::BOLD) {
                result |= RM::BOLD;
            }
            if m.contains(Modifier::DIM) {
                result |= RM::DIM;
            }
            if m.contains(Modifier::ITALIC) {
                result |= RM::ITALIC;
            }
            if m.contains(Modifier::SLOW_BLINK) {
                result |= RM::SLOW_BLINK;
            }
            if m.contains(Modifier::RAPID_BLINK) {
                result |= RM::RAPID_BLINK;
            }
            if m.contains(Modifier::REVERSED) {
                result |= RM::REVERSED;
            }
            if m.contains(Modifier::HIDDEN) {
                result |= RM::HIDDEN;
            }
            if m.contains(Modifier::CROSSED_OUT) {
                result |= RM::CROSSED_OUT;
            }
            result
        }
    }

    impl From<ratatui::style::Modifier> for Modifier {
        fn from(m: ratatui::style::Modifier) -> Self {
            use ratatui::style::Modifier as RM;
            let mut result = Modifier::empty();
            if m.contains(RM::BOLD) {
                result |= Modifier::BOLD;
            }
            if m.contains(RM::DIM) {
                result |= Modifier::DIM;
            }
            if m.contains(RM::ITALIC) {
                result |= Modifier::ITALIC;
            }
            if m.contains(RM::SLOW_BLINK) {
                result |= Modifier::SLOW_BLINK;
            }
            if m.contains(RM::RAPID_BLINK) {
                result |= Modifier::RAPID_BLINK;
            }
            if m.contains(RM::REVERSED) {
                result |= Modifier::REVERSED;
            }
            if m.contains(RM::HIDDEN) {
                result |= Modifier::HIDDEN;
            }
            if m.contains(RM::CROSSED_OUT) {
                result |= Modifier::CROSSED_OUT;
            }
            result
        }
    }

    impl From<Style> for ratatui::style::Style {
        fn from(s: Style) -> Self {
            let mut rs = ratatui::style::Style::default();
            if let Some(fg) = s.fg {
                rs.fg = Some(fg.into());
            }
            if let Some(bg) = s.bg {
                rs.bg = Some(bg.into());
            }
            if let Some(uc) = s.underline_color {
                rs.underline_color = Some(uc.into());
            }
            // Map underline_style to UNDERLINED modifier
            if let Some(us) = s.underline_style {
                if !matches!(us, UnderlineStyle::Reset) {
                    rs.add_modifier |= ratatui::style::Modifier::UNDERLINED;
                }
            }
            rs.add_modifier |= s.add_modifier.into();
            rs.sub_modifier |= s.sub_modifier.into();
            rs
        }
    }

    impl From<ratatui::style::Style> for Style {
        fn from(s: ratatui::style::Style) -> Self {
            Style {
                fg: s.fg.map(Into::into),
                bg: s.bg.map(Into::into),
                underline_color: s.underline_color.map(Into::into),
                underline_style: if s.add_modifier.contains(ratatui::style::Modifier::UNDERLINED) {
                    Some(UnderlineStyle::Line)
                } else {
                    None
                },
                add_modifier: s.add_modifier.into(),
                sub_modifier: s.sub_modifier.into(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rect_size_preservation() {
        for width in 0..256u16 {
            for height in 0..256u16 {
                let rect = Rect::new(0, 0, width, height);
                rect.area(); // Should not panic.
                assert_eq!(rect.width, width);
                assert_eq!(rect.height, height);
            }
        }

        // One dimension below 255, one above. Area below max u16.
        let rect = Rect::new(0, 0, 300, 100);
        assert_eq!(rect.width, 300);
        assert_eq!(rect.height, 100);
    }

    #[test]
    fn test_rect_chop_from_left() {
        let rect = Rect::new(0, 0, 20, 30);
        assert_eq!(Rect::new(10, 0, 10, 30), rect.clip_left(10));
        assert_eq!(
            Rect::new(20, 0, 0, 30),
            rect.clip_left(40),
            "x should be clamped to original width if new width is bigger"
        );
    }

    #[test]
    fn test_rect_chop_from_right() {
        let rect = Rect::new(0, 0, 20, 30);
        assert_eq!(Rect::new(0, 0, 10, 30), rect.clip_right(10));
    }

    #[test]
    fn test_rect_chop_from_top() {
        let rect = Rect::new(0, 0, 20, 30);
        assert_eq!(Rect::new(0, 10, 20, 20), rect.clip_top(10));
        assert_eq!(
            Rect::new(0, 30, 20, 0),
            rect.clip_top(50),
            "y should be clamped to original height if new height is bigger"
        );
    }

    #[test]
    fn test_rect_chop_from_bottom() {
        let rect = Rect::new(0, 0, 20, 30);
        assert_eq!(Rect::new(0, 0, 20, 20), rect.clip_bottom(10));
    }

    fn styles() -> Vec<Style> {
        vec![
            Style::default(),
            Style::default().fg(Color::Yellow),
            Style::default().bg(Color::Yellow),
            Style::default().add_modifier(Modifier::BOLD),
            Style::default().remove_modifier(Modifier::BOLD),
            Style::default().add_modifier(Modifier::ITALIC),
            Style::default().remove_modifier(Modifier::ITALIC),
            Style::default().add_modifier(Modifier::ITALIC | Modifier::BOLD),
            Style::default().remove_modifier(Modifier::ITALIC | Modifier::BOLD),
        ]
    }

    #[test]
    fn combined_patch_gives_same_result_as_individual_patch() {
        let styles = styles();
        for &a in &styles {
            for &b in &styles {
                for &c in &styles {
                    for &d in &styles {
                        let combined = a.patch(b.patch(c.patch(d)));

                        assert_eq!(
                            Style::default().patch(a).patch(b).patch(c).patch(d),
                            Style::default().patch(combined)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn sanity_nibble_lowercase() {
        for i in 0..0x10_u8 {
            let c = format!("{:x}", i);
            assert_eq!(c.len(), 1);
            assert_eq!(
                u8::from_str_radix(&c, 0x10).unwrap(),
                from_nibble(c.as_bytes()[0])
            );
        }
    }
    #[test]
    fn sanity_nibble_uppercase() {
        for i in 0..0x10_u8 {
            let c = format!("{:X}", i);
            assert_eq!(c.len(), 1);
            assert_eq!(
                u8::from_str_radix(&c, 0x10).unwrap(),
                from_nibble(c.as_bytes()[0])
            );
        }
    }

    #[test]
    fn sanity_nibble2() {
        assert_eq!(dupe_from_nibble(b'0'), Some(0));
        assert_eq!(dupe_from_nibble(b'1'), Some(0x11));
        assert_eq!(dupe_from_nibble(b'7'), Some(0x77));
        assert_eq!(dupe_from_nibble(b'a'), Some(0xaa));
        assert_eq!(dupe_from_nibble(b'f'), Some(0xff));
    }

    #[test]
    fn invalid_nibble() {
        for c in *b"gGzZ+-" {
            assert_eq!(from_nibble(c), 0xff);
        }
    }

    #[test]
    fn pair_endian() {
        assert_eq!(byte_from_hex(*b"00"), Some(0));
        assert_eq!(byte_from_hex(*b"fF"), Some(0xff));
        assert_eq!(byte_from_hex(*b"c3"), Some(0xc3));
    }
    #[test]
    fn invalid_pair() {
        assert!(byte_from_hex(*b"+1").is_none());
        assert!(byte_from_hex(*b"-1").is_none());
        assert!(byte_from_hex(*b"Gg").is_none());
        assert!(byte_from_hex(*b"0x").is_none());
    }

    #[test]
    fn hex_color_no_regress() {
        assert_eq!(Color::from_hex("#+a+b+c"), Err(MalformedHex::NotANibble));
        assert_eq!(Color::from_hex("#+0+1+2"), Err(MalformedHex::NotANibble));
    }
    #[test]
    fn hex_color_sanity() {
        assert_eq!(Color::from_hex("#01fe3a"), Ok(Color::Rgb(0x01, 0xfe, 0x3a)));
        assert_eq!(Color::from_hex("#abc"), Ok(Color::Rgb(0xaa, 0xbb, 0xcc)));
    }
    #[test]
    fn hex_color_invalid_len() {
        for h in [
            "#0",
            "#00",
            "#0000",
            "#00000",
            "#0000000",
            "#00000000",
            "#000000000",
            "#0000000000",
        ] {
            assert_eq!(Color::from_hex(h), Err(MalformedHex::LenOOB));
        }
    }
}
