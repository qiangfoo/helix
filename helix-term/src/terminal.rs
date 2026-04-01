//! Terminal wrapper providing lifecycle management (claim/restore/reconfigure)
//! on top of ratatui's Terminal with crossterm backend.

use crossterm::{
    cursor::{Hide, MoveTo, SetCursorStyle, Show},
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, queue,
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use crate::view::graphics::{CursorKind, Rect};
use once_cell::sync::OnceCell;
use ratatui::buffer::Buffer;
use std::io::{self, Stdout, Write};
use termini::TermInfo;

/// Terminal configuration derived from editor config.
#[derive(Debug)]
pub struct Config {
    pub enable_mouse_capture: bool,
    pub force_enable_extended_underlines: bool,
}

impl From<&crate::view::editor::Config> for Config {
    fn from(config: &crate::view::editor::Config) -> Self {
        Self {
            enable_mouse_capture: config.mouse,
            force_enable_extended_underlines: config.undercurl,
        }
    }
}

fn term_program() -> Option<String> {
    match std::env::var("TERM_PROGRAM") {
        Err(_) => std::env::var("TERM").ok(),
        Ok(term_program) => Some(term_program),
    }
}

fn vte_version() -> Option<usize> {
    std::env::var("VTE_VERSION").ok()?.parse().ok()
}

fn reset_cursor_approach(terminfo: TermInfo) -> String {
    let mut reset_str = String::new();
    if let Some(termini::Value::Utf8String(se_str)) = terminfo.extended_cap("Se") {
        reset_str.push_str(se_str);
    }
    reset_str.push_str(
        terminfo
            .utf8_string_cap(termini::StringCapability::CursorNormal)
            .unwrap_or(""),
    );
    reset_str.push_str("\x1B[0 q");
    reset_str
}

#[derive(Clone, Debug)]
struct Capabilities {
    _has_extended_underlines: bool,
    reset_cursor_command: String,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            _has_extended_underlines: false,
            reset_cursor_command: "\x1B[0 q".to_string(),
        }
    }
}

impl Capabilities {
    pub fn from_env_or_default(config: &Config) -> Self {
        match TermInfo::from_env() {
            Err(_) => Capabilities {
                _has_extended_underlines: config.force_enable_extended_underlines,
                ..Capabilities::default()
            },
            Ok(t) => Capabilities {
                _has_extended_underlines: config.force_enable_extended_underlines
                    || t.extended_cap("Smulx").is_some()
                    || t.extended_cap("Su").is_some()
                    || vte_version() >= Some(5102)
                    || matches!(term_program().as_deref(), Some("WezTerm")),
                reset_cursor_command: reset_cursor_approach(t),
            },
        }
    }
}

/// Helix terminal wrapper around ratatui's Terminal with crossterm backend.
pub struct HelixTerminal {
    terminal: ratatui::Terminal<ratatui::backend::CrosstermBackend<Stdout>>,
    config: Config,
    capabilities: Capabilities,
    supports_keyboard_enhancement: OnceCell<bool>,
    mouse_capture_enabled: bool,
    supports_bracketed_paste: bool,
}

impl HelixTerminal {
    pub fn new(config: Config) -> io::Result<Self> {
        crossterm::style::force_color_output(true);
        let capabilities = Capabilities::from_env_or_default(&config);
        let backend = ratatui::backend::CrosstermBackend::new(io::stdout());
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self {
            terminal,
            capabilities,
            config,
            supports_keyboard_enhancement: OnceCell::new(),
            mouse_capture_enabled: false,
            supports_bracketed_paste: true,
        })
    }

    fn supports_keyboard_enhancement_protocol(&self) -> bool {
        *self
            .supports_keyboard_enhancement
            .get_or_init(|| {
                use std::time::Instant;
                let now = Instant::now();
                let supported = matches!(terminal::supports_keyboard_enhancement(), Ok(true));
                log::debug!(
                    "The keyboard enhancement protocol is {}supported in this terminal (checked in {:?})",
                    if supported { "" } else { "not " },
                    Instant::now().duration_since(now)
                );
                supported
            })
    }

    /// Enter raw mode, alternate screen, enable mouse capture, etc.
    pub fn claim(&mut self) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableFocusChange
        )?;
        match execute!(io::stdout(), EnableBracketedPaste) {
            Err(err) if err.kind() == io::ErrorKind::Unsupported => {
                log::warn!("Bracketed paste is not supported on this terminal.");
                self.supports_bracketed_paste = false;
            }
            Err(err) => return Err(err),
            Ok(_) => (),
        }
        execute!(io::stdout(), Clear(ClearType::All))?;
        if self.config.enable_mouse_capture {
            execute!(io::stdout(), EnableMouseCapture)?;
            self.mouse_capture_enabled = true;
        }
        if self.supports_keyboard_enhancement_protocol() {
            execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            )?;
        }
        Ok(())
    }

    /// Leave alternate screen, disable raw mode, etc.
    pub fn restore(&mut self) -> io::Result<()> {
        io::stdout()
            .write_all(self.capabilities.reset_cursor_command.as_bytes())?;
        if self.config.enable_mouse_capture {
            execute!(io::stdout(), DisableMouseCapture)?;
        }
        if self.supports_keyboard_enhancement_protocol() {
            execute!(io::stdout(), PopKeyboardEnhancementFlags)?;
        }
        if self.supports_bracketed_paste {
            execute!(io::stdout(), DisableBracketedPaste)?;
        }
        execute!(
            io::stdout(),
            DisableFocusChange,
            LeaveAlternateScreen
        )?;
        terminal::disable_raw_mode()
    }

    /// Reconfigure terminal (toggle mouse capture, etc.)
    pub fn reconfigure(&mut self, config: Config) -> io::Result<()> {
        if self.mouse_capture_enabled != config.enable_mouse_capture {
            if config.enable_mouse_capture {
                execute!(io::stdout(), EnableMouseCapture)?;
            } else {
                execute!(io::stdout(), DisableMouseCapture)?;
            }
            self.mouse_capture_enabled = config.enable_mouse_capture;
        }
        self.config = config;
        Ok(())
    }

    /// Whether the terminal supports true color.
    pub fn supports_true_color(&self) -> bool {
        crate::true_color()
    }

    /// Get theme mode (dark/light). Not available with crossterm backend.
    pub fn get_theme_mode(&self) -> Option<crate::view::theme::Mode> {
        None
    }

    /// Set the terminal background color via crossterm.
    pub fn set_background_color(&mut self, color: Option<crate::view::graphics::Color>) -> io::Result<()> {
        use crossterm::style::{SetBackgroundColor, Color as CColor};
        if let Some(color) = color {
            let rcolor: ratatui::style::Color = color.into();
            let ccolor: CColor = rcolor.into();
            execute!(io::stdout(), SetBackgroundColor(ccolor))
        } else {
            execute!(io::stdout(), SetBackgroundColor(CColor::Reset))
        }
    }

    // --- Delegate to ratatui Terminal ---

    pub fn clear(&mut self) -> io::Result<()> {
        self.terminal.clear()?;
        Ok(())
    }

    pub fn size(&self) -> Rect {
        let size = self.terminal.size().unwrap_or_default();
        Rect::new(0, 0, size.width, size.height)
    }

    pub fn autoresize(&mut self) -> io::Result<Rect> {
        self.terminal.autoresize()?;
        Ok(self.size())
    }

    pub fn resize(&mut self, area: Rect) -> io::Result<()> {
        self.terminal.resize(area)
    }

    pub fn current_buffer_mut(&mut self) -> &mut Buffer {
        self.terminal.current_buffer_mut()
    }

    /// Flush the diff between previous and current buffer, set cursor, swap buffers.
    pub fn draw(
        &mut self,
        cursor_pos: Option<(u16, u16)>,
        cursor_kind: CursorKind,
    ) -> io::Result<()> {
        // Flush buffer diff to backend
        self.terminal.flush()?;

        // Handle cursor
        let mut stdout = io::stdout();
        match cursor_kind {
            CursorKind::Hidden => {
                queue!(stdout, Hide)?;
            }
            kind => {
                if let Some((col, row)) = cursor_pos {
                    queue!(stdout, Show, MoveTo(col, row))?;
                }
                match kind {
                    CursorKind::Block => queue!(stdout, SetCursorStyle::SteadyBlock)?,
                    CursorKind::Bar => queue!(stdout, SetCursorStyle::SteadyBar)?,
                    CursorKind::Underline => queue!(stdout, SetCursorStyle::SteadyUnderScore)?,
                    CursorKind::Hidden => unreachable!(),
                }
            }
        }
        stdout.flush()?;

        // Swap buffers for next frame
        self.terminal.swap_buffers();

        Ok(())
    }
}

/// Test terminal using ratatui's TestBackend.
#[cfg(feature = "integration")]
pub struct TestTerminal {
    terminal: ratatui::Terminal<ratatui::backend::TestBackend>,
}

#[cfg(feature = "integration")]
impl TestTerminal {
    pub fn new(width: u16, height: u16) -> io::Result<Self> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let terminal = ratatui::Terminal::new(backend)?;
        Ok(Self { terminal })
    }

    pub fn claim(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub fn restore(&mut self) -> io::Result<()> {
        Ok(())
    }

    pub fn reconfigure(&mut self, _config: Config) -> io::Result<()> {
        Ok(())
    }

    pub fn supports_true_color(&self) -> bool {
        true
    }

    pub fn get_theme_mode(&self) -> Option<crate::view::theme::Mode> {
        None
    }

    pub fn set_background_color(&mut self, _color: Option<crate::view::graphics::Color>) -> io::Result<()> {
        Ok(())
    }

    pub fn clear(&mut self) -> io::Result<()> {
        self.terminal.clear()?;
        Ok(())
    }

    pub fn size(&self) -> Rect {
        let size = self.terminal.size().unwrap_or_default();
        Rect::new(0, 0, size.width, size.height)
    }

    pub fn autoresize(&mut self) -> io::Result<Rect> {
        self.terminal.autoresize()?;
        Ok(self.size())
    }

    pub fn resize(&mut self, area: Rect) -> io::Result<()> {
        self.terminal.resize(area)
    }

    pub fn current_buffer_mut(&mut self) -> &mut Buffer {
        self.terminal.current_buffer_mut()
    }

    pub fn draw(
        &mut self,
        _cursor_pos: Option<(u16, u16)>,
        _cursor_kind: CursorKind,
    ) -> io::Result<()> {
        self.terminal.flush()?;
        self.terminal.swap_buffers();
        Ok(())
    }
}
